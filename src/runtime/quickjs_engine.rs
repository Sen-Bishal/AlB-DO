use super::engine::{
    stable_source_hash, BootstrapPayload, LoadErrorKind, RenderOutput, RuntimeEngine, RuntimeError,
    RuntimeResult,
};
use rquickjs::{Context, Function, Runtime};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;
use swc_common::{
    comments::SingleThreadedComments, sync::Lrc, FileName, Globals, Mark, SourceMap, Span, Spanned,
    GLOBALS,
};
use swc_ecma_ast::{
    Decl, ExportSpecifier, ImportSpecifier, Module, ModuleDecl, ModuleExportName, ModuleItem, Pat,
};
use swc_ecma_codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;

const MAX_MODULE_SIZE: usize = 10 * 1024 * 1024; // 10 MB limit
use swc_ecma_transforms_react::{jsx, Options as JsxOptions, Runtime as JsxRuntime};
use swc_ecma_transforms_typescript::strip_type;
use swc_ecma_visit::VisitMutWith;

const MODULE_RECORD_FLAG: &str = "__albedo_is_module_record";
const MODULE_MISSING_MARKER: &str = "__ALBEDO_MODULE_MISSING__:";
const INVALID_ENTRY_EXPORT_MARKER: &str = "__ALBEDO_INVALID_ENTRY_EXPORT__:";

#[derive(Debug, Deserialize)]
struct RenderEnvelope {
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

pub struct QuickJsEngine {
    _runtime: Runtime,
    context: Context,
    loaded_module_hashes: HashMap<String, u64>,
    initialized: bool,
}

impl QuickJsEngine {
    pub fn new() -> RuntimeResult<Self> {
        let runtime = Runtime::new().map_err(|err| {
            RuntimeError::init(format!("failed to create QuickJS runtime: {err}"))
        })?;
        let context = Context::full(&runtime).map_err(|err| {
            RuntimeError::init(format!("failed to create QuickJS context: {err}"))
        })?;

        Ok(Self {
            _runtime: runtime,
            context,
            loaded_module_hashes: HashMap::new(),
            initialized: false,
        })
    }
}

impl RuntimeEngine for QuickJsEngine {
    fn init(&mut self, bootstrap: &BootstrapPayload) -> RuntimeResult<()> {
        self.context.with(|ctx| -> RuntimeResult<()> {
            ctx.eval::<(), _>(build_builtin_runtime_helpers_script())
                .map_err(|err| {
                    RuntimeError::init(format!("failed to install built-in runtime helpers: {err}"))
                })?;

            if !bootstrap.dom_shim_js.trim().is_empty() {
                ctx.eval::<(), _>(bootstrap.dom_shim_js.as_str())
                    .map_err(|err| {
                        RuntimeError::init(format!("failed to evaluate DOM shim: {err}"))
                    })?;
            }

            if !bootstrap.runtime_helpers_js.trim().is_empty() {
                ctx.eval::<(), _>(bootstrap.runtime_helpers_js.as_str())
                    .map_err(|err| {
                        RuntimeError::init(format!("failed to evaluate runtime helpers: {err}"))
                    })?;
            }

            ctx.eval::<(), _>("globalThis.__ALBEDO_MODULES = Object.create(null);")
                .map_err(|err| {
                    RuntimeError::init(format!("failed to initialize module table: {err}"))
                })?;

            let render_script = build_render_function_script();
            ctx.eval::<(), _>(render_script.as_str()).map_err(|err| {
                RuntimeError::init(format!("failed to install reusable render function: {err}"))
            })?;

            Ok(())
        })?;

        self.loaded_module_hashes.clear();
        for preload in &bootstrap.preloaded_libraries {
            self.load_module(&preload.specifier, &preload.code)?;
        }

        self.initialized = true;
        Ok(())
    }

