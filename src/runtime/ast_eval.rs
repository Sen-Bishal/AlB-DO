use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
struct ImportBinding {
    source: String,
    export_name: String,
}

#[derive(Debug, Clone)]
enum ParamBinding {
    Ident(String),
    Object(Vec<(String, String)>),
    Ignore,
}

#[derive(Debug, Clone)]
struct ComponentFunction {
    params: Vec<ParamBinding>,
    body_expr: Expr,
}

#[derive(Debug, Clone)]
struct ParsedModule {
    imports: HashMap<String, ImportBinding>,
    functions: HashMap<String, ComponentFunction>,
    default_export: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ComponentProject {
    root: PathBuf,
    modules: HashMap<String, ParsedModule>,
}

impl ComponentProject {
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut modules = HashMap::new();

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if !matches!(ext, "jsx" | "tsx" | "js" | "ts") {
                continue;
            }

            let relative = path
                .strip_prefix(&root)
                .map_err(|err| anyhow!("failed to compute module path: {err}"))?;
            let specifier = normalize_specifier(relative);
            let source = std::fs::read_to_string(path)
                .map_err(|err| anyhow!("failed to read '{}': {err}", path.display()))?;
            modules.insert(specifier, parse_module(&source, path)?);
        }

        if modules.is_empty() {
            return Err(anyhow!("no components found under '{}'", root.display()));
        }

