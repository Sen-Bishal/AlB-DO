use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};

use crate::runtime::eval::component::prop_name_to_string;

#[derive(Debug, Clone)]
pub struct ImportBinding {
    pub source: String,
    pub export_name: String,
}

#[derive(Debug, Clone)]
pub enum ParamBinding {
    Ident(String),
    Object(Vec<(String, String)>),
    Ignore,
}

#[derive(Debug, Clone)]
pub struct ComponentFunction {
    pub params: Vec<ParamBinding>,
    pub body_stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct ParsedModule {
    pub imports: HashMap<String, ImportBinding>,
    pub functions: HashMap<String, ComponentFunction>,
    pub default_export: Option<String>,
}

pub fn parse_module(source: &str, file_path: &Path) -> Result<ParsedModule> {
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

pub fn apply_var_pat_to_env(pat: &Pat, value: Value, env: &mut HashMap<String, Value>) {
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

pub fn bind_params_positional(
    params: &[ParamBinding],
    args: &Value,
    env: &mut HashMap<String, Value>,
) {
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

pub fn param_from_pat(pat: &Pat) -> ParamBinding {
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

pub fn bind_params(params: &[ParamBinding], props: &Value, env: &mut HashMap<String, Value>) {
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