    fn load_module(&mut self, specifier: &str, code: &str) -> RuntimeResult<()> {
        if code.len() > MAX_MODULE_SIZE {
            return Err(RuntimeError::load(
                LoadErrorKind::EngineFailure,
                format!(
                    "Module '{specifier}' exceeds maximum size limit of {} bytes",
                    MAX_MODULE_SIZE
                ),
            ));
        }

        let code_hash = stable_source_hash(code);
        if self.loaded_module_hashes.get(specifier).copied() == Some(code_hash) {
            return Ok(());
        }

        let script = compile_module_script_for_quickjs(specifier, code)?;

        self.context.with(|ctx| {
            ctx.eval::<(), _>(script.as_str()).map_err(|err| {
                RuntimeError::load(
                    LoadErrorKind::EngineFailure,
                    format!("failed to load module '{specifier}': {err}"),
                )
            })
        })?;

        self.loaded_module_hashes
            .insert(specifier.to_string(), code_hash);
        Ok(())
    }

    fn load_precompiled_module(
        &mut self,
        specifier: &str,
        compiled_script: &str,
        source_hash: u64,
    ) -> RuntimeResult<()> {
        if self.loaded_module_hashes.get(specifier).copied() == Some(source_hash) {
            return Ok(());
        }

        self.context.with(|ctx| {
            ctx.eval::<(), _>(compiled_script).map_err(|err| {
                RuntimeError::load(
                    LoadErrorKind::EngineFailure,
                    format!("failed to load precompiled module '{specifier}': {err}"),
                )
            })
        })?;

        self.loaded_module_hashes
            .insert(specifier.to_string(), source_hash);
        Ok(())
    }

    fn render_component(&mut self, entry: &str, props_json: &str) -> RuntimeResult<RenderOutput> {
        if !self.initialized {
            return Err(RuntimeError::init("runtime not initialized"));
        }

        let eval_start = Instant::now();
        let envelope_json = self.context.with(|ctx| {
            let globals = ctx.globals();
            let render_fn: Function = globals.get("__ALBEDO_RENDER_COMPONENT").map_err(|err| {
                RuntimeError::render(format!(
                    "reusable render function missing for component '{entry}': {err}"
                ))
            })?;

            render_fn
                .call::<(String, String), String>((entry.to_string(), props_json.to_string()))
                .map_err(|err| {
                    RuntimeError::render(format!(
                        "failed to execute reusable render function for component '{entry}': {err}"
                    ))
                })
        })?;
        let eval_ms = eval_start.elapsed().as_millis();

        let envelope: RenderEnvelope = serde_json::from_str(&envelope_json).map_err(|err| {
            RuntimeError::render(format!(
                "failed to decode render result envelope for '{entry}': {err}"
            ))
        })?;

        if envelope.ok {
            let html = envelope.value.ok_or_else(|| {
                RuntimeError::render(format!(
                    "render script for '{entry}' returned success without value"
                ))
            })?;
            Ok(RenderOutput { html, eval_ms })
        } else {
            let message = envelope
                .error
                .unwrap_or_else(|| "unknown runtime error".to_string());
            Err(map_render_error(entry, &message))
        }
    }

