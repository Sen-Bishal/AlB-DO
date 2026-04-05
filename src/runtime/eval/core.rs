use crate::types::ComponentId;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::runtime::eval::component::{
    classnames_collect, escape_html, fnv1a_hash, import_candidates, is_classnames_source,
    is_component_module, is_component_tag, is_truthy, is_void_tag, lit_to_value,
    normalize_jsx_text, normalize_slashes, normalize_specifier, prop_name_to_string, render_attrs,
    value_to_string,
};
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
            _ => Ok(Value::Null),
        }
    }

    fn eval_member(
        &self,
        module_spec: &str,
        member: &swc_ecma_ast::MemberExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
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
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Member(member) = callee_expr.as_ref() {
                if let MemberProp::Ident(prop_ident) = &member.prop {
                    let method = prop_ident.sym.as_ref();
                    let obj_val = self.eval_expr(module_spec, &member.obj, env)?;

                    if method == "map" {
                        if let Value::Array(items) = obj_val {
                            if let Some(ExprOrSpread {
                                expr: mapper,
                                spread: None,
                            }) = call.args.first()
                            {
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

        Ok(Value::Null)
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
