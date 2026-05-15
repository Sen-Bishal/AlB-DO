use crate::types::ComponentId;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::runtime::eval::component::{
    arg_num, classnames_collect, date_value_ms, escape_html, fnv1a_32, fnv1a_hash,
    import_candidates, is_classnames_source, is_component_module, is_component_tag, is_truthy,
    is_void_tag, json_int, json_num, lit_to_value, make_date_value, normalize_jsx_text,
    normalize_slashes, normalize_specifier, prop_name_to_string, render_attrs, to_number,
    value_to_string,
};

thread_local! {
    /// Per-render element counter. Reset by `render_entry` at the top of
    /// every render call so element ids are deterministic per render and
    /// independent across concurrent renders on different threads.
    ///
    /// Combined with `module_spec` it produces the FNV-1a-32 input that
    /// becomes `data-albedo-id` on every shell element bakabox should be
    /// able to address. Phase K's compiler can replace this with a
    /// content-hash strategy when HMR stability matters.
    static RENDER_ELEMENT_COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// Bakabox reads anchors from `data-albedo-id` (DEFAULT_ANCHOR_ATTRIBUTE
/// in `assets/albedo-runtime.js`). Keep these in sync.
pub const ALBEDO_ID_ATTR: &str = "data-albedo-id";

fn next_element_stable_id(module_spec: &str) -> u32 {
    RENDER_ELEMENT_COUNTER.with(|cell| {
        let counter = cell.get();
        cell.set(counter.wrapping_add(1));
        let key = format!("{module_spec}#{counter}");
        fnv1a_32(key.as_bytes())
    })
}

fn reset_element_counter() {
    RENDER_ELEMENT_COUNTER.with(|cell| cell.set(0));
}
use crate::runtime::eval::expr::{
    apply_var_pat_to_env, bind_params, bind_params_positional, param_from_pat,
    parse_module as parse_module_impl, ParamBinding, ParsedModule,
};

#[derive(Debug, Clone)]
pub struct ComponentProject {
    root: PathBuf,
    modules: HashMap<String, ParsedModule>,
    source_hashes: HashMap<String, u64>,
    specifier_to_id: HashMap<String, ComponentId>,
    next_id: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchReport {
    pub reparsed: usize,
    pub skipped_unchanged: usize,
    pub deleted: usize,
    pub reparsed_ids: Vec<ComponentId>,
    pub reparsed_specifiers: Vec<String>,
    pub deleted_ids: Vec<ComponentId>,
    pub deleted_specifiers: Vec<String>,
}

impl ComponentProject {
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut modules = HashMap::new();
        let mut source_hashes = HashMap::new();
        let mut specifier_to_id: HashMap<String, ComponentId> = HashMap::new();
        let mut next_id: u64 = 0;

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if !is_component_module(path) {
                continue;
            }

            let relative = path
                .strip_prefix(&root)
                .map_err(|err| anyhow!("failed to compute module path: {err}"))?;
            let specifier = normalize_specifier(relative);
            let source = std::fs::read_to_string(path)
                .map_err(|err| anyhow!("failed to read '{}': {err}", path.display()))?;
            let parsed = parse_module_impl(&source, path)?;
            source_hashes.insert(specifier.clone(), fnv1a_hash(source.as_bytes()));
            specifier_to_id.insert(specifier.clone(), ComponentId::new(next_id));
            next_id += 1;
            modules.insert(specifier, parsed);
        }

        if modules.is_empty() {
            return Err(anyhow!("no components found under '{}'", root.display()));
        }

        Ok(Self {
            root,
            modules,
            source_hashes,
            specifier_to_id,
            next_id,
        })
    }

    pub fn patch(
        &mut self,
        changed_paths: &[PathBuf],
        deleted_paths: &[PathBuf],
    ) -> Result<PatchReport> {
        let mut report = PatchReport::default();
        let mut parsed_updates = Vec::new();
        let mut staged_deletions = HashSet::new();
        let mut seen_changed = HashSet::new();

        for changed_path in changed_paths {
            let Some((specifier, absolute_path)) = self.module_specifier_for_path(changed_path)
            else {
                continue;
            };

            if !seen_changed.insert(specifier.clone()) {
                continue;
            }

            match std::fs::read_to_string(&absolute_path) {
                Ok(source) => {
                    let next_hash = fnv1a_hash(source.as_bytes());
                    if self.source_hashes.get(&specifier).copied() == Some(next_hash) {
                        report.skipped_unchanged += 1;
                        continue;
                    }

                    let parsed = parse_module_impl(&source, &absolute_path)?;
                    parsed_updates.push((specifier, parsed, next_hash));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    staged_deletions.insert(specifier);
                }
                Err(err) => {
                    return Err(anyhow!(
                        "failed to read '{}' while patching: {err}",
                        absolute_path.display()
                    ));
                }
            }
        }

        for deleted_path in deleted_paths {
            let Some((specifier, _)) = self.module_specifier_for_path(deleted_path) else {
                continue;
            };
            staged_deletions.insert(specifier);
        }

        for (specifier, parsed, source_hash) in parsed_updates {
            self.modules.insert(specifier.clone(), parsed);
            self.source_hashes.insert(specifier.clone(), source_hash);
            let component_id = *self
                .specifier_to_id
                .entry(specifier.clone())
                .or_insert_with(|| {
                    let id = ComponentId::new(self.next_id);
                    self.next_id += 1;
                    id
                });
            report.reparsed_ids.push(component_id);
            report.reparsed_specifiers.push(specifier);
            report.reparsed += 1;
        }

        for specifier in staged_deletions {
            let component_id = self.specifier_to_id.get(&specifier).copied();
            let removed_module = self.modules.remove(&specifier).is_some();
            let removed_hash = self.source_hashes.remove(&specifier).is_some();
            if removed_module || removed_hash {
                if let Some(component_id) = component_id {
                    report.deleted_ids.push(component_id);
                }
                report.deleted_specifiers.push(specifier);
                report.deleted += 1;
            }
        }

        Ok(report)
    }

    pub fn component_id_for_specifier(&self, specifier: &str) -> Option<ComponentId> {
        let spec = normalize_slashes(specifier);
        self.specifier_to_id.get(&spec).copied()
    }

    pub fn component_id_for_name(&self, name: &str) -> Option<ComponentId> {
        self.specifier_to_id
            .iter()
            .find(|(spec, _)| {
                Path::new(spec)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
            .map(|(_, &id)| id)
    }

    pub fn component_id_by_name(&self, name: &str) -> Option<ComponentId> {
        self.component_id_for_name(name)
    }

    pub fn render_entry(&self, entry: &str, props: &Value) -> Result<String> {
        // Each top-level render starts with a fresh element counter so the
        // `data-albedo-id` attributes the renderer stamps are stable per
        // render and don't leak across concurrent requests.
        reset_element_counter();
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

    fn module_specifier_for_path(&self, path: &Path) -> Option<(String, PathBuf)> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let relative_path = absolute_path.strip_prefix(&self.root).ok()?;
        if !is_component_module(relative_path) {
            return None;
        }
        Some((normalize_specifier(relative_path), absolute_path))
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
        // Observer frame: opens a cascade-tracking scope for this component's
        // render. The guard publishes a `RenderInfo` on drop iff a process-wide
        // `RenderObserver` is installed — when none is, the whole scope
        // collapses to a single `OnceLock::get()` check.
        let _frame =
            crate::runtime::render_observer::enter_frame_guard(function_name, module_spec);

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
        let stmts = function.body_stmts.clone();
        self.eval_body_stmts(module_spec, &stmts, &mut env)
    }

    fn eval_expr(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
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
            Expr::Ident(ident) => {
                let name = ident.sym.to_string();
                if let Some(value) = env.get(&name) {
                    Ok(value.clone())
                } else {
                    // Static evaluator has no binding for this identifier.
                    // Phase K wires reactive bindings; until then make the
                    // miss findable in dev rather than letting it vanish.
                    tracing::debug!(
                        target: "albedo::eval",
                        ident = %name,
                        module = %module_spec,
                        "unbound identifier in JSX expression — evaluating to null",
                    );
                    Ok(Value::Null)
                }
            }
            Expr::Member(member) => self.eval_member(module_spec, member, env),
            Expr::Paren(paren) => self.eval_expr(module_spec, &paren.expr, env),
            Expr::Tpl(tpl) => self.eval_tpl(module_spec, tpl, env),
            Expr::Bin(bin) => self.eval_bin(module_spec, bin, env),
            Expr::Cond(cond) => self.eval_cond(module_spec, cond, env),
            Expr::Call(call) => self.eval_call_expr(module_spec, call, env),
            Expr::New(new_expr) => self.eval_new_expr(module_spec, new_expr, env),
            Expr::Array(arr) => self.eval_array_expr(module_spec, arr, env),
            Expr::Object(obj) => self.eval_object_expr(module_spec, obj, env),
            Expr::Unary(unary) => self.eval_unary(module_spec, unary, env),
            Expr::OptChain(opt) => self.eval_opt_chain(module_spec, opt, env),
            Expr::Seq(seq) => {
                let mut last = Value::Null;
                for expr in &seq.exprs {
                    last = self.eval_expr(module_spec, expr, env)?;
                }
                Ok(last)
            }
            // TypeScript escape hatches are runtime no-ops: unwrap to the
            // inner expression. SWC keeps these in the AST when JSX/TSX
            // sources contain `as`, `!`, `<X>e`, `satisfies`, `as const`,
            // or `f<T>` instantiation expressions.
            Expr::TsAs(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsNonNull(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsConstAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsTypeAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsSatisfies(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsInstantiation(node) => self.eval_expr(module_spec, &node.expr, env),
            other => {
                // Phase J keeps unhandled shapes returning Null for backwards
                // compatibility, but never silently — every drop emits a
                // tracing event that lets us extend the evaluator. Phase K's
                // SWC pass will compile most of these away into slot-store
                // opcodes, so this list should shrink, not grow.
                tracing::debug!(
                    target: "albedo::eval",
                    module = %module_spec,
                    expr_kind = std::any::type_name_of_val(other),
                    "unhandled JSX expression shape — evaluating to null",
                );
                Ok(Value::Null)
            }
        }
    }

    fn eval_opt_chain(
        &self,
        module_spec: &str,
        opt: &swc_ecma_ast::OptChainExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match &*opt.base {
            OptChainBase::Member(member) => {
                let obj = self.eval_expr(module_spec, &member.obj, env)?;
                if matches!(obj, Value::Null) {
                    return Ok(Value::Null);
                }
                self.eval_member_on(module_spec, &obj, &member.prop, env)
            }
            OptChainBase::Call(call) => {
                let callee = self.eval_expr(module_spec, &call.callee, env)?;
                if matches!(callee, Value::Null) {
                    return Ok(Value::Null);
                }
                // Callable-value support is Phase K; until then, treat
                // optional calls as null when reachable.
                Ok(Value::Null)
            }
        }
    }

    fn eval_new_expr(
        &self,
        module_spec: &str,
        new_expr: &swc_ecma_ast::NewExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Phase J only models `new Date(...)` because that's what ships in
        // the matrix. Other constructors fall through to Null with a trace.
        if let Expr::Ident(ident) = &*new_expr.callee {
            if ident.sym.as_ref() == "Date" {
                let args: Vec<Value> = match &new_expr.args {
                    Some(args) => args
                        .iter()
                        .map(|a| self.eval_expr(module_spec, &a.expr, env))
                        .collect::<Result<Vec<_>>>()?,
                    None => Vec::new(),
                };
                let ms = match args.first() {
                    None => 0.0, // Phase J: deterministic; no system clock.
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                    Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
                    _ => 0.0,
                };
                return Ok(make_date_value(ms));
            }
        }
        tracing::debug!(
            target: "albedo::eval",
            module = %module_spec,
            "unhandled `new` constructor — evaluating to null",
        );
        Ok(Value::Null)
    }

    fn eval_member(
        &self,
        module_spec: &str,
        member: &swc_ecma_ast::MemberExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let object = self.eval_expr(module_spec, &member.obj, env)?;
        self.eval_member_on(module_spec, &object, &member.prop, env)
    }

    /// Resolve a property access on an already-evaluated value. Factored
    /// out so `Expr::OptChain` and `Expr::Member` share the dispatch.
    fn eval_member_on(
        &self,
        module_spec: &str,
        object: &Value,
        prop: &swc_ecma_ast::MemberProp,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Computed access uses the runtime value verbatim — for arrays we
        // want a numeric index without stringifying through `value_to_string`,
        // which would render `1` as `"1"` and lose array-vs-object intent.
        match prop {
            MemberProp::Computed(computed) => {
                let key = self.eval_expr(module_spec, &computed.expr, env)?;
                if let (Value::Array(items), Some(idx)) = (object, key.as_f64()) {
                    if idx.is_finite() && idx >= 0.0 && idx == idx.trunc() {
                        return Ok(items.get(idx as usize).cloned().unwrap_or(Value::Null));
                    }
                }
                let prop_name = value_to_string(&key);
                self.lookup_named_prop(object, &prop_name)
            }
            MemberProp::Ident(ident) => {
                let prop_name = ident.sym.to_string();
                self.lookup_named_prop(object, &prop_name)
            }
            _ => Ok(Value::Null),
        }
    }

    fn lookup_named_prop(&self, object: &Value, prop_name: &str) -> Result<Value> {
        match object {
            Value::Object(map) => {
                // Date-tagged objects expose no JS-level properties; method
                // calls on them are handled in `eval_call_expr` via the
                // member callee path.
                Ok(map.get(prop_name).cloned().unwrap_or(Value::Null))
            }
            Value::Array(items) => match prop_name {
                "length" => Ok(json_int(items.len() as i64)),
                _ => {
                    // Numeric string indexing: `arr["0"]` matches JS semantics.
                    if let Ok(idx) = prop_name.parse::<usize>() {
                        return Ok(items.get(idx).cloned().unwrap_or(Value::Null));
                    }
                    Ok(Value::Null)
                }
            },
            Value::String(s) => match prop_name {
                "length" => Ok(json_int(s.chars().count() as i64)),
                _ => Ok(Value::Null),
            },
            _ => Ok(Value::Null),
        }
    }

    fn eval_tpl(
        &self,
        module_spec: &str,
        tpl: &swc_ecma_ast::Tpl,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let mut result = String::new();
        for (i, quasi) in tpl.quasis.iter().enumerate() {
            let text = quasi
                .cooked
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| quasi.raw.to_string());
            result.push_str(&text);
            if i < tpl.exprs.len() {
                let val = self.eval_expr(module_spec, &tpl.exprs[i], env)?;
                result.push_str(&value_to_string(&val));
            }
        }
        Ok(Value::String(result))
    }

    fn eval_bin(
        &self,
        module_spec: &str,
        bin: &swc_ecma_ast::BinExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match bin.op {
            BinaryOp::LogicalAnd => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if !is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::LogicalOr => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::NullishCoalescing => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if matches!(left, Value::Null) {
                    self.eval_expr(module_spec, &bin.right, env)
                } else {
                    Ok(left)
                }
            }
            BinaryOp::Add => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                match (&left, &right) {
                    (Value::Number(l), Value::Number(r)) => Ok(json_num(
                        l.as_f64().unwrap_or(0.0) + r.as_f64().unwrap_or(0.0),
                    )),
                    _ => Ok(Value::String(format!(
                        "{}{}",
                        value_to_string(&left),
                        value_to_string(&right)
                    ))),
                }
            }
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod | BinaryOp::Exp => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let value = match bin.op {
                    BinaryOp::Sub => l - r,
                    BinaryOp::Mul => l * r,
                    BinaryOp::Div => l / r,
                    BinaryOp::Mod => l % r,
                    BinaryOp::Exp => l.powf(r),
                    _ => unreachable!(),
                };
                Ok(json_num(value))
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let result = match bin.op {
                    BinaryOp::Lt => l < r,
                    BinaryOp::Gt => l > r,
                    BinaryOp::LtEq => l <= r,
                    BinaryOp::GtEq => l >= r,
                    _ => unreachable!(),
                };
                Ok(Value::Bool(result))
            }
            BinaryOp::EqEq | BinaryOp::EqEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) == value_to_string(&right),
                ))
            }
            BinaryOp::NotEq | BinaryOp::NotEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) != value_to_string(&right),
                ))
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_cond(
        &self,
        module_spec: &str,
        cond: &swc_ecma_ast::CondExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let test = self.eval_expr(module_spec, &cond.test, env)?;
        if is_truthy(&test) {
            self.eval_expr(module_spec, &cond.cons, env)
        } else {
            self.eval_expr(module_spec, &cond.alt, env)
        }
    }

    fn eval_unary(
        &self,
        module_spec: &str,
        unary: &swc_ecma_ast::UnaryExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let val = self.eval_expr(module_spec, &unary.arg, env)?;
        match unary.op {
            UnaryOp::Bang => Ok(Value::Bool(!is_truthy(&val))),
            UnaryOp::Minus => {
                if let Value::Number(n) = &val {
                    Ok(serde_json::Number::from_f64(-n.as_f64().unwrap_or(0.0))
                        .map(Value::Number)
                        .unwrap_or(Value::Null))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_array_expr(
        &self,
        module_spec: &str,
        arr: &swc_ecma_ast::ArrayLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut out = Vec::with_capacity(arr.elems.len());
        for elem in &arr.elems {
            if let Some(ExprOrSpread { expr, spread: None }) = elem {
                out.push(self.eval_expr(module_spec, expr, env)?);
            }
        }
        Ok(Value::Array(out))
    }

    fn eval_object_expr(
        &self,
        module_spec: &str,
        obj: &swc_ecma_ast::ObjectLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut map = serde_json::Map::new();
        for prop in &obj.props {
            if let PropOrSpread::Prop(prop_box) = prop {
                match prop_box.as_ref() {
                    Prop::KeyValue(kv) => {
                        if let Some(key) = prop_name_to_string(&kv.key) {
                            let val = self.eval_expr(module_spec, &kv.value, env)?;
                            map.insert(key, val);
                        }
                    }
                    Prop::Shorthand(ident) => {
                        let name = ident.sym.to_string();
                        let val = env.get(&name).cloned().unwrap_or(Value::Null);
                        map.insert(name, val);
                    }
                    _ => {}
                }
            }
        }
        Ok(Value::Object(map))
    }

    fn eval_call_expr(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        // --- Member-callee dispatch: obj.method(...args) -----------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Member(member) = callee_expr.as_ref() {
                if let MemberProp::Ident(prop_ident) = &member.prop {
                    let method = prop_ident.sym.to_string();

                    // Static-namespace dispatch (Math.x, Date.x, JSON.x, ...)
                    // is handled before evaluating `member.obj` because the
                    // namespace itself isn't a value we model — `Math.floor`
                    // would otherwise try to look up `Math` in env and miss.
                    if let Expr::Ident(ns_ident) = &*member.obj {
                        let ns_name = ns_ident.sym.to_string();
                        if !env.contains_key(&ns_name) {
                            if let Some(value) = self.eval_static_namespace_call(
                                module_spec,
                                &ns_name,
                                &method,
                                &call.args,
                                env,
                            )? {
                                return Ok(value);
                            }
                        }
                    }

                    // Instance-method dispatch.
                    let obj_val = self.eval_expr(module_spec, &member.obj, env)?;
                    if let Some(value) = self.eval_instance_method(
                        module_spec,
                        &obj_val,
                        &method,
                        &call.args,
                        env,
                    )? {
                        return Ok(value);
                    }
                }
            }
        }

        // --- Bare-ident callee dispatch: f(...args) ----------------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Ident(ident) = callee_expr.as_ref() {
                let fn_name = ident.sym.to_string();
                let module = self.modules.get(module_spec);
                let import = module.and_then(|m| m.imports.get(&fn_name));

                // classnames / clsx — flatten args into a class string.
                let is_classnames = import
                    .map(|b| is_classnames_source(&b.source))
                    .unwrap_or(false);
                if is_classnames {
                    let mut classes = Vec::new();
                    for arg in &call.args {
                        if arg.spread.is_some() {
                            continue;
                        }
                        let val = self.eval_expr(module_spec, &arg.expr, env)?;
                        classnames_collect(&val, &mut classes);
                    }
                    return Ok(Value::String(classes.join(" ")));
                }

                // useState shim (Phase J): recognize the React import and
                // return `[initial, null]`. Phase K replaces this with real
                // slot-store reads/writes; until then this lets `{count}`
                // render its initial value instead of vanishing.
                let is_react_use_state = fn_name == "useState"
                    && import
                        .map(|b| b.source == "react" && b.export_name == "useState")
                        .unwrap_or(false);
                if is_react_use_state {
                    let initial = match call.args.first() {
                        Some(arg) if arg.spread.is_none() => {
                            self.eval_expr(module_spec, &arg.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(Value::Array(vec![initial, Value::Null]));
                }

                // JS-style coercions.
                if fn_name == "String" || fn_name == "Number" || fn_name == "Boolean" {
                    let arg = match call.args.first() {
                        Some(a) if a.spread.is_none() => {
                            self.eval_expr(module_spec, &a.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(match fn_name.as_str() {
                        "String" => Value::String(value_to_string(&arg)),
                        "Number" => json_num(to_number(&arg)),
                        "Boolean" => Value::Bool(is_truthy(&arg)),
                        _ => unreachable!(),
                    });
                }
            }
        }

        Ok(Value::Null)
    }

    fn eval_static_namespace_call(
        &self,
        module_spec: &str,
        ns: &str,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        let evaluated: Vec<Value> = args
            .iter()
            .filter(|a| a.spread.is_none())
            .map(|a| self.eval_expr(module_spec, &a.expr, env))
            .collect::<Result<Vec<_>>>()?;

        let result = match (ns, method) {
            // Math.* — covers everything that shows up in display logic.
            ("Math", "floor") => json_num(arg_num(&evaluated, 0).floor()),
            ("Math", "ceil") => json_num(arg_num(&evaluated, 0).ceil()),
            ("Math", "round") => json_num(arg_num(&evaluated, 0).round()),
            ("Math", "trunc") => json_num(arg_num(&evaluated, 0).trunc()),
            ("Math", "abs") => json_num(arg_num(&evaluated, 0).abs()),
            ("Math", "sqrt") => json_num(arg_num(&evaluated, 0).sqrt()),
            ("Math", "max") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::NEG_INFINITY, f64::max),
            ),
            ("Math", "min") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::INFINITY, f64::min),
            ),
            ("Math", "pow") => json_num(arg_num(&evaluated, 0).powf(arg_num(&evaluated, 1))),

            // Date statics — no system clock in Phase J (deterministic SSR).
            // `Date.now()` returns 0; user code that wants a real timestamp
            // should accept it as a prop. Phase K will surface a clock slot.
            ("Date", "now") => json_int(0),

            // JSON.* — useful in display-time templates for debug surfaces.
            ("JSON", "stringify") => match evaluated.first() {
                Some(value) => Value::String(serde_json::to_string(value).unwrap_or_default()),
                None => Value::Null,
            },

            // Object.keys / Object.values — used in admin/debug UIs.
            ("Object", "keys") => match evaluated.first() {
                Some(Value::Object(map)) => {
                    Value::Array(map.keys().cloned().map(Value::String).collect())
                }
                _ => Value::Array(Vec::new()),
            },
            ("Object", "values") => match evaluated.first() {
                Some(Value::Object(map)) => Value::Array(map.values().cloned().collect()),
                _ => Value::Array(Vec::new()),
            },

            _ => return Ok(None),
        };

        Ok(Some(result))
    }

    fn eval_instance_method(
        &self,
        module_spec: &str,
        receiver: &Value,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        // Date instance methods first — Date is encoded as a tagged object.
        if let Some(ms) = date_value_ms(receiver) {
            return Ok(Some(self.eval_date_method(method, ms)));
        }

        match receiver {
            Value::String(s) => {
                let result = match method {
                    "toUpperCase" => Some(Value::String(s.to_uppercase())),
                    "toLowerCase" => Some(Value::String(s.to_lowercase())),
                    "trim" => Some(Value::String(s.trim().to_string())),
                    "trimStart" | "trimLeft" => Some(Value::String(s.trim_start().to_string())),
                    "trimEnd" | "trimRight" => Some(Value::String(s.trim_end().to_string())),
                    "toString" => Some(Value::String(s.clone())),
                    _ => None,
                };
                Ok(result)
            }
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                let evaluated: Vec<Value> = args
                    .iter()
                    .filter(|a| a.spread.is_none())
                    .map(|a| self.eval_expr(module_spec, &a.expr, env))
                    .collect::<Result<Vec<_>>>()?;
                let result = match method {
                    "toFixed" => {
                        let digits = arg_num(&evaluated, 0).clamp(0.0, 100.0) as usize;
                        Some(Value::String(format!("{:.*}", digits, f)))
                    }
                    "toString" => {
                        let radix = if evaluated.is_empty() {
                            10.0
                        } else {
                            arg_num(&evaluated, 0)
                        };
                        if radix == 10.0 {
                            Some(Value::String(value_to_string(receiver)))
                        } else if (radix - radix.trunc()).abs() < f64::EPSILON
                            && (2.0..=36.0).contains(&radix)
                            && f.is_finite()
                            && f == f.trunc()
                        {
                            let int = f as i64;
                            let radix = radix as u32;
                            let mut digits = String::new();
                            let (sign, mut value) = if int < 0 {
                                ("-", (-(int as i128)) as u128)
                            } else {
                                ("", int as u128)
                            };
                            if value == 0 {
                                digits.push('0');
                            }
                            while value > 0 {
                                let d = (value % radix as u128) as u32;
                                let ch = std::char::from_digit(d, radix).unwrap_or('0');
                                digits.insert(0, ch);
                                value /= radix as u128;
                            }
                            Some(Value::String(format!("{sign}{digits}")))
                        } else {
                            Some(Value::String(value_to_string(receiver)))
                        }
                    }
                    _ => None,
                };
                Ok(result)
            }
            Value::Array(items) => match method {
                "map" => {
                    if let Some(swc_ecma_ast::ExprOrSpread {
                        expr: mapper,
                        spread: None,
                    }) = args.first()
                    {
                        let parts = items
                            .iter()
                            .enumerate()
                            .map(|(i, item)| {
                                self.eval_closure(module_spec, mapper, item, i, env)
                                    .map(|v| value_to_string(&v))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        return Ok(Some(Value::String(parts.join(""))));
                    }
                    Ok(Some(Value::Null))
                }
                "join" => {
                    let sep = match args.first() {
                        Some(a) if a.spread.is_none() => {
                            value_to_string(&self.eval_expr(module_spec, &a.expr, env)?)
                        }
                        _ => ",".to_string(),
                    };
                    let parts: Vec<String> = items.iter().map(value_to_string).collect();
                    Ok(Some(Value::String(parts.join(&sep))))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn eval_date_method(&self, method: &str, ms: f64) -> Value {
        match method {
            "getTime" | "valueOf" => json_num(ms),
            "toISOString" | "toJSON" | "toString" => {
                Value::String(value_to_string(&make_date_value(ms)))
            }
            _ => Value::Null,
        }
    }

    fn eval_closure(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        arg: &Value,
        index: usize,
        parent_env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        match expr {
            Expr::Arrow(arrow) => {
                let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                match &*arrow.body {
                    BlockStmtOrExpr::BlockStmt(block) => self
                        .eval_body_stmts(module_spec, &block.stmts, &mut env)
                        .map(Value::String),
                    BlockStmtOrExpr::Expr(body_expr) => {
                        self.eval_expr(module_spec, body_expr, &env)
                    }
                }
            }
            Expr::Fn(fn_expr) => {
                let params: Vec<ParamBinding> = fn_expr
                    .function
                    .params
                    .iter()
                    .map(|p| param_from_pat(&p.pat))
                    .collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                if let Some(body) = &fn_expr.function.body {
                    self.eval_body_stmts(module_spec, &body.stmts, &mut env)
                        .map(Value::String)
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_body_stmts(
        &self,
        module_spec: &str,
        stmts: &[swc_ecma_ast::Stmt],
        env: &mut HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

        for stmt in stmts {
            match stmt {
                Stmt::Return(ret) => {
                    let value = if let Some(expr) = &ret.arg {
                        self.eval_expr(module_spec, expr, env)?
                    } else {
                        Value::Null
                    };
                    return Ok(value_to_string(&value));
                }
                Stmt::Decl(Decl::Var(var)) => {
                    self.eval_var_decl_into_env(module_spec, var, env);
                }
                _ => {}
            }
        }
        Ok(String::new())
    }

    fn eval_var_decl_into_env(
        &self,
        module_spec: &str,
        var: &swc_ecma_ast::VarDecl,
        env: &mut HashMap<String, Value>,
    ) {
        for decl in &var.decls {
            let value = if let Some(init) = &decl.init {
                self.eval_expr(module_spec, init, env)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            apply_var_pat_to_env(&decl.name, value, env);
        }
    }

    fn eval_jsx_fragment(
        &self,
        module_spec: &str,
        fragment: &swc_ecma_ast::JSXFragment,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        self.render_children(module_spec, &fragment.children, env, false)
    }

    fn eval_jsx_element(
        &self,
        module_spec: &str,
        element: &swc_ecma_ast::JSXElement,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

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

        let mut attrs = self.read_attrs(module_spec, &element.opening.attrs, env)?;

        // Shell-stamp every host (lowercase-tag) element with a stable
        // `data-albedo-id`. Bakabox's `seedNodesFromDocument` looks for
        // exactly this attribute (DEFAULT_ANCHOR_ATTRIBUTE) at boot, so
        // this is the single contract that makes any future Tier-B/C
        // patch addressable. The id is derived BEFORE children render so
        // counter ordering is pre-order and matches client-side traversal.
        //
        // We don't override an explicit user-supplied `data-albedo-id`,
        // which lets test harnesses or static fragments pin a known id.
        if !attrs
            .iter()
            .any(|(name, _)| name == ALBEDO_ID_ATTR)
        {
            let stable_id = next_element_stable_id(module_spec);
            attrs.push((
                ALBEDO_ID_ATTR.to_string(),
                Value::String(stable_id.to_string()),
            ));
        }

        let attrs_html = render_attrs(&attrs);
        let children_html = self.render_children(module_spec, &element.children, env, false)?;
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
        attrs: &[swc_ecma_ast::JSXAttrOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<(String, Value)>> {
        use swc_ecma_ast::*;
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
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        use swc_ecma_ast::*;
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
                        if !matches!(value, Value::Null | Value::Bool(false)) {
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
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
        escape_expr_children: bool,
    ) -> Result<String> {
        use swc_ecma_ast::*;
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
                        if matches!(value, Value::Null | Value::Bool(false)) {
                            continue;
                        }
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