    fn warm(&mut self) -> RuntimeResult<()> {
        self.context.with(|ctx| {
            ctx.eval::<i32, _>("40 + 2")
                .map(|_| ())
                .map_err(|err| RuntimeError::init(format!("runtime warm-up failed: {err}")))
        })
    }
}

fn build_render_function_script() -> String {
    format!(
        r#"
globalThis.__ALBEDO_RENDER_COMPONENT = function(entry, propsJson) {{
  try {{
    const __albedo_record = globalThis.__ALBEDO_MODULES[entry];
    const __albedo_has_own = Object.prototype.hasOwnProperty;
    const __albedo_is_record = function(candidate) {{
      return candidate !== null
        && typeof candidate === 'object'
        && candidate.{MODULE_RECORD_FLAG} === true;
    }};
    if (typeof __albedo_record === 'undefined') {{
      throw new Error('{MODULE_MISSING_MARKER}' + entry);
    }}
    let __albedo_component = __albedo_record;
    if (__albedo_is_record(__albedo_record)) {{
      if (!__albedo_has_own.call(__albedo_record, 'default')) {{
        throw new Error('{INVALID_ENTRY_EXPORT_MARKER}' + entry);
      }}
      __albedo_component = __albedo_record.default;
    }}
    if (typeof __albedo_component === 'undefined') {{
      throw new Error('{INVALID_ENTRY_EXPORT_MARKER}' + entry);
    }}
    const __albedo_props = JSON.parse(propsJson);
    const __albedo_require = function(specifier) {{
      const resolved = globalThis.__ALBEDO_MODULES[specifier];
      if (typeof resolved === 'undefined') {{
        throw new Error('{MODULE_MISSING_MARKER}' + specifier);
      }}
      if (__albedo_is_record(resolved)) {{
        if (__albedo_has_own.call(resolved, 'default')) {{
          const defaultExport = resolved.default;
          if (typeof defaultExport === 'function') {{
            return function(props) {{ return defaultExport(props, __albedo_require); }};
          }}
          return defaultExport;
        }}
        return resolved;
      }}
      if (typeof resolved === 'function') {{
        return function(props) {{ return resolved(props, __albedo_require); }};
      }}
      return resolved;
    }};
    const __albedo_value = (typeof __albedo_component === 'function')
      ? __albedo_component(__albedo_props, __albedo_require)
      : __albedo_component;
    return JSON.stringify({{ ok: true, value: String(__albedo_value) }});
  }} catch (err) {{
    const message = (err && typeof err.message === 'string') ? err.message : String(err);
    return JSON.stringify({{ ok: false, error: message }});
  }}
}};
"#
    )
}

fn build_builtin_runtime_helpers_script() -> &'static str {
    r#"
if (typeof globalThis.h !== 'function') {
  const __albedo_escape_html = function(str) {
    return String(str).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#x27;');
  };

  const __albedo_push_children = function(value, out) {
    if (Array.isArray(value)) {
      for (const item of value) {
        __albedo_push_children(item, out);
      }
      return;
    }
    if (value === null || typeof value === 'undefined' || value === false) {
      return;
    }
    out.push(String(value));
  };

  const h = function(type, props, ...children) {
    const flatChildren = [];
    __albedo_push_children(children, flatChildren);

    if (typeof type === 'function') {
      const mergedProps = Object.assign({}, props || {});
      if (flatChildren.length === 1) {
        mergedProps.children = flatChildren[0];
      } else if (flatChildren.length > 1) {
        mergedProps.children = flatChildren;
      }
      return type(mergedProps);
    }

    let attrs = '';
    const safeProps = props || {};
    for (const key in safeProps) {
      if (!Object.prototype.hasOwnProperty.call(safeProps, key) || key === 'children') {
        continue;
      }
      const value = safeProps[key];
      if (value === false || value === null || typeof value === 'undefined') {
        continue;
      }
      if (value === true) {
        attrs += ' ' + key;
        continue;
      }
      attrs += ' ' + key + '="' + __albedo_escape_html(value) + '"';
    }

    const inner = flatChildren.join('');
    return '<' + String(type) + attrs + '>' + inner + '</' + String(type) + '>';
  };

  h.Fragment = function Fragment(fragmentProps) {
    if (!fragmentProps || typeof fragmentProps.children === 'undefined') {
      return '';
    }
    const out = [];
    __albedo_push_children(fragmentProps.children, out);
    return out.join('');
  };

  globalThis.h = h;
}
"#
}

pub(crate) fn compile_module_script_for_quickjs(
    specifier: &str,
    code: &str,
) -> RuntimeResult<String> {
    let normalized = code.trim();
    if normalized.is_empty() {
        return Err(RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("module '{specifier}' is empty"),
        ));
    }

    let transpiled = transpile_module_source_for_quickjs(specifier, normalized)?;

    if !transpiled.contains("export") && !transpiled.contains("import") {
        return compile_legacy_expression_module(specifier, transpiled.as_str());
    }

    compile_exporting_module(specifier, transpiled.as_str())
}

fn compile_legacy_expression_module(
    specifier: &str,
    expression_source: &str,
) -> RuntimeResult<String> {
    let expression = expression_source.trim().trim_end_matches(';');
    let statements = vec![format!("const __albedo_default_export__ = ({expression});")];
    let exports = vec!["__albedo_exports.default = __albedo_default_export__;".to_string()];
    build_module_record_script(specifier, &statements, &exports)
}