        Ok(Self { root, modules })
    }

    pub fn render_entry(&self, entry: &str, props: &Value) -> Result<String> {
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;
        self.render_export(&entry, "default", props)
    }

    fn resolve_entry(&self, entry: &str) -> Option<String> {
        let entry = normalize_slashes(entry);
        if self.modules.contains_key(&entry) {
            return Some(entry);
        }
        if Path::new(&entry).extension().is_none() {
            for ext in ["jsx", "tsx", "js", "ts"] {
                let candidate = format!("{entry}.{ext}");
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn render_export(&self, module_spec: &str, export_name: &str, props: &Value) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let local = if export_name == "default" {
            module
                .default_export
                .clone()
                .ok_or_else(|| anyhow!("module '{}' has no default export", module_spec))?
        } else {
            export_name.to_string()
        };
        self.render_local(module_spec, &local, props)
    }

    fn render_local(
        &self,
        module_spec: &str,
        function_name: &str,
        props: &Value,
    ) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let function = module.functions.get(function_name).ok_or_else(|| {
            anyhow!(
                "function '{}' missing in module '{}'",
                function_name,
                module_spec
            )
        })?;

        let mut env = HashMap::new();
        bind_params(&function.params, props, &mut env);
        let value = self.eval_expr(module_spec, &function.body_expr, &env)?;
        Ok(value_to_string(&value))
    }

    fn eval_expr(
        &self,
        module_spec: &str,
        expr: &Expr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        match expr {
            Expr::JSXElement(element) => Ok(Value::String(self.eval_jsx_element(
                module_spec,
                element,
                env,
            )?)),
            Expr::JSXFragment(fragment) => Ok(Value::String(self.eval_jsx_fragment(
                module_spec,
                fragment,
                env,
            )?)),
            Expr::Lit(lit) => Ok(lit_to_value(lit)),
            Expr::Ident(ident) => Ok(env
                .get(&ident.sym.to_string())
                .cloned()
                .unwrap_or(Value::Null)),
            Expr::Member(member) => self.eval_member(module_spec, member, env),
            Expr::Paren(paren) => self.eval_expr(module_spec, &paren.expr, env),
            _ => Err(anyhow!(
                "unsupported expression in JSX evaluator: {:?}",
                expr
            )),
        }
    }

    fn eval_member(
        &self,
        module_spec: &str,
        member: &MemberExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let object = self.eval_expr(module_spec, &member.obj, env)?;
        let prop_name = match &member.prop {
            MemberProp::Ident(ident) => ident.sym.to_string(),
            MemberProp::Computed(computed) => {
                let value = self.eval_expr(module_spec, &computed.expr, env)?;
                value_to_string(&value)
            }
            _ => return Ok(Value::Null),
        };

        if let Value::Object(map) = object {
            Ok(map.get(&prop_name).cloned().unwrap_or(Value::Null))
        } else {
            Ok(Value::Null)
        }
    }

    fn eval_jsx_fragment(
        &self,
        module_spec: &str,
        fragment: &JSXFragment,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        self.render_children(module_spec, &fragment.children, env, false)
    }

    fn eval_jsx_element(
        &self,
        module_spec: &str,
        element: &JSXElement,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        let tag = match &element.opening.name {
            JSXElementName::Ident(ident) => ident.sym.to_string(),
            _ => return Err(anyhow!("unsupported JSX tag in module '{}'", module_spec)),
        };

        if is_component_tag(&tag) {
            let mut props = Map::new();
            for (name, value) in self.read_attrs(module_spec, &element.opening.attrs, env)? {
                if !name.starts_with("on") {
                    props.insert(name, value);
                }
            }

            let children = self.read_children_as_values(module_spec, &element.children, env)?;
            if !children.is_empty() {
                if children.len() == 1 {
                    props.insert("children".to_string(), children[0].clone());
                } else {
                    props.insert("children".to_string(), Value::Array(children));
                }
            }

            return self.render_component_ref(module_spec, &tag, &Value::Object(props));
        }

        let attrs = self.read_attrs(module_spec, &element.opening.attrs, env)?;
        let attrs_html = render_attrs(&attrs);
        let children_html = self.render_children(module_spec, &element.children, env, true)?;
        let void_tag = is_void_tag(&tag);

        if void_tag && children_html.is_empty() {
            if attrs_html.is_empty() {
                Ok(format!("<{tag} />"))
            } else {
                Ok(format!("<{tag} {attrs_html} />"))
            }
        } else if attrs_html.is_empty() {
            Ok(format!("<{tag}>{children_html}</{tag}>"))
        } else {
            Ok(format!("<{tag} {attrs_html}>{children_html}</{tag}>"))
        }
    }

    fn render_component_ref(
        &self,
        module_spec: &str,
        component: &str,
        props: &Value,
    ) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;

        if let Some(import_binding) = module.imports.get(component) {
            if import_binding.source == "react" {
                return Ok(String::new());
            }
            let target = self
                .resolve_import(module_spec, &import_binding.source)
                .ok_or_else(|| {
                    anyhow!(
                        "could not resolve import '{}' from '{}'",
                        import_binding.source,
                        module_spec
                    )
                })?;
            return self.render_export(&target, &import_binding.export_name, props);
        }

        self.render_local(module_spec, component, props)
    }

    fn read_attrs(
        &self,
        module_spec: &str,
        attrs: &[JSXAttrOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<(String, Value)>> {
        let mut out = Vec::new();
        for attr in attrs {
            match attr {
                JSXAttrOrSpread::SpreadElement(_) => {
                    return Err(anyhow!("spread attributes are not supported"));
                }
                JSXAttrOrSpread::JSXAttr(attr) => {
                    let name = match &attr.name {
                        JSXAttrName::Ident(ident) => ident.sym.to_string(),
                        _ => return Err(anyhow!("unsupported JSX attribute name")),
                    };
                    let value = match &attr.value {
                        None => Value::Bool(true),
                        Some(JSXAttrValue::Lit(lit)) => lit_to_value(lit),
                        Some(JSXAttrValue::JSXExprContainer(container)) => match &container.expr {
                            JSXExpr::Expr(expr) => self.eval_expr(module_spec, expr, env)?,
                            JSXExpr::JSXEmptyExpr(_) => Value::Null,
                        },
                        _ => Value::Null,
                    };
                    out.push((name, value));
                }
            }
        }
        Ok(out)
    }

    fn read_children_as_values(
        &self,
        module_spec: &str,
        children: &[JSXElementChild],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        out.push(Value::String(normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        let value = self.eval_expr(module_spec, expr, env)?;
                        if !value.is_null() {
                            out.push(value);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    out.push(Value::String(self.eval_jsx_element(
                        module_spec,
                        element,
                        env,
                    )?));
                }
                JSXElementChild::JSXFragment(fragment) => {
                    out.push(Value::String(self.eval_jsx_fragment(
                        module_spec,
                        fragment,
                        env,
                    )?));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn render_children(
        &self,
        module_spec: &str,
        children: &[JSXElementChild],
        env: &HashMap<String, Value>,
        escape_expr_children: bool,
    ) -> Result<String> {
        let mut html = String::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        html.push_str(&escape_html(&normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        let value = self.eval_expr(module_spec, expr, env)?;
                        let text = value_to_string(&value);
                        if escape_expr_children {
                            html.push_str(&escape_html(&text));
                        } else {
                            html.push_str(&text);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    html.push_str(&self.eval_jsx_element(module_spec, element, env)?);
                }
                JSXElementChild::JSXFragment(fragment) => {
                    html.push_str(&self.eval_jsx_fragment(module_spec, fragment, env)?);
                }
                _ => {}
            }
        }
        Ok(html)
    }

    fn resolve_import(&self, current_module: &str, source: &str) -> Option<String> {
        if !source.starts_with('.') {
            return None;
        }

        let current_dir = Path::new(current_module)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let base = normalize_specifier(current_dir.join(source));
        for candidate in import_candidates(&base) {
            if self.modules.contains_key(&candidate) {
                return Some(candidate);
            }
        }

        if let Some(stripped) = source.strip_prefix("./components/") {
            let alt = normalize_specifier(PathBuf::from(stripped));
            for candidate in import_candidates(&alt) {
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

pub fn render_from_components_dir(
    components_root: impl AsRef<Path>,
    entry_module: &str,
    props: &Value,
) -> Result<String> {
    let project = ComponentProject::load_from_dir(components_root)?;
    project.render_entry(entry_module, props)
}

fn parse_module(source: &str, file_path: &Path) -> Result<ParsedModule> {
    let module = parse_source(source, file_path)?;
    let mut parsed = ParsedModule {
        imports: HashMap::new(),
        functions: HashMap::new(),
        default_export: None,
    };
    let mut synthetic_index = 0usize;

    for item in module.body {
        match item {
            ModuleItem::ModuleDecl(decl) => match decl {
                ModuleDecl::Import(import_decl) => {
                    let source = import_decl.src.value.to_string();
                    for specifier in import_decl.specifiers {
                        match specifier {
                            ImportSpecifier::Default(default_spec) => {
                                parsed.imports.insert(
                                    default_spec.local.sym.to_string(),
                                    ImportBinding {
                                        source: source.clone(),
                                        export_name: "default".to_string(),
                                    },
                                );
                            }
                            ImportSpecifier::Named(named_spec) => {
                                let local = named_spec.local.sym.to_string();
                                let export_name = named_spec
                                    .imported
                                    .as_ref()
                                    .and_then(module_export_name_to_string)
                                    .unwrap_or_else(|| local.clone());
                                parsed.imports.insert(
                                    local,
                                    ImportBinding {
                                        source: source.clone(),
                                        export_name,
                                    },
                                );
                            }
                            ImportSpecifier::Namespace(_) => {}
                        }
                    }
                }
                ModuleDecl::ExportDecl(export_decl) => match export_decl.decl {
                    Decl::Fn(fn_decl) => {
                        let name = fn_decl.ident.sym.to_string();
                        parsed
                            .functions
                            .insert(name, function_from_fn_decl(&fn_decl)?);
                    }
                    Decl::Var(var_decl) => collect_var_functions(&var_decl, &mut parsed.functions)?,
                    _ => {}
                },
                ModuleDecl::ExportDefaultDecl(default_decl) => {
                    if let DefaultDecl::Fn(fn_expr) = default_decl.decl {
                        let name = fn_expr
                            .ident
                            .as_ref()
                            .map(|ident| ident.sym.to_string())
                            .unwrap_or_else(|| {
                                let generated = format!("__default_{synthetic_index}");
                                synthetic_index += 1;
                                generated
                            });
                        parsed
                            .functions
                            .insert(name.clone(), function_from_function(&fn_expr.function)?);
                        parsed.default_export = Some(name);
                    }
                }
                ModuleDecl::ExportDefaultExpr(default_expr) => match *default_expr.expr {
                    Expr::Ident(ident) => {
                        parsed.default_export = Some(ident.sym.to_string());
                    }
                    Expr::Fn(fn_expr) => {
                        let name = fn_expr
                            .ident
                            .as_ref()
                            .map(|ident| ident.sym.to_string())
                            .unwrap_or_else(|| {
                                let generated = format!("__default_{synthetic_index}");
                                synthetic_index += 1;
                                generated
                            });
                        parsed
                            .functions
                            .insert(name.clone(), function_from_function(&fn_expr.function)?);
                        parsed.default_export = Some(name);
                    }
                    Expr::Arrow(arrow) => {
                        let name = format!("__default_{synthetic_index}");
                        synthetic_index += 1;
                        parsed
                            .functions
                            .insert(name.clone(), function_from_arrow(&arrow)?);
                        parsed.default_export = Some(name);
                    }
                    _ => {}
                },
                _ => {}
            },
            ModuleItem::Stmt(stmt) => match stmt {
                Stmt::Decl(Decl::Fn(fn_decl)) => {
                    let name = fn_decl.ident.sym.to_string();
                    parsed
                        .functions
                        .insert(name, function_from_fn_decl(&fn_decl)?);
                }
                Stmt::Decl(Decl::Var(var_decl)) => {
                    collect_var_functions(&var_decl, &mut parsed.functions)?
                }
                _ => {}
            },
        }
    }

    Ok(parsed)
}

fn parse_source(source: &str, file_path: &Path) -> Result<Module> {
    let source_map: Rc<SourceMap> = Rc::new(SourceMap::default());
    let source_file = source_map.new_source_file(
        FileName::Custom(file_path.to_string_lossy().to_string()).into(),
        source.to_string(),
    );
    let ext = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let syntax = if matches!(ext, "ts" | "tsx") {
        Syntax::Typescript(TsSyntax {
            tsx: ext == "tsx",
            decorators: true,
            ..Default::default()
        })
    } else {
        Syntax::Es(EsSyntax {
            jsx: matches!(ext, "jsx" | "js"),
            decorators: true,
            ..Default::default()
        })
    };

    let mut parser = Parser::new(syntax, StringInput::from(&*source_file), None);
    parser
        .parse_module()
        .map_err(|err| anyhow!("parse error in '{}': {:?}", file_path.display(), err))
}

fn function_from_fn_decl(fn_decl: &FnDecl) -> Result<ComponentFunction> {
    function_from_function(&fn_decl.function)
}

fn function_from_function(function: &Function) -> Result<ComponentFunction> {
    let params = function
        .params
        .iter()
        .map(|param| param_from_pat(&param.pat))
        .collect();
    let body = function
        .body
        .as_ref()
        .ok_or_else(|| anyhow!("missing function body"))?;
    let body_expr = extract_return(body)?;
    Ok(ComponentFunction { params, body_expr })
}

fn function_from_arrow(arrow: &ArrowExpr) -> Result<ComponentFunction> {
    let params = arrow.params.iter().map(param_from_pat).collect();
    let body_expr = match &*arrow.body {
        BlockStmtOrExpr::BlockStmt(block) => extract_return(block)?,
        BlockStmtOrExpr::Expr(expr) => (**expr).clone(),
    };
    Ok(ComponentFunction { params, body_expr })
}

fn collect_var_functions(
    var_decl: &VarDecl,
    out: &mut HashMap<String, ComponentFunction>,
) -> Result<()> {
    for decl in &var_decl.decls {
        let name = match &decl.name {
            Pat::Ident(binding_ident) => binding_ident.id.sym.to_string(),
            _ => continue,
        };
        let Some(init) = &decl.init else { continue };
        match &**init {
            Expr::Arrow(arrow) => {
                out.insert(name, function_from_arrow(arrow)?);
            }
            Expr::Fn(fn_expr) => {
                out.insert(name, function_from_function(&fn_expr.function)?);
            }
            _ => {}
        }
    }
    Ok(())
}

fn extract_return(block: &BlockStmt) -> Result<Expr> {
    for stmt in &block.stmts {
        if let Stmt::Return(return_stmt) = stmt {
            if let Some(expr) = &return_stmt.arg {
                return Ok((**expr).clone());
            }
        }
    }
    Err(anyhow!("component function has no return expression"))
}

fn param_from_pat(pat: &Pat) -> ParamBinding {
    match pat {
        Pat::Ident(binding_ident) => ParamBinding::Ident(binding_ident.id.sym.to_string()),
        Pat::Object(object_pat) => {
            let mut fields = Vec::new();
            for prop in &object_pat.props {
                match prop {
                    ObjectPatProp::Assign(assign) => {
                        let key = assign.key.sym.to_string();
                        fields.push((key.clone(), key));
                    }
                    ObjectPatProp::KeyValue(key_value) => {
                        let key = prop_name_to_string(&key_value.key);
                        let local = match &*key_value.value {
                            Pat::Ident(binding_ident) => Some(binding_ident.id.sym.to_string()),
                            _ => None,
                        };
                        if let (Some(key), Some(local)) = (key, local) {
                            fields.push((key, local));
                        }
                    }
                    ObjectPatProp::Rest(_) => {}
                }
            }
            ParamBinding::Object(fields)
        }
        _ => ParamBinding::Ignore,
    }
}

fn bind_params(params: &[ParamBinding], props: &Value, env: &mut HashMap<String, Value>) {
    let props_map = props.as_object().cloned().unwrap_or_default();
    for param in params {
        match param {
            ParamBinding::Ident(name) => {
                env.insert(name.clone(), props.clone());
            }
            ParamBinding::Object(fields) => {
                for (key, local) in fields {
                    env.insert(
                        local.clone(),
                        props_map.get(key).cloned().unwrap_or(Value::Null),
                    );
                }
            }
            ParamBinding::Ignore => {}
        }
    }
}

fn module_export_name_to_string(name: &ModuleExportName) -> Option<String> {
    match name {
        ModuleExportName::Ident(ident) => Some(ident.sym.to_string()),
        ModuleExportName::Str(str_lit) => Some(str_lit.value.to_string()),
    }
}

fn prop_name_to_string(name: &PropName) -> Option<String> {
    match name {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(str_lit) => Some(str_lit.value.to_string()),
        PropName::Num(num) => Some(num.value.to_string()),
        _ => None,
    }
}

fn lit_to_value(lit: &Lit) -> Value {
    match lit {
        Lit::Str(str_lit) => Value::String(str_lit.value.to_string()),
        Lit::Bool(bool_lit) => Value::Bool(bool_lit.value),
        Lit::Num(num) => serde_json::Number::from_f64(num.value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Lit::Null(_) => Value::Null,
        _ => Value::Null,
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        Value::Array(values) => values.iter().map(value_to_string).collect(),
        Value::Object(object) => serde_json::to_string(object).unwrap_or_default(),
    }
}

fn normalize_specifier(path: impl AsRef<Path>) -> String {
    let mut parts = Vec::new();
    for component in path.as_ref().components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            std::path::Component::Normal(segment) => {
                parts.push(segment.to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    normalize_slashes(&parts.join("/"))
}

fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

fn import_candidates(base: &str) -> Vec<String> {
    let mut out = Vec::new();
    if Path::new(base).extension().is_some() {
        out.push(base.to_string());
    } else {
        for ext in ["jsx", "tsx", "js", "ts"] {
            out.push(format!("{base}.{ext}"));
        }
        for ext in ["jsx", "tsx", "js", "ts"] {
            out.push(format!("{base}/index.{ext}"));
        }
    }
    out
}

fn normalize_jsx_text(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn is_component_tag(tag: &str) -> bool {
    tag.chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    escape_html(value).replace('"', "&quot;")
}

fn render_attrs(attrs: &[(String, Value)]) -> String {
    let mut out = Vec::new();
    for (name, value) in attrs {
        if name.starts_with("on") {
            continue;
        }
        let attr_name = if name == "className" { "class" } else { name };
        match value {
            Value::Null => {}
            Value::Bool(false) => {}
            Value::Bool(true) => out.push(attr_name.to_string()),
            _ => {
                let text = value_to_string(value);
                if !text.is_empty() {
                    out.push(format!("{attr_name}=\"{}\"", escape_attr(&text)));
                }
            }
        }
    }
    out.join(" ")
}

fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_test_app_components_entry() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app")
            .join("src")
            .join("components");
        let project = ComponentProject::load_from_dir(root).unwrap();
        let html = project
            .render_entry("App.jsx", &Value::Object(Map::new()))
            .unwrap();

        assert!(html.contains("<div class=\"App\">"));
        assert!(html.contains("<h1>My App</h1>"));
        assert!(html.contains("<button>Home</button>"));
        assert!(html.contains("<h3>Fast</h3>"));
        assert!(html.contains("<p>© 2026 My App</p>"));
    }
}
