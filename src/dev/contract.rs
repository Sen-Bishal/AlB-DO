use crate::runtime::hot_set::HOT_SET_MAX;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{
    Callee, Expr, ExprOrSpread, Lit, Module, ModuleDecl, ModuleItem, Prop, PropName, PropOrSpread,
    UnaryOp,
};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};

pub const DEV_CONFIG_JSON: &str = "albedo.config.json";
pub const DEV_CONFIG_TS: &str = "albedo.config.ts";
pub const DEV_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevConfig {
    #[serde(default = "default_contract_version")]
    pub contract_version: u16,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub server: DevServerConfig,
    #[serde(default)]
    pub watch: DevWatchConfig,
    #[serde(default)]
    pub hmr: DevHmrConfig,
    #[serde(default)]
    pub hot_set: Vec<HotSetRegistration>,
    #[serde(default)]
    pub static_slice: StaticSliceConfig,
    /// Map of URL path → entry component filename.
    /// e.g. { "/analytics": "Analytics.tsx", "/settings": "Settings.tsx" }
    /// The root entry ("/") is always served from `entry`.
    #[serde(default)]
    pub routes: HashMap<String, String>,
}

impl Default for DevConfig {
    fn default() -> Self {
        Self {
            contract_version: DEV_CONTRACT_VERSION,
            root: None,
            entry: None,
            server: DevServerConfig::default(),
            watch: DevWatchConfig::default(),
            hmr: DevHmrConfig::default(),
            hot_set: Vec::new(),
            static_slice: StaticSliceConfig::default(),
            routes: HashMap::new(),
        }
    }
}

impl DevConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract_version != DEV_CONTRACT_VERSION {
            return Err(format!(
                "unsupported contract_version '{}' (expected {})",
                self.contract_version, DEV_CONTRACT_VERSION
            ));
        }

        if let Some(root) = &self.root {
            if root.trim().is_empty() {
                return Err("root must not be empty when set".to_string());
            }
        }

        if let Some(entry) = &self.entry {
            validate_entry_module(entry)?;
        }

        self.server.validate()?;
        self.watch.validate()?;
        self.hmr.validate()?;
        self.static_slice.validate()?;
        validate_hot_set(&self.hot_set)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for DevServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

impl DevServerConfig {
    fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("server.host must not be empty".to_string());
        }
        self.host
            .parse::<IpAddr>()
            .map_err(|err| format!("server.host must be a valid IP address: {err}"))?;
        if self.port == 0 {
            return Err("server.port must be > 0".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevWatchConfig {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Default for DevWatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(),
            ignore: Vec::new(),
        }
    }
}