fn compile_exporting_module(specifier: &str, source: &str) -> RuntimeResult<String> {
    let module = parse_module(specifier, source)?;
    let mut statements = Vec::new();
    let mut export_assignments = Vec::new();

    for item in module.body {
        match item {
            ModuleItem::Stmt(stmt) => {
                let snippet = normalize_statement(slice_source(source, stmt.span(), specifier)?);
                if !snippet.is_empty() {
                    statements.push(snippet);
                }
            }
            ModuleItem::ModuleDecl(decl) => match decl {
                ModuleDecl::ExportDefaultExpr(default_expr) => {
                    let expr_source = slice_source(source, default_expr.expr.span(), specifier)?;
                    statements.push(format!(
                        "const __albedo_default_export__ = ({expr_source});"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                }
                ModuleDecl::ExportDefaultDecl(default_decl) => {
                    let decl_source = slice_source(source, default_decl.span(), specifier)?;
                    let default_value =
                        strip_export_default_prefix(&decl_source).ok_or_else(|| {
                            RuntimeError::load(
                                LoadErrorKind::UnsupportedSyntax,
                                format!(
                                "unsupported default export declaration in module '{specifier}'"
                            ),
                            )
                        })?;
                    statements.push(format!(
                        "const __albedo_default_export__ = {default_value};"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                }
                ModuleDecl::ExportDecl(export_decl) => match export_decl.decl {
                    Decl::Fn(fn_decl) => {
                        let decl_source = normalize_statement(slice_source(
                            source,
                            fn_decl.function.span,
                            specifier,
                        )?);
                        if !decl_source.is_empty() {
                            statements.push(decl_source);
                        }
                        let export_name = fn_decl.ident.sym.to_string();
                        let export_key = js_string_literal(&export_name, specifier)?;
                        export_assignments
                            .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                    }
                    Decl::Var(var_decl) => {
                        let decl_source =
                            normalize_statement(slice_source(source, var_decl.span, specifier)?);
                        if !decl_source.is_empty() {
                            statements.push(decl_source);
                        }

                        for decl in var_decl.decls {
                            let export_name = match decl.name {
                                Pat::Ident(binding_ident) => binding_ident.id.sym.to_string(),
                                _ => {
                                    return Err(RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "unsupported export pattern in module '{specifier}'; only identifier bindings are supported"
                                        ),
                                    ));
                                }
                            };
                            let export_key = js_string_literal(&export_name, specifier)?;
                            export_assignments
                                .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                        }
                    }
                    other => {
                        return Err(RuntimeError::load(
                            LoadErrorKind::UnsupportedSyntax,
                            format!(
                                "unsupported export declaration '{:?}' in module '{specifier}'",
                                other
                            ),
                        ));
                    }
                },
                ModuleDecl::ExportNamed(named_export) => {
                    if named_export.src.is_some() {
                        return Err(RuntimeError::load(
                            LoadErrorKind::UnsupportedSyntax,
                            format!(
                                "re-export from external source is not supported in module '{specifier}'"
                            ),
                        ));
                    }

                    for named_specifier in named_export.specifiers {
                        match named_specifier {
                            ExportSpecifier::Named(named) => {
                                let local = module_export_name_to_ident(&named.orig).ok_or_else(|| {
                                    RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "unsupported named export source in module '{specifier}'"
                                        ),
                                    )
                                })?;
                                let exported = named
                                    .exported
                                    .as_ref()
                                    .and_then(module_export_name_to_ident)
                                    .unwrap_or_else(|| local.clone());

                                let export_key = js_string_literal(&exported, specifier)?;
                                export_assignments
                                    .push(format!("__albedo_exports[{export_key}] = {local};"));
                            }
                            ExportSpecifier::Default(default_export) => {
                                let local = default_export.exported.sym.to_string();
                                export_assignments
                                    .push(format!("__albedo_exports.default = {local};"));
                            }
                            ExportSpecifier::Namespace(_) => {
                                return Err(RuntimeError::load(
                                    LoadErrorKind::UnsupportedSyntax,
                                    format!(
                                        "namespace exports are not supported in module '{specifier}'"
                                    ),
                                ));
                            }
                        }
                    }
                }
                ModuleDecl::Import(import_decl) => {
                    let rewritten = rewrite_import_declaration(import_decl, specifier)?;
                    statements.extend(rewritten);
                }
                unsupported => {
                    return Err(RuntimeError::load(
                        LoadErrorKind::UnsupportedSyntax,
                        format!(
                            "unsupported module declaration '{:?}' in module '{specifier}'",
                            unsupported
                        ),
                    ));
                }
            },
        }
    }

    build_module_record_script(specifier, &statements, &export_assignments)
}

