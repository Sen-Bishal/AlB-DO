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
    body_stmts: Vec<Stmt>,
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
        let stmts = function.body_stmts.clone();
        self.eval_body_stmts(module_spec, &stmts, &mut env)
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
            Expr::Tpl(tpl) => self.eval_tpl(module_spec, tpl, env),
            Expr::Bin(bin) => self.eval_bin(module_spec, bin, env),
            Expr::Cond(cond) => self.eval_cond(module_spec, cond, env),
            Expr::Call(call) => self.eval_call_expr(module_spec, call, env),
            Expr::Array(arr) => self.eval_array_expr(module_spec, arr, env),
            Expr::Object(obj) => self.eval_object_expr(module_spec, obj, env),
            Expr::Unary(unary) => self.eval_unary(module_spec, unary, env),
            // Graceful fallback for unsupported expressions — yield Null rather than crash
            _ => Ok(Value::Null),
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

    fn eval_tpl(
        &self,
        module_spec: &str,
        tpl: &Tpl,
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
        bin: &BinExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        match bin.op {
            // Short-circuit logical ops
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
            // String / number addition
            BinaryOp::Add => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                match (&left, &right) {
                    (Value::Number(l), Value::Number(r)) => {
                        let sum = l.as_f64().unwrap_or(0.0) + r.as_f64().unwrap_or(0.0);
                        Ok(serde_json::Number::from_f64(sum)
                            .map(Value::Number)
                            .unwrap_or(Value::Null))
                    }
                    _ => Ok(Value::String(format!(
                        "{}{}",
                        value_to_string(&left),
                        value_to_string(&right)
                    ))),
                }
            }
            // Comparison operators — return Bool (used in ternary conditions)
            BinaryOp::EqEq | BinaryOp::EqEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(value_to_string(&left) == value_to_string(&right)))
            }
            BinaryOp::NotEq | BinaryOp::NotEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(value_to_string(&left) != value_to_string(&right)))
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_cond(
        &self,
        module_spec: &str,
        cond: &CondExpr,
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
        unary: &UnaryExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
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
        arr: &ArrayLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
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
        obj: &ObjectLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
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
        call: &CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        // --- Array.map() ---
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Member(member) = callee_expr.as_ref() {
                if let MemberProp::Ident(prop_ident) = &member.prop {
                    let method = prop_ident.sym.as_ref();
                    let obj_val = self.eval_expr(module_spec, &member.obj, env)?;

                    // .map(fn) — render each item
                    if method == "map" {
                        if let Value::Array(items) = obj_val {
                            if let Some(ExprOrSpread { expr: mapper, spread: None }) = call.args.first() {
                                let parts = items
                                    .iter()
                                    .enumerate()
                                    .map(|(i, item)| {
                                        self.eval_closure(module_spec, mapper, item, i, env)
                                            .map(|v| value_to_string(&v))
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                return Ok(Value::String(parts.join("")));
                            }
                        }
                        return Ok(Value::Null);
                    }

                    // String prototype methods
                    if let Value::String(s) = &obj_val {
                        let result = match method {
                            "toUpperCase" => Some(s.to_uppercase()),
                            "toLowerCase" => Some(s.to_lowercase()),
                            "trim" => Some(s.trim().to_string()),
                            "trimStart" | "trimLeft" => Some(s.trim_start().to_string()),
                            "trimEnd" | "trimRight" => Some(s.trim_end().to_string()),
                            _ => None,
                        };
                        if let Some(r) = result {
                            return Ok(Value::String(r));
                        }
                    }
                }
            }
        }

        // --- classnames / clsx ---
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Ident(ident) = callee_expr.as_ref() {
                let fn_name = ident.sym.to_string();
                let module = self.modules.get(module_spec);
                let is_classnames = module
                    .and_then(|m| m.imports.get(&fn_name))
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
            }
        }

        // Unknown call — return Null gracefully
        Ok(Value::Null)
    }

    fn eval_closure(
        &self,
        module_spec: &str,
        expr: &Expr,
        arg: &Value,
        index: usize,
        parent_env: &HashMap<String, Value>,
    ) -> Result<Value> {
        match expr {
            Expr::Arrow(arrow) => {
                let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
                let mut env = parent_env.clone();
                // Also expose the index as second argument if there's a second param
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                match &*arrow.body {
                    BlockStmtOrExpr::BlockStmt(block) => {
                        self.eval_body_stmts(module_spec, &block.stmts, &mut env)
                            .map(Value::String)
                    }
                    BlockStmtOrExpr::Expr(body_expr) => {
                        self.eval_expr(module_spec, body_expr, &env)
                    }
                }
            }
            Expr::Fn(fn_expr) => {
                let params: Vec<ParamBinding> =
                    fn_expr.function.params.iter().map(|p| param_from_pat(&p.pat)).collect();
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
        stmts: &[Stmt],
        env: &mut HashMap<String, Value>,
    ) -> Result<String> {
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
        var: &VarDecl,
        env: &mut HashMap<String, Value>,
    ) {
        for decl in &var.decls {
            let value = if let Some(init) = &decl.init {
                self.eval_expr(module_spec, init, env).unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            apply_var_pat_to_env(&decl.name, value, env);
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
                        // Null and Bool(false) render as nothing — same as React
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
    Ok(ComponentFunction {
        params,
        body_stmts: body.stmts.clone(),
    })
}

fn function_from_arrow(arrow: &ArrowExpr) -> Result<ComponentFunction> {
    let params = arrow.params.iter().map(param_from_pat).collect();
    let body_stmts = match &*arrow.body {
        BlockStmtOrExpr::BlockStmt(block) => block.stmts.clone(),
        BlockStmtOrExpr::Expr(expr) => vec![Stmt::Return(ReturnStmt {
            span: Default::default(),
            arg: Some(Box::new((**expr).clone())),
        })],
    };
    Ok(ComponentFunction { params, body_stmts })
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

fn apply_var_pat_to_env(pat: &Pat, value: Value, env: &mut HashMap<String, Value>) {
    match pat {
        Pat::Ident(binding) => {
            env.insert(binding.id.sym.to_string(), value);
        }
        Pat::Array(array_pat) => {
            for (i, elem) in array_pat.elems.iter().enumerate() {
                if let Some(elem_pat) = elem {
                    let elem_val = match &value {
                        Value::Array(arr) => arr.get(i).cloned().unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    apply_var_pat_to_env(elem_pat, elem_val, env);
                }
            }
        }
        Pat::Object(object_pat) => {
            let map = value.as_object().cloned().unwrap_or_default();
            for prop in &object_pat.props {
                match prop {
                    ObjectPatProp::Assign(assign) => {
                        let key = assign.key.sym.to_string();
                        let val = map.get(&key).cloned().unwrap_or(Value::Null);
                        env.insert(key, val);
                    }
                    ObjectPatProp::KeyValue(kv) => {
                        if let Some(key) = prop_name_to_string(&kv.key) {
                            let val = map.get(&key).cloned().unwrap_or(Value::Null);
                            apply_var_pat_to_env(&kv.value, val, env);
                        }
                    }
                    ObjectPatProp::Rest(_) => {}
                }
            }
        }
        _ => {}
    }
}

// Bind closure params positionally from a Value::Array of arguments
fn bind_params_positional(params: &[ParamBinding], args: &Value, env: &mut HashMap<String, Value>) {
    let arr = args.as_array().cloned().unwrap_or_default();
    for (i, param) in params.iter().enumerate() {
        let val = arr.get(i).cloned().unwrap_or(Value::Null);
        match param {
            ParamBinding::Ident(name) => {
                env.insert(name.clone(), val);
            }
            ParamBinding::Object(fields) => {
                let map = val.as_object().cloned().unwrap_or_default();
                for (key, local) in fields {
                    env.insert(local.clone(), map.get(key).cloned().unwrap_or(Value::Null));
                }
            }
            ParamBinding::Ignore => {}
        }
    }
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

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn classnames_collect(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::String(s) if !s.is_empty() => {
            // A single string may itself be space-separated classes
            out.push(s.clone());
        }
        Value::Array(arr) => {
            for item in arr {
                classnames_collect(item, out);
            }
        }
        Value::Object(map) => {
            for (key, flag) in map {
                if is_truthy(flag) {
                    out.push(key.clone());
                }
            }
        }
        _ => {}
    }
}

fn is_classnames_source(source: &str) -> bool {
    matches!(source, "classnames" | "clsx")
        || source.ends_with("/classnames")
        || source.ends_with("/clsx")
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

    #[test]
    fn test_ternary_classname() {
        let project = ComponentProject::load_from_dir(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-app").join("src").join("components")
        ).unwrap();
        let source = r#"
            export default function Badge({ active }) {
                return <span className={active ? "badge--active" : "badge--inactive"}>{active ? "On" : "Off"}</span>;
            }
        "#;
        let mut modules = project.modules.clone();
        let path = std::path::PathBuf::from("Badge.tsx");
        let parsed = super::parse_module(source, &path).unwrap();
        modules.insert("Badge.tsx".to_string(), parsed);
        let p = ComponentProject { root: project.root.clone(), modules };
        let props = serde_json::json!({ "active": true });
        let html = p.render_entry("Badge.tsx", &props).unwrap();
        assert!(html.contains("badge--active"));
        assert!(html.contains("On"));
    }

    #[test]
    fn test_template_literal_classname() {
        let source = r#"
            export default function Card({ variant }) {
                return <div className={`card card--${variant}`}>hello</div>;
            }
        "#;
        let path = std::path::PathBuf::from("Card.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("Card.tsx".to_string(), module);
        let props = serde_json::json!({ "variant": "primary" });
        let html = base.render_entry("Card.tsx", &props).unwrap();
        assert!(html.contains("card card--primary"));
    }

    #[test]
    fn test_logical_and_short_circuit() {
        let source = r#"
            export default function Alert({ show, message }) {
                return <div>{show && <span>{message}</span>}</div>;
            }
        "#;
        let path = std::path::PathBuf::from("Alert.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("Alert.tsx".to_string(), module);

        let html_shown = base.render_entry("Alert.tsx", &serde_json::json!({ "show": true, "message": "hello" })).unwrap();
        assert!(html_shown.contains("<span>hello</span>"));

        let html_hidden = base.render_entry("Alert.tsx", &serde_json::json!({ "show": false, "message": "hello" })).unwrap();
        assert!(!html_hidden.contains("<span>"));
    }

    #[test]
    fn test_const_binding_in_function_body() {
        let source = r#"
            export default function Label({ kind }) {
                const cls = kind === "primary" ? "label-primary" : "label-default";
                return <span className={cls}>{kind}</span>;
            }
        "#;
        let path = std::path::PathBuf::from("Label.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("Label.tsx".to_string(), module);
        let props = serde_json::json!({ "kind": "primary" });
        let html = base.render_entry("Label.tsx", &props).unwrap();
        assert!(html.contains("label-primary"));
    }

    #[test]
    fn test_array_map_renders_list() {
        let source = r#"
            export default function List({ items }) {
                return <ul>{items.map(item => <li>{item}</li>)}</ul>;
            }
        "#;
        let path = std::path::PathBuf::from("List.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("List.tsx".to_string(), module);
        let props = serde_json::json!({ "items": ["alpha", "beta", "gamma"] });
        let html = base.render_entry("List.tsx", &props).unwrap();
        assert!(html.contains("<li>alpha</li>"));
        assert!(html.contains("<li>beta</li>"));
        assert!(html.contains("<li>gamma</li>"));
    }

    #[test]
    fn test_hooks_in_body_do_not_crash() {
        // useState and useEffect calls should be silently ignored (evaluate to Null)
        let source = r#"
            export default function Counter() {
                const [count, setCount] = useState(0);
                return <div className="counter">{count}</div>;
            }
        "#;
        let path = std::path::PathBuf::from("Counter.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("Counter.tsx".to_string(), module);
        let html = base.render_entry("Counter.tsx", &serde_json::json!({})).unwrap();
        assert!(html.contains("class=\"counter\""));
    }

    #[test]
    fn test_classnames_call() {
        let source = r#"
            import cx from 'classnames';
            export default function Button({ primary, disabled }) {
                return <button className={cx("btn", { "btn--primary": primary, "btn--disabled": disabled })}>click</button>;
            }
        "#;
        let path = std::path::PathBuf::from("Button.tsx");
        let module = super::parse_module(source, &path).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app").join("src").join("components");
        let mut base = ComponentProject::load_from_dir(&root).unwrap();
        base.modules.insert("Button.tsx".to_string(), module);
        let props = serde_json::json!({ "primary": true, "disabled": false });
        let html = base.render_entry("Button.tsx", &props).unwrap();
        assert!(html.contains("btn"));
        assert!(html.contains("btn--primary"));
        assert!(!html.contains("btn--disabled"));
    }
}
