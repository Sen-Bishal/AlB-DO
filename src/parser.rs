use crate::effects::EffectProfile;
use crate::types::*;
use std::path::Path;
use std::rc::Rc;
use swc_common::SourceMap;
use swc_ecma_ast::*;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

pub struct ComponentParser {
    source_map: Rc<SourceMap>,
}

impl ComponentParser {
    pub fn new() -> Self {
        Self {
            source_map: Rc::new(SourceMap::default()),
        }
    }

    pub fn parse_file(&self, path: &Path) -> Result<Vec<ParsedComponent>> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CompilerError::AnalysisFailed(format!("Failed to read file: {}", e)))?;

        self.parse_source(&content, path.to_str().unwrap_or("unknown"))
    }

    pub fn parse_source(&self, source: &str, filename: &str) -> Result<Vec<ParsedComponent>> {
        let source_file = self.source_map.new_source_file(
            swc_common::FileName::Custom(filename.to_string()).into(),
            source.to_string(),
        );

        let syntax = if filename.ends_with(".tsx") || filename.ends_with(".ts") {
            Syntax::Typescript(TsSyntax {
                tsx: filename.ends_with(".tsx"),
                decorators: true,
                ..Default::default()
            })
        } else {
            Syntax::Es(EsSyntax {
                jsx: true,
                decorators: true,
                ..Default::default()
            })
        };

        let input = StringInput::from(&*source_file);
        let mut parser = Parser::new(syntax, input, None);

        let module = parser
            .parse_module()
            .map_err(|e| CompilerError::AnalysisFailed(format!("Parse error: {:?}", e)))?;

        let source_hash = hash_source(source);
        let mut visitor = ComponentVisitor::new(filename.to_string(), source_hash);
        module.visit_with(&mut visitor);

        Ok(visitor.components)
    }
}

impl Default for ComponentParser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ParsedComponent {
    pub name: String,
    pub file_path: String,
    pub line_number: usize,
    pub imports: Vec<String>,
    pub estimated_size: usize,
    pub is_default_export: bool,
    pub props: Vec<String>,
    pub effect_profile: EffectProfile,
    pub source_hash: u64,
}

struct ComponentVisitor {
    file_path: String,
    source_hash: u64,
    components: Vec<ParsedComponent>,
    current_imports: Vec<String>,
}

impl ComponentVisitor {
    fn new(file_path: String, source_hash: u64) -> Self {
        Self {
            file_path,
            source_hash,
            components: Vec::new(),
            current_imports: Vec::new(),
        }
    }

    fn extract_component_name(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(ident) => Some(ident.sym.to_string()),
            Expr::Arrow(_) => Some(format!("ArrowComponent_{}", self.components.len())),
            Expr::Fn(func) => func.ident.as_ref().map(|i| i.sym.to_string()),
            _ => None,
        }
    }

    fn effect_profile_from_function(&self, function: &Function) -> EffectProfile {
        let mut collector = EffectCollector::default();
        if function.is_async {
            collector.profile.asynchronous = true;
        }
        if let Some(body) = &function.body {
            body.visit_with(&mut collector);
        }
        collector.profile
    }

    fn effect_profile_from_arrow(&self, arrow: &ArrowExpr) -> EffectProfile {
        let mut collector = EffectCollector::default();
        if arrow.is_async {
            collector.profile.asynchronous = true;
        }
        match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => block.visit_with(&mut collector),
            BlockStmtOrExpr::Expr(expr) => expr.visit_with(&mut collector),
        }
        collector.profile
    }

    fn effect_profile_from_expr(&self, expr: &Expr) -> EffectProfile {
        match expr {
            Expr::Arrow(arrow) => self.effect_profile_from_arrow(arrow),
            Expr::Fn(function) => self.effect_profile_from_function(&function.function),
            _ => EffectProfile::default(),
        }
    }

    fn push_component(
        &mut self,
        name: String,
        estimated_size: usize,
        is_default_export: bool,
        effect_profile: EffectProfile,
    ) {
        self.components.push(ParsedComponent {
            name,
            file_path: self.file_path.clone(),
            line_number: 0,
            imports: self.current_imports.clone(),
            estimated_size,
            is_default_export,
            props: Vec::new(),
            effect_profile,
            source_hash: self.source_hash,
        });
    }
}

impl Visit for ComponentVisitor {
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        for spec in &import.specifiers {
            let name = match spec {
                ImportSpecifier::Named(n) => n.local.sym.to_string(),
                ImportSpecifier::Default(d) => d.local.sym.to_string(),
                ImportSpecifier::Namespace(n) => n.local.sym.to_string(),
            };
            self.current_imports.push(name);
        }
    }

    fn visit_fn_decl(&mut self, func: &FnDecl) {
        let name = func.ident.sym.to_string();

        if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            let estimated_size = name.len() * 50 + 200;
            let effect_profile = self.effect_profile_from_function(&func.function);
            self.push_component(name, estimated_size, false, effect_profile);
        }
    }

    fn visit_var_decl(&mut self, var: &VarDecl) {
        for decl in &var.decls {
            if let Some(name) = decl.name.as_ident().map(|i| i.sym.to_string()) {
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    if let Some(init) = &decl.init {
                        let is_component = matches!(&**init, Expr::Arrow(_) | Expr::Fn(_));
                        if is_component {
                            let estimated_size = name.len() * 50 + 200;
                            let effect_profile = self.effect_profile_from_expr(init);
                            self.push_component(name, estimated_size, false, effect_profile);
                        }
                    }
                }
            }
        }
    }

    fn visit_export_default_decl(&mut self, export: &ExportDefaultDecl) {
        if let DefaultDecl::Fn(func) = &export.decl {
            if let Some(ident) = &func.ident {
                let name = ident.sym.to_string();
                let estimated_size = name.len() * 50 + 300;
                let effect_profile = self.effect_profile_from_function(&func.function);
                self.push_component(name, estimated_size, true, effect_profile);
            }
        }
    }

    fn visit_export_default_expr(&mut self, export: &ExportDefaultExpr) {
        if let Some(name) = self.extract_component_name(&export.expr) {
            let estimated_size = name.len() * 50 + 200;
            let effect_profile = self.effect_profile_from_expr(&export.expr);

            if let Some(comp) = self.components.iter_mut().find(|c| c.name == name) {
                comp.is_default_export = true;
                comp.effect_profile = comp.effect_profile.join(effect_profile);
            } else {
                self.push_component(name, estimated_size, true, effect_profile);
            }
        }
    }
}