fn transpile_module_source_for_quickjs(specifier: &str, source: &str) -> RuntimeResult<String> {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let preferred_syntax = syntax_for_specifier(specifier);
        let (mut module, source_map) =
            parse_module_with_fallback(specifier, source, preferred_syntax)?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, false));
        module.visit_mut_with(&mut strip_type());

        let mut jsx_options = JsxOptions::default();
        jsx_options.runtime = Some(JsxRuntime::Classic);
        jsx_options.pragma = Some("h".to_string());
        jsx_options.pragma_frag = Some("h.Fragment".to_string());
        jsx_options.development = Some(false);
        module.visit_mut_with(&mut jsx(
            source_map.clone(),
            None::<SingleThreadedComments>,
            jsx_options,
            top_level_mark,
            unresolved_mark,
        ));

        emit_module_source(specifier, &module, source_map)
    })
}

fn syntax_for_specifier(specifier: &str) -> Syntax {
    match Path::new(specifier)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("ts") => Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: true,
            ..Default::default()
        }),
        Some("tsx") => Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..Default::default()
        }),
        _ => Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }),
    }
}

fn parse_module_with_fallback(
    specifier: &str,
    source: &str,
    preferred_syntax: Syntax,
) -> RuntimeResult<(Module, Lrc<SourceMap>)> {
    let should_try_ts_fallback =
        matches!(preferred_syntax, Syntax::Es(_)) && Path::new(specifier).extension().is_none();

    match parse_module_with_syntax(specifier, source, preferred_syntax) {
        Ok(module) => Ok(module),
        Err(primary_error) => {
            if !should_try_ts_fallback {
                return Err(primary_error);
            }

            parse_module_with_syntax(
                specifier,
                source,
                Syntax::Typescript(TsSyntax {
                    tsx: true,
                    decorators: true,
                    ..Default::default()
                }),
            )
            .map_err(|_| primary_error)
        }
    }
}

fn parse_module_with_syntax(
    specifier: &str,
    source: &str,
    syntax: Syntax,
) -> RuntimeResult<(Module, Lrc<SourceMap>)> {
    let source_map: Lrc<SourceMap> = Default::default();
    let source_file = source_map.new_source_file(
        FileName::Custom(format!("quickjs:{specifier}")).into(),
        source.to_string(),
    );

    let mut parser = Parser::new(syntax, StringInput::from(&*source_file), None);
    parser
        .parse_module()
        .map(|module| (module, source_map))
        .map_err(|err| {
            RuntimeError::load(
                LoadErrorKind::UnsupportedSyntax,
                format!("failed to parse module '{specifier}': {:?}", err),
            )
        })
}

fn parse_module(specifier: &str, source: &str) -> RuntimeResult<Module> {
    let source_map: Rc<SourceMap> = Rc::new(SourceMap::default());
    let source_file = source_map.new_source_file(
        FileName::Custom(format!("quickjs:{specifier}")).into(),
        source.to_string(),
    );

    let mut parser = Parser::new(
        Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }),
        StringInput::from(&*source_file),
        None,
    );

    parser.parse_module().map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::UnsupportedSyntax,
            format!("failed to parse module '{specifier}': {:?}", err),
        )
    })
}