impl DevWatchConfig {
    fn validate(&self) -> Result<(), String> {
        if self.debounce_ms == 0 {
            return Err("watch.debounce_ms must be > 0".to_string());
        }
        if self.debounce_ms > 5000 {
            return Err("watch.debounce_ms must be <= 5000".to_string());
        }
        for pattern in &self.ignore {
            if pattern.trim().is_empty() {
                return Err("watch.ignore must not contain empty patterns".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevHmrConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub transport: HmrTransport,
}

impl Default for DevHmrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: HmrTransport::default(),
        }
    }
}

impl DevHmrConfig {
    fn validate(&self) -> Result<(), String> {
        if !self.enabled && self.transport != HmrTransport::Sse {
            return Err(
                "hmr.transport can only differ from default when hmr.enabled=true".to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HmrTransport {
    Sse,
    WebSocket,
}

impl Default for HmrTransport {
    fn default() -> Self {
        Self::Sse
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HotSetRegistration {
    pub component: String,
    #[serde(default)]
    pub priority: HotSetPriority,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HotSetPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Default for HotSetPriority {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StaticSliceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub opt_out: Vec<String>,
}

impl Default for StaticSliceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            opt_out: Vec::new(),
        }
    }
}

impl StaticSliceConfig {
    fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        for component in &self.opt_out {
            let trimmed = component.trim();
            if trimmed.is_empty() {
                return Err(
                    "static_slice.opt_out must not contain empty component names".to_string(),
                );
            }
            if !seen.insert(trimmed.to_string()) {
                return Err(format!(
                    "static_slice.opt_out contains duplicate component '{}'",
                    trimmed
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevCliOptions {
    pub root_override: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub entry_override: Option<String>,
    pub host_override: Option<String>,
    pub port_override: Option<u16>,
    pub no_hmr: bool,
    pub open: bool,
    pub strict: bool,
    pub verbose: bool,
    pub print_contract: bool,
}

impl Default for DevCliOptions {
    fn default() -> Self {
        Self {
            root_override: None,
            config_path: None,
            entry_override: None,
            host_override: None,
            port_override: None,
            no_hmr: false,
            open: false,
            strict: false,
            verbose: false,
            print_contract: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedDevContract {
    pub contract_version: u16,
    pub project_dir: PathBuf,
    pub config_path: Option<PathBuf>,
    pub root: PathBuf,
    pub entry: String,
    pub server: DevServerConfig,
    pub watch: DevWatchConfig,
    pub hmr: DevHmrConfig,
    pub hot_set: Vec<HotSetRegistration>,
    pub static_slice: StaticSliceConfig,
    pub strict: bool,
    pub verbose: bool,
    pub open: bool,
    pub routes: HashMap<String, String>,
}

pub fn parse_dev_cli_args(raw_args: &[String]) -> Result<DevCliOptions, String> {
    let mut options = DevCliOptions::default();
    let mut idx = 0usize;

    if let Some(first) = raw_args.first() {
        if !first.starts_with('-') {
            options.root_override = Some(PathBuf::from(first));
            idx = 1;
        }
    }

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--config" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --config".to_string())?;
                options.config_path = Some(PathBuf::from(value));
            }
            "--entry" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --entry".to_string())?;
                validate_entry_module(value)?;
                options.entry_override = Some(value.clone());
            }
            "--host" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --host".to_string())?;
                if value.trim().is_empty() {
                    return Err("--host must not be empty".to_string());
                }
                options.host_override = Some(value.clone());
            }
            "--port" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --port".to_string())?;
                let port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port '{value}'"))?;
                if port == 0 {
                    return Err("--port must be > 0".to_string());
                }
                options.port_override = Some(port);
            }
            "--no-hmr" => {
                options.no_hmr = true;
            }
            "--open" => {
                options.open = true;
            }
            "--strict" => {
                options.strict = true;
            }
            "--verbose" | "-v" => {
                options.verbose = true;
            }
            "--print-contract" => {
                options.print_contract = true;
            }
            unknown => {
                return Err(format!("unknown dev option '{unknown}'"));
            }
        }
        idx += 1;
    }

    Ok(options)
}

pub fn resolve_dev_contract(
    raw_args: &[String],
    cwd: &Path,
) -> Result<ResolvedDevContract, String> {
    let cli = parse_dev_cli_args(raw_args)?;
    let loaded = load_dev_config(cwd, cli.config_path.as_deref())?;
    let mut config = loaded.config;
    config.validate()?;

    let project_dir = loaded.project_dir;
    let root_input = if let Some(root) = cli.root_override {
        root
    } else if let Some(root) = config.root.take() {
        PathBuf::from(root)
    } else {
        PathBuf::from(default_root())
    };

    let root = if root_input.is_absolute() {
        root_input
    } else {
        project_dir.join(root_input)
    };

    if !root.exists() {
        return Err(format!("dev root '{}' does not exist", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("dev root '{}' is not a directory", root.display()));
    }

    if let Some(host) = cli.host_override {
        config.server.host = host;
    }
    if let Some(port) = cli.port_override {
        config.server.port = port;
    }
    if cli.no_hmr {
        config.hmr.enabled = false;
        config.hmr.transport = HmrTransport::Sse;
    }

    config.validate()?;

    let entry = if let Some(entry) = cli.entry_override {
        entry
    } else if let Some(entry) = config.entry.take() {
        validate_entry_module(&entry)?;
        entry
    } else {
        detect_default_entry_module(&root).ok_or_else(|| {
            format!(
                "no entry module found in '{}'; pass --entry <FILE> or set 'entry' in {}",
                root.display(),
                loaded
                    .config_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| DEV_CONFIG_JSON.to_string())
            )
        })?
    };

    let entry_path = root.join(&entry);
    if !entry_path.is_file() {
        return Err(format!(
            "entry module '{}' does not exist under root '{}'",
            entry,
            root.display()
        ));
    }

    Ok(ResolvedDevContract {
        contract_version: config.contract_version,
        project_dir,
        config_path: loaded.config_path,
        root,
        entry,
        server: config.server,
        watch: config.watch,
        hmr: config.hmr,
        hot_set: config.hot_set,
        static_slice: config.static_slice,
        strict: cli.strict,
        verbose: cli.verbose,
        open: cli.open,
        routes: config.routes,
    })
}

fn detect_default_entry_module(root: &Path) -> Option<String> {
    for candidate in ["App.tsx", "App.jsx", "App.ts", "App.js"] {
        if root.join(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn validate_hot_set(hot_set: &[HotSetRegistration]) -> Result<(), String> {
    if hot_set.len() > HOT_SET_MAX {
        return Err(format!(
            "hot_set has {} entries; max supported is {}",
            hot_set.len(),
            HOT_SET_MAX
        ));
    }

    let mut seen = HashSet::new();
    for entry in hot_set {
        let name = entry.component.trim();
        if name.is_empty() {
            return Err("hot_set.component must not be empty".to_string());
        }
        if !seen.insert(name.to_string()) {
            return Err(format!(
                "hot_set contains duplicate component '{}'",
                entry.component
            ));
        }
    }

    Ok(())
}

fn validate_entry_module(entry: &str) -> Result<(), String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err("entry must not be empty".to_string());
    }
    let ext = Path::new(trimmed)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    if !matches!(ext, "tsx" | "ts" | "jsx" | "js") {
        return Err(format!(
            "entry '{entry}' must end with .tsx, .ts, .jsx, or .js"
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct LoadedDevConfig {
    config: DevConfig,
    project_dir: PathBuf,
    config_path: Option<PathBuf>,
}

fn load_dev_config(cwd: &Path, explicit_path: Option<&Path>) -> Result<LoadedDevConfig, String> {
    if let Some(path) = explicit_path {
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let project_dir = full_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf());
        let config = parse_dev_config_file(&full_path)?;
        return Ok(LoadedDevConfig {
            config,
            project_dir,
            config_path: Some(full_path),
        });
    }

    let json_path = cwd.join(DEV_CONFIG_JSON);
    let ts_path = cwd.join(DEV_CONFIG_TS);

    let has_json = json_path.is_file();
    let has_ts = ts_path.is_file();

    if has_json && has_ts {
        return Err(format!(
            "both '{}' and '{}' exist; keep one or pass --config",
            json_path.display(),
            ts_path.display()
        ));
    }

    if has_json {
        let config = parse_dev_config_file(&json_path)?;
        return Ok(LoadedDevConfig {
            config,
            project_dir: cwd.to_path_buf(),
            config_path: Some(json_path),
        });
    }

    if has_ts {
        let config = parse_dev_config_file(&ts_path)?;
        return Ok(LoadedDevConfig {
            config,
            project_dir: cwd.to_path_buf(),
            config_path: Some(ts_path),
        });
    }

    Ok(LoadedDevConfig {
        config: DevConfig::default(),
        project_dir: cwd.to_path_buf(),
        config_path: None,
    })
}

fn parse_dev_config_file(path: &Path) -> Result<DevConfig, String> {
    if !path.is_file() {
        return Err(format!("config file '{}' does not exist", path.display()));
    }

    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read config '{}': {err}", path.display()))?;

    match extension.as_str() {
        "json" => {
            let config = serde_json::from_str::<DevConfig>(&contents).map_err(|err| {
                format!("failed to parse JSON config '{}': {err}", path.display())
            })?;
            config.validate()?;
            Ok(config)
        }
        "ts" => {
            let value = parse_typescript_default_export_to_json(path, &contents)?;
            let config = serde_json::from_value::<DevConfig>(value).map_err(|err| {
                format!(
                    "failed to decode TypeScript config '{}' into contract shape: {err}",
                    path.display()
                )
            })?;
            config.validate()?;
            Ok(config)
        }
        _ => Err(format!(
            "unsupported config extension '.{}'; use .json or .ts",
            extension
        )),
    }
}

fn parse_typescript_default_export_to_json(path: &Path, source: &str) -> Result<Value, String> {
    let module = parse_ts_module(path, source)?;
    let expr = find_default_export_expr(path, &module)?;
    expr_to_json(path, &expr)
}

fn parse_ts_module(path: &Path, source: &str) -> Result<Module, String> {
    let source_map: Lrc<SourceMap> = Default::default();
    let source_file = source_map.new_source_file(
        FileName::Custom(path.display().to_string()).into(),
        source.to_string(),
    );
    let mut parser = Parser::new(
        Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: true,
            ..Default::default()
        }),
        StringInput::from(&*source_file),
        None,
    );
    parser.parse_module().map_err(|err| {
        format!(
            "failed to parse TypeScript config '{}': {:?}",
            path.display(),
            err
        )
    })
}

fn find_default_export_expr(path: &Path, module: &Module) -> Result<Expr, String> {
    let mut default_export: Option<Expr> = None;

    for item in &module.body {
        match item {
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(export_expr)) => {
                default_export = Some((*export_expr.expr).clone());
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(_)) => {
                return Err(format!(
                    "config '{}' must export an object expression (or defineConfig(object))",
                    path.display()
                ));
            }
            _ => {}
        }
    }

    default_export.ok_or_else(|| {
        format!(
            "config '{}' must contain `export default {{ ... }}`",
            path.display()
        )
    })
}

fn expr_to_json(path: &Path, expr: &Expr) -> Result<Value, String> {
    match expr {
        Expr::Object(object) => object_to_json(path, object.props.as_slice()),
        Expr::Array(array) => array_to_json(path, array.elems.as_slice()),
        Expr::Lit(lit) => lit_to_json(path, lit),
        Expr::Paren(paren) => expr_to_json(path, &paren.expr),
        Expr::TsAs(ts_as) => expr_to_json(path, &ts_as.expr),
        Expr::TsSatisfies(ts_sat) => expr_to_json(path, &ts_sat.expr),
        Expr::Call(call) => call_to_json(path, call),
        Expr::Unary(unary) => unary_to_json(path, unary.op, &unary.arg),
        Expr::Tpl(template) => {
            if template.exprs.is_empty() {
                let mut out = String::new();
                for quasi in &template.quasis {
                    out.push_str(quasi.raw.as_ref());
                }
                Ok(Value::String(out))
            } else {
                Err(format!(
                    "unsupported template string expression in '{}' (dynamic interpolation not allowed)",
                    path.display()
                ))
            }
        }
        _ => Err(format!(
            "unsupported expression in '{}'; config must be static object/array/literal values",
            path.display()
        )),
    }
}

fn unary_to_json(path: &Path, op: UnaryOp, arg: &Expr) -> Result<Value, String> {
    match op {
        UnaryOp::Minus => {
            let numeric = expr_to_json(path, arg)?;
            let Some(value) = numeric.as_f64() else {
                return Err(format!(
                    "unsupported unary '-' expression in '{}'; expected a numeric literal",
                    path.display()
                ));
            };
            number_to_json(path, -value)
        }
        UnaryOp::Plus => expr_to_json(path, arg),
        _ => Err(format!(
            "unsupported unary operator in '{}'; only + and - are supported",
            path.display()
        )),
    }
}

fn call_to_json(path: &Path, call: &swc_ecma_ast::CallExpr) -> Result<Value, String> {
    let is_define_config = match &call.callee {
        Callee::Expr(expr) => {
            matches!(expr.as_ref(), Expr::Ident(ident) if ident.sym == *"defineConfig")
        }
        _ => false,
    };

    if !is_define_config {
        return Err(format!(
            "unsupported call expression in '{}'; only defineConfig(...) is allowed",
            path.display()
        ));
    }

    if call.args.len() != 1 {
        return Err(format!(
            "defineConfig(...) in '{}' must receive exactly one argument",
            path.display()
        ));
    }

    let arg = call.args.first().ok_or_else(|| {
        format!(
            "defineConfig(...) in '{}' is missing the configuration argument",
            path.display()
        )
    })?;

    if arg.spread.is_some() {
        return Err(format!(
            "defineConfig(...) in '{}' does not support spread arguments",
            path.display()
        ));
    }

    expr_to_json(path, &arg.expr)
}

fn array_to_json(path: &Path, elems: &[Option<ExprOrSpread>]) -> Result<Value, String> {
    let mut out = Vec::new();
    for element in elems {
        let Some(expr) = element else {
            out.push(Value::Null);
            continue;
        };
        if expr.spread.is_some() {
            return Err(format!(
                "spread elements are not supported in array literals for '{}'",
                path.display()
            ));
        }
        out.push(expr_to_json(path, &expr.expr)?);
    }
    Ok(Value::Array(out))
}

fn object_to_json(path: &Path, props: &[PropOrSpread]) -> Result<Value, String> {
    let mut map = Map::new();
    for prop in props {
        match prop {
            PropOrSpread::Spread(_) => {
                return Err(format!(
                    "object spread is not supported in config '{}'",
                    path.display()
                ));
            }
            PropOrSpread::Prop(prop) => match prop.as_ref() {
                Prop::KeyValue(kv) => {
                    let key = prop_name_to_string(path, &kv.key)?;
                    let value = expr_to_json(path, &kv.value)?;
                    map.insert(key, value);
                }
                _ => {
                    return Err(format!(
                        "unsupported object property in '{}'; use key-value pairs only",
                        path.display()
                    ));
                }
            },
        }
    }
    Ok(Value::Object(map))
}

fn prop_name_to_string(path: &Path, name: &PropName) -> Result<String, String> {
    match name {
        PropName::Ident(ident) => Ok(ident.sym.to_string()),
        PropName::Str(string) => Ok(string.value.to_string()),
        PropName::Num(num) => Ok(num.value.to_string()),
        PropName::Computed(_) => Err(format!(
            "computed property names are not supported in '{}'",
            path.display()
        )),
        PropName::BigInt(_) => Err(format!(
            "bigint property names are not supported in '{}'",
            path.display()
        )),
    }
}

fn lit_to_json(path: &Path, lit: &Lit) -> Result<Value, String> {
    match lit {
        Lit::Str(string) => Ok(Value::String(string.value.to_string())),
        Lit::Bool(boolean) => Ok(Value::Bool(boolean.value)),
        Lit::Num(number) => number_to_json(path, number.value),
        Lit::Null(_) => Ok(Value::Null),
        _ => Err(format!(
            "unsupported literal value in '{}'; use string/number/boolean/null",
            path.display()
        )),
    }
}

fn number_to_json(path: &Path, value: f64) -> Result<Value, String> {
    if value.fract() == 0.0 {
        let as_i64 = value as i64;
        if (as_i64 as f64 - value).abs() < f64::EPSILON {
            return Ok(Value::Number(serde_json::Number::from(as_i64)));
        }
    }

    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| format!("invalid number literal in '{}'", path.display()))
}

const fn default_contract_version() -> u16 {
    DEV_CONTRACT_VERSION
}

const fn default_port() -> u16 {
    3000
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

const fn default_debounce_ms() -> u64 {
    75
}

const fn default_true() -> bool {
    true
}

fn default_root() -> &'static str {
    "src/components"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dev_cli_args_defaults() {
        let options = parse_dev_cli_args(&[]).unwrap();
        assert_eq!(options, DevCliOptions::default());
    }

    #[test]
    fn test_parse_dev_cli_args_with_overrides() {
        let args = vec![
            "test-app/src/components".to_string(),
            "--entry".to_string(),
            "App.tsx".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "4173".to_string(),
            "--no-hmr".to_string(),
            "--open".to_string(),
            "--strict".to_string(),
            "--verbose".to_string(),
            "--print-contract".to_string(),
        ];
        let options = parse_dev_cli_args(&args).unwrap();
        assert_eq!(
            options.root_override,
            Some(PathBuf::from("test-app/src/components"))
        );
        assert_eq!(options.entry_override.as_deref(), Some("App.tsx"));
        assert_eq!(options.host_override.as_deref(), Some("127.0.0.1"));
        assert_eq!(options.port_override, Some(4173));
        assert!(options.no_hmr);
        assert!(options.open);
        assert!(options.strict);
        assert!(options.verbose);
        assert!(options.print_contract);
    }

    #[test]
    fn test_parse_json_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DEV_CONFIG_JSON);
        std::fs::write(
            &path,
            r#"{
  "contract_version": 1,
  "root": "test-app/src/components",
  "entry": "App.jsx",
  "server": { "host": "127.0.0.1", "port": 4010 },
  "watch": { "debounce_ms": 100, "ignore": ["**/*.snap"] },
  "hmr": { "enabled": true, "transport": "sse" },
  "hot_set": [{ "component": "PriceTicker", "priority": "critical" }],
  "static_slice": { "enabled": true, "opt_out": ["DynamicWidget"] }
}"#,
        )
        .unwrap();

        let config = parse_dev_config_file(&path).unwrap();
        assert_eq!(config.root.as_deref(), Some("test-app/src/components"));
        assert_eq!(config.entry.as_deref(), Some("App.jsx"));
        assert_eq!(config.server.port, 4010);
        assert_eq!(config.hot_set.len(), 1);
    }

    #[test]
    fn test_parse_ts_config_file_with_define_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DEV_CONFIG_TS);
        std::fs::write(
            &path,
            r#"
export default defineConfig({
  contract_version: 1,
  root: "src/components",
  entry: "App.tsx",
  server: { host: "127.0.0.1", port: 3005 },
  hmr: { enabled: true, transport: "web_socket" },
  hot_set: [{ component: "LiveChart", priority: "high" }],
  static_slice: { enabled: true, opt_out: ["LiveChart"] }
});
"#,
        )
        .unwrap();

        let config = parse_dev_config_file(&path).unwrap();
        assert_eq!(config.entry.as_deref(), Some("App.tsx"));
        assert_eq!(config.server.port, 3005);
        assert_eq!(config.hmr.transport, HmrTransport::WebSocket);
        assert_eq!(config.hot_set[0].component, "LiveChart");
    }

    #[test]
    fn test_resolve_dev_contract_uses_config_and_cli_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src").join("components");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("App.tsx"),
            "export default function App(){return null;}",
        )
        .unwrap();
        std::fs::write(
            temp.path().join(DEV_CONFIG_JSON),
            r#"{
  "contract_version": 1,
  "root": "src/components",
  "server": { "host": "127.0.0.1", "port": 3000 }
}"#,
        )
        .unwrap();

        let args = vec![
            "--port".to_string(),
            "4999".to_string(),
            "--strict".to_string(),
            "--open".to_string(),
        ];
        let resolved = resolve_dev_contract(&args, temp.path()).unwrap();
        assert_eq!(resolved.root, root);
        assert_eq!(resolved.entry, "App.tsx");
        assert_eq!(resolved.server.port, 4999);
        assert!(resolved.strict);
        assert!(resolved.open);
    }

    #[test]
    fn test_load_dev_config_errors_when_json_and_ts_both_exist() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(DEV_CONFIG_JSON), "{}").unwrap();
        std::fs::write(temp.path().join(DEV_CONFIG_TS), "export default {};").unwrap();
        let err = load_dev_config(temp.path(), None).unwrap_err();
        assert!(err.contains("both"));
    }
}