#[derive(Default)]
struct EffectCollector {
    profile: EffectProfile,
}

impl EffectCollector {
    fn mark_call(&mut self, call_name: &str) {
        let name = call_name.trim();
        if is_hook_call(name) {
            self.profile.hooks = true;
        }
        if is_io_call(name) {
            self.profile.io = true;
            self.profile.asynchronous = true;
        }
        if is_async_call(name) {
            self.profile.asynchronous = true;
        }
        if is_side_effect_call(name) {
            self.profile.side_effects = true;
        }
    }
}

impl Visit for EffectCollector {
    fn visit_await_expr(&mut self, await_expr: &AwaitExpr) {
        self.profile.asynchronous = true;
        await_expr.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Some(name) = callee_name(&call.callee) {
            self.mark_call(&name);
        }
        call.visit_children_with(self);
    }
}

fn hash_source(source: &str) -> u64 {
    // FNV-1a 64-bit — matches stable_source_hash in engine.rs.
    // DefaultHasher must NOT be used here: it is not stable across Rust
    // versions or process restarts, which would corrupt the incremental cache.
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn callee_name(callee: &Callee) -> Option<String> {
    match callee {
        Callee::Expr(expr) => expr_name(expr),
        Callee::Super(_) | Callee::Import(_) => None,
    }
}

fn expr_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(ident) => Some(ident.sym.to_string()),
        Expr::Member(member) => member_name(member),
        _ => None,
    }
}

fn member_name(member: &MemberExpr) -> Option<String> {
    let object = expr_name(&member.obj)?;
    let property = match &member.prop {
        MemberProp::Ident(ident) => ident.sym.to_string(),
        MemberProp::Computed(computed) => expr_name(&computed.expr)?,
        MemberProp::PrivateName(_) => return None,
    };
    Some(format!("{object}.{property}"))
}

fn is_hook_call(name: &str) -> bool {
    if !name.starts_with("use") || name.len() <= 3 {
        return false;
    }
    name.chars().nth(3).is_some_and(|ch| ch.is_uppercase())
}

fn is_async_call(name: &str) -> bool {
    const ASYNC_CALLS: &[&str] = &[
        "fetch",
        "Promise.all",
        "Promise.race",
        "Promise.resolve",
        "setTimeout",
        "queueMicrotask",
    ];
    ASYNC_CALLS.iter().any(|candidate| *candidate == name)
}

fn is_io_call(name: &str) -> bool {
    const IO_CALLS: &[&str] = &[
        "fetch",
        "axios",
        "axios.get",
        "axios.post",
        "fs.readFile",
        "fs.readFileSync",
        "fs.writeFile",
        "fs.writeFileSync",
        "http.get",
        "http.request",
        "https.get",
        "https.request",
        "localStorage.getItem",
        "localStorage.setItem",
        "sessionStorage.getItem",
        "sessionStorage.setItem",
    ];
    IO_CALLS.iter().any(|candidate| *candidate == name)
}

fn is_side_effect_call(name: &str) -> bool {
    const SIDE_EFFECT_CALLS: &[&str] = &[
        "console.log",
        "console.info",
        "console.warn",
        "console.error",
        "document.write",
        "localStorage.setItem",
        "sessionStorage.setItem",
        "history.pushState",
        "window.location.assign",
    ];
    SIDE_EFFECT_CALLS.iter().any(|candidate| *candidate == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_component() {
        let parser = ComponentParser::new();
        let source = r#"
            function Button() {
                return <button>Click</button>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "Button");
    }

    #[test]
    fn test_parse_arrow_component() {
        let parser = ComponentParser::new();
        let source = r#"
            const Header = () => {
                return <header>Title</header>;
            };
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "Header");
    }

    #[test]
    fn test_parse_with_imports() {
        let parser = ComponentParser::new();
        let source = r#"
            import React from 'react';
            import Button from './Button';
            
            function App() {
                return <div><Button /></div>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert!(components[0].imports.contains(&"Button".to_string()));
    }

    #[test]
    fn test_parse_detects_effects() {
        let parser = ComponentParser::new();
        let source = r#"
            export default async function App() {
                const [count] = useState(0);
                const response = await fetch('/api/data');
                console.log(response, count);
                return <main>{count}</main>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        let component = &components[0];
        assert!(component.effect_profile.hooks);
        assert!(component.effect_profile.asynchronous);
        assert!(component.effect_profile.io);
        assert!(component.effect_profile.side_effects);
    }

    #[test]
    fn test_parse_produces_stable_source_hash() {
        let parser = ComponentParser::new();
        let source = "export default function App(){return <main/>;}";
        let first = parser.parse_source(source, "test.jsx").unwrap();
        let second = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(first[0].source_hash, second[0].source_hash);
    }
}