fn emit_module_source(
    specifier: &str,
    module: &Module,
    source_map: Lrc<SourceMap>,
) -> RuntimeResult<String> {
    let mut output = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            comments: None,
            cm: source_map.clone(),
            wr: JsWriter::new(source_map, "\n", &mut output, None),
        };
        emitter.emit_module(module).map_err(|err| {
            RuntimeError::load(
                LoadErrorKind::EngineFailure,
                format!("failed to emit transpiled module '{specifier}': {err}"),
            )
        })?;
    }
    String::from_utf8(output).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("failed to decode transpiled module '{specifier}' as UTF-8: {err}"),
        )
    })
}

fn build_module_record_script(
    specifier: &str,
    statements: &[String],
    export_assignments: &[String],
) -> RuntimeResult<String> {
    let escaped_specifier = js_string_literal(specifier, specifier)?;

    let mut script = String::new();
    script.push_str("(function() {\n");
    script.push_str("  const __albedo_exports = Object.create(null);\n");
    script.push_str(&format!(
        "  Object.defineProperty(__albedo_exports, \"{MODULE_RECORD_FLAG}\", {{ value: true, enumerable: false }});\n"
    ));

    for statement in statements {
        if statement.trim().is_empty() {
            continue;
        }
        script.push_str("  ");
        script.push_str(statement);
        if !statement.ends_with('\n') {
            script.push('\n');
        }
    }

    for export in export_assignments {
        script.push_str("  ");
        script.push_str(export);
        if !export.ends_with('\n') {
            script.push('\n');
        }
    }

    script.push_str(&format!(
        "  globalThis.__ALBEDO_MODULES[{escaped_specifier}] = __albedo_exports;\n"
    ));
    script.push_str("})();");
    Ok(script)
}

fn js_string_literal(value: &str, specifier: &str) -> RuntimeResult<String> {
    serde_json::to_string(value).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "failed to serialize JavaScript string literal for module '{specifier}': {err}"
            ),
        )
    })
}

fn slice_source(source: &str, span: Span, specifier: &str) -> RuntimeResult<String> {
    let start = span.lo.0.saturating_sub(1) as usize;
    let end = span.hi.0.saturating_sub(1) as usize;

    if end < start {
        return Err(RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "invalid span while transforming module '{specifier}' (start={start}, end={end})"
            ),
        ));
    }

    source.get(start..end).map(|slice| slice.to_string()).ok_or_else(|| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "span out of bounds while transforming module '{specifier}' (start={start}, end={end}, len={})",
                source.len()
            ),
        )
    })
}

fn normalize_statement(source: String) -> String {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.ends_with(';') || trimmed.ends_with('}') {
        trimmed.to_string()
    } else {
        format!("{trimmed};")
    }
}

fn strip_export_default_prefix(source: &str) -> Option<String> {
    let trimmed = source.trim_start();
    trimmed
        .strip_prefix("export default")
        .map(|rest| rest.trim().trim_end_matches(';').to_string())
}

fn rewrite_import_declaration(
    import_decl: swc_ecma_ast::ImportDecl,
    specifier: &str,
) -> RuntimeResult<Vec<String>> {
    let import_source = import_decl.src.value.to_string();
    let import_source_literal = js_string_literal(import_source.as_str(), specifier)?;
    let require_call = format!("__albedo_require({import_source_literal})");

    if import_decl.specifiers.is_empty() {
        return Ok(vec![format!("{require_call};")]);
    }

    let mut statements = Vec::new();
    let mut named_bindings = Vec::new();
    let import_binding = format!(
        "__albedo_import_{}_{}",
        import_decl.span.lo.0, import_decl.span.hi.0
    );
    statements.push(format!("const {import_binding} = {require_call};"));

    for import_specifier in import_decl.specifiers {
        match import_specifier {
            ImportSpecifier::Default(default_specifier) => {
                let local = default_specifier.local.sym.to_string();
                statements.push(format!("const {local} = {import_binding};"));
            }
            ImportSpecifier::Namespace(namespace_specifier) => {
                let local = namespace_specifier.local.sym.to_string();
                statements.push(format!("const {local} = {import_binding};"));
            }
            ImportSpecifier::Named(named_specifier) => {
                let local = named_specifier.local.sym.to_string();
                let binding = match named_specifier.imported.as_ref() {
                    None => local.clone(),
                    Some(ModuleExportName::Ident(imported_ident))
                        if imported_ident.sym == named_specifier.local.sym =>
                    {
                        local.clone()
                    }
                    Some(imported_name) => {
                        let property = module_export_name_to_property(imported_name, specifier)?;
                        format!("{property}: {local}")
                    }
                };
                named_bindings.push(binding);
            }
        }
    }

    if !named_bindings.is_empty() {
        statements.push(format!(
            "const {{ {} }} = {import_binding};",
            named_bindings.join(", ")
        ));
    }

    Ok(statements)
}

fn module_export_name_to_property(
    name: &ModuleExportName,
    specifier: &str,
) -> RuntimeResult<String> {
    match name {
        ModuleExportName::Ident(ident) => Ok(ident.sym.to_string()),
        ModuleExportName::Str(string_literal) => {
            let value = string_literal.value.to_string();
            js_string_literal(value.as_str(), specifier)
        }
    }
}

fn module_export_name_to_ident(name: &ModuleExportName) -> Option<String> {
    match name {
        ModuleExportName::Ident(ident) => Some(ident.sym.to_string()),
        ModuleExportName::Str(_) => None,
    }
}

fn map_render_error(entry: &str, message: &str) -> RuntimeError {
    if let Some(specifier) = extract_marker_payload(message, MODULE_MISSING_MARKER) {
        return RuntimeError::load(
            LoadErrorKind::ModuleMissing,
            format!("module missing during render: '{specifier}'"),
        );
    }

    if let Some(entry_module) = extract_marker_payload(message, INVALID_ENTRY_EXPORT_MARKER) {
        return RuntimeError::load(
            LoadErrorKind::InvalidEntryExport,
            format!("invalid entry export for '{entry_module}': expected a default export"),
        );
    }

    RuntimeError::render(format!("failed to render component '{entry}': {message}"))
}

fn extract_marker_payload(message: &str, marker: &str) -> Option<String> {
    let index = message.find(marker)?;
    let tail = &message[(index + marker.len())..];

    let mut payload = String::new();
    for ch in tail.chars() {
        if ch.is_whitespace() || matches!(ch, '\n' | '\r' | '\'' | '"' | ')' | ']' | '}') {
            break;
        }
        payload.push(ch);
    }

    let value = payload.trim_matches(':').trim_matches(',').to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::compile_module_script_for_quickjs;

    #[test]
    fn test_compile_module_rewrites_import_declarations_to_runtime_requires() {
        let source = r#"
            import DefaultThing from "pkg/default";
            import { a, b as c } from "pkg/named";
            import * as ns from "pkg/ns";
            import "pkg/side-effect";

            export default function App() {
                return String(DefaultThing) + String(a) + String(c) + String(ns);
            }
        "#;

        let compiled = compile_module_script_for_quickjs("components/App.jsx", source).unwrap();
        assert!(compiled.contains(r#"__albedo_require("pkg/default")"#));
        assert!(compiled.contains("const DefaultThing = __albedo_import_"));
        assert!(compiled.contains(r#"__albedo_require("pkg/named")"#));
        assert!(compiled.contains("const { a, b: c } = __albedo_import_"));
        assert!(compiled.contains(r#"__albedo_require("pkg/ns")"#));
        assert!(compiled.contains("const ns = __albedo_import_"));
        assert!(compiled.contains(r#"__albedo_require("pkg/side-effect");"#));
    }

    #[test]
    fn test_compile_module_transpiles_jsx_and_strips_typescript() {
        let source = r#"
            export default function App(props: { name: string }) {
                const title: string = props.name as string;
                return <main>{title}</main>;
            }
        "#;

        let compiled = compile_module_script_for_quickjs("components/App.tsx", source).unwrap();
        assert!(compiled.contains("h("));
        assert!(!compiled.contains("<main>"));
        assert!(!compiled.contains(": string"));
        assert!(!compiled.contains(" as string"));
    }
}
