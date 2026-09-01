use crate::runtime::hot_set::HOT_SET_MAX;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// FORGE collections this app declares — the `forge` block, keyed by
    /// collection name (which is both the `useSharedSlot` topic and, by default,
    /// the table). Empty means "no declaration", and the runtime falls back to
    /// the built-in walking-skeleton guestbook.
    ///
    /// A `BTreeMap` so the declaration order that reaches
    /// [`ForgeSchema::from_declarations`](crate::forge::ForgeSchema::from_declarations)
    /// is stable across builds.
    #[serde(default)]
    pub forge: BTreeMap<String, crate::forge::CollectionDecl>,
    /// APERTURE · external sources this app declares — the `sources` block,
    /// keyed by the name the author calls (`github.repo(…)`).
    ///
    /// A `BTreeMap` for the same reason `forge` is one: the lowering order that
    /// reaches
    /// [`SourceRegistry::from_declarations`](crate::aperture::SourceRegistry::from_declarations)
    /// must be identical on every machine and every build, because the declared
    /// hosts it yields become the egress allowlist.
    #[serde(default)]
    pub sources: BTreeMap<String, crate::aperture::SourceDecl>,
    /// AUTH · where principals come from — the `auth` block.
    ///
    /// The third sibling of `forge` and `sources`, and carried the same way. An
    /// absent block is not a misconfiguration: it means every request is
    /// anonymous, which is what an app without login wants.
    #[serde(default)]
    pub auth: crate::auth::AuthDeclaration,
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
            forge: BTreeMap::new(),
            sources: BTreeMap::new(),
            auth: crate::auth::AuthDeclaration::default(),
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

        // APERTURE · structural validation of the `sources` block, so a typo in
        // a path template or a missing `scope` fails the build rather than
        // surfacing at serve time as a route that reads nothing.
        //
        // The env lookup is deliberately permissive here: whether `GITHUB_TOKEN`
        // is set is a property of the machine, not of the config, and `albedo
        // build` on a laptop must not fail because a production secret is
        // absent. Boot re-lowers with the real environment, where a missing
        // variable is genuinely fatal.
        if !self.sources.is_empty() {
            crate::aperture::SourceRegistry::from_declarations(&self.sources, |_| {
                Some(String::new())
            })
            .map_err(|err| format!("invalid `sources` block: {err}"))?;
        }
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
    /// The **project** directory — where `albedo.config.*` is looked for and
    /// where the source layout is detected. This is what the positional `[dir]`
    /// on `albedo dev|build|ship|doctor` means.
    ///
    /// 🔴 It used to set [`Self::root_override`] instead, which is a different
    /// directory: the *source root* (`src/`), resolved relative to the project.
    /// The two coincide only for a flat layout, so on the scaffold the CLI
    /// generates, `albedo build .` set the root to the project directory,
    /// suppressed layout detection (a declared root is authoritative), and
    /// failed with "entry module 'routes/index.tsx' does not exist under root".
    /// `albedo build ../other-app` was worse: it read the config from the
    /// *current* directory and looked for sources in the other one.
    pub project_dir_override: Option<PathBuf>,
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
            project_dir_override: None,
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
    /// Which convention in [`SOURCE_LAYOUTS`] supplied `root` and `entry`, when
    /// the author declared neither. `None` means they were declared, and nothing
    /// was guessed.
    ///
    /// 🔴 **Carried so a foreign layout cannot be adopted silently.** ALBEDO
    /// discovers routes from `<root>/routes` only, so matching, say, Next's App
    /// Router finds one entry module and none of that router's pages — a project
    /// that builds but describes a fraction of itself. Naming the match is what
    /// lets every lane say so; a boolean "was detected" could not.
    #[serde(default)]
    pub detected_layout: Option<String>,
    pub server: DevServerConfig,
    pub watch: DevWatchConfig,
    pub hmr: DevHmrConfig,
    pub hot_set: Vec<HotSetRegistration>,
    pub static_slice: StaticSliceConfig,
    pub strict: bool,
    pub verbose: bool,
    pub open: bool,
    pub routes: HashMap<String, String>,
    /// A4 · per-URL layout chain (outermost → leaf), each entry a
    /// `root`-relative module path (`routes/layout.tsx`,
    /// `routes/blog/layout.tsx`, …). The dev render loop composes
    /// these around the leaf route's HTML so `albedo dev` shows the
    /// same nav/footer shell `albedo serve` composes at build time —
    /// without this dev and prod render structurally different
    /// documents. Empty for routes with no `layout.tsx` in their path.
    #[serde(default)]
    pub route_layouts: HashMap<String, Vec<String>>,
    /// The app's declared FORGE collections, carried through from
    /// [`DevConfig::forge`]. Empty means the app declared none, and the boot
    /// path falls back to the built-in guestbook default.
    #[serde(default)]
    pub forge: BTreeMap<String, crate::forge::CollectionDecl>,
    /// APERTURE · the app's declared sources, carried through from
    /// [`DevConfig::sources`]. Empty means the app declared none, and no
    /// outbound host is allowlisted.
    #[serde(default)]
    pub sources: BTreeMap<String, crate::aperture::SourceDecl>,
    /// AUTH · the app's declared providers, carried through from
    /// [`DevConfig::auth`]. Empty `providers` means every request is anonymous.
    #[serde(default)]
    pub auth: crate::auth::AuthDeclaration,
}

pub fn parse_dev_cli_args(raw_args: &[String]) -> Result<DevCliOptions, String> {
    let mut options = DevCliOptions::default();
    let mut idx = 0usize;

    if let Some(first) = raw_args.first() {
        if !first.starts_with('-') {
            options.project_dir_override = Some(PathBuf::from(first));
            idx = 1;
        }
    }

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--root" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --root".to_string())?;
                options.root_override = Some(PathBuf::from(value));
            }
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
    // The positional `[dir]` selects the project *before* anything is read, so
    // the config, the layout detection and the output directory all agree on
    // which project this is. Resolving it here rather than threading it
    // downstream is what keeps them from disagreeing.
    let cwd = match &cli.project_dir_override {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => cwd.join(dir),
        None => cwd.to_path_buf(),
    };
    let cwd = cwd.as_path();
    let loaded = load_dev_config(cwd, cli.config_path.as_deref())?;
    let mut config = loaded.config;
    config.validate()?;

    let project_dir = loaded.project_dir;
    let declared_root = cli
        .root_override
        .clone()
        .or_else(|| config.root.take().map(PathBuf::from));

    // Detected only when the author declared no root — a declared one is
    // authoritative and must not be second-guessed by a heuristic. Computed
    // before the entry because the two are one fact: see `SourceLayout`.
    let detected = if declared_root.is_none() && cli.entry_override.is_none() && config.entry.is_none()
    {
        detect_source_layout(&project_dir)
    } else {
        None
    };

    let root_input = match (&declared_root, &detected) {
        (Some(root), _) => root.clone(),
        (None, Some(layout)) => layout.root.clone(),
        // No layout matched and nothing was declared: fall back to `src`, then
        // to the project directory. The second rung matters for every framework
        // that puts its sources at the top level — without it the failure is
        // "dev root does not exist", which points at a directory the author
        // never asked for.
        (None, None) => {
            if project_dir.join(default_root()).is_dir() {
                PathBuf::from(default_root())
            } else {
                PathBuf::from(".")
            }
        }
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
    } else if let Some(layout) = &detected {
        layout.entry.clone()
    } else {
        detect_default_entry_module(&root).ok_or_else(|| {
            format!(
                "no entry module found in '{}', and none of the layouts ALBEDO recognises \
                 ({}) matched '{}'. Pass --entry <FILE> or set 'entry' in {}",
                root.display(),
                SOURCE_LAYOUTS
                    .iter()
                    .map(|layout| layout.name)
                    .collect::<Vec<_>>()
                    .join(", "),
                project_dir.display(),
                loaded
                    .config_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| DEV_CONFIG_JSON.to_string())
            )
        })?
    };

    // 🪤 An entry is root-relative, and the root is frequently NOT the directory
    // the author is standing in — `src/` by default, `app/` for Remix. So the
    // path that is true on disk (`src/app/page.tsx`) is the one that fails, and
    // the message used to insist a file the author can plainly see does not
    // exist. Accept the project-relative spelling when it lands inside the root
    // and rewrite it, rather than making a person compute the difference.
    let entry = match resolve_entry_path(&project_dir, &root, &entry) {
        Some((rewritten, _)) => rewritten,
        None => {
            return Err(format!(
                "entry module '{}' does not exist under root '{}' (also tried it relative to \
                 the project directory '{}')",
                entry,
                root.display(),
                project_dir.display()
            ));
        }
    };

    // Phase N · file-based routing. `<root>/routes/` is the convention;
    // when it exists, every `*.tsx` / `*.jsx` / `*.ts` / `*.js` becomes
    // a route automatically. Discovered routes overlay on top of any
    // config-declared routes (file-based wins on URL conflict — the
    // expected behaviour for convention-over-configuration).
    let mut routes = config.routes;
    let mut route_layouts: HashMap<String, Vec<String>> = HashMap::new();
    let mut entry_override_from_routes: Option<String> = None;
    let routes_dir = root.join(crate::routing::ROUTES_DIRNAME);
    if routes_dir.is_dir() {
        let discovery = crate::routing::discover_routes(&routes_dir).map_err(|err| {
            format!(
                "file-based routing failed under '{}': {err}",
                routes_dir.display()
            )
        })?;
        for route in discovery.routes {
            // Entry paths are stored relative to `root` so the dev
            // render loop (which calls `project.render_entry(entry)`)
            // resolves them through the same code path as config-
            // declared routes.
            let entry_rel = format!(
                "{}/{}",
                crate::routing::ROUTES_DIRNAME,
                route.source_rel_path.to_string_lossy().replace('\\', "/")
            );
            // A4 · capture the route's layout chain (outermost → leaf)
            // as `root`-relative module paths so the dev server can
            // compose the same layouts prod does. `discover_routes`
            // already orders the chain root-down.
            if !route.layout_chain.is_empty() {
                let chain = route
                    .layout_chain
                    .iter()
                    .map(|rel| {
                        format!(
                            "{}/{}",
                            crate::routing::ROUTES_DIRNAME,
                            rel.to_string_lossy().replace('\\', "/")
                        )
                    })
                    .collect::<Vec<_>>();
                route_layouts.insert(route.url_path.clone(), chain);
            }
            if route.url_path == "/" {
                // An `index.tsx` under `routes/` overrides the dev
                // contract's `entry`. This is what makes the user's
                // file at `src/routes/index.tsx` actually render at
                // `/` without them having to edit the config.
                entry_override_from_routes = Some(entry_rel);
            } else {
                routes.insert(route.url_path, entry_rel);
            }
        }
    }

    let entry = entry_override_from_routes.unwrap_or(entry);
    // Re-validate the (possibly overridden) entry exists under root.
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
        detected_layout: detected.map(|layout| layout.name.to_string()),
        server: config.server,
        watch: config.watch,
        hmr: config.hmr,
        hot_set: config.hot_set,
        static_slice: config.static_slice,
        strict: cli.strict,
        verbose: cli.verbose,
        open: cli.open,
        routes,
        route_layouts,
        forge: config.forge,
        sources: config.sources,
        auth: config.auth,
    })
}

/// A source layout ALBEDO knows how to find its way into.
///
/// 🔑 **Root and entry are detected together, because they are one fact.** A
/// Next project created with `--src-dir` puts its router under `src/app`; the
/// same project without it puts the router at the top level and has no `src/` at
/// all. Resolving the root first and *then* hunting for an entry inside it
/// cannot express that — it fails on the second shape before it ever looks for
/// an entry. So a convention names both halves.
///
/// Only JSX/TSX layouts appear here, and that is a boundary rather than an
/// omission: this is a JSX compiler, so Svelte, Vue and Astro projects are not
/// half-supported, they are out of scope.
#[derive(Debug, Clone, Copy)]
pub struct SourceLayout {
    /// What to call it when telling the author what was detected.
    pub name: &'static str,
    /// Source root, relative to the project directory. `""` is the project
    /// directory itself.
    pub root: &'static str,
    /// Entry candidates relative to [`Self::root`], in preference order.
    pub entries: &'static [&'static str],
}

/// The layouts probed when a project declares neither `root` nor `entry`.
///
/// **Order is the specification.** ALBEDO's own convention is tried first and
/// unconditionally, so nothing here can ever take precedence over a real ALBEDO
/// project that happens to also carry an `app/` directory. Everything after it is
/// a foreign layout, tried most-specific-first.
///
/// 🔴 **Detecting a foreign layout finds an entry, not a working port.** ALBEDO
/// discovers *routes* from `<root>/routes` only, so a Next project resolves one
/// entry module and none of its router's pages. That is why detection reports
/// which convention it matched — see [`ResolvedDevContract::detected_layout`] —
/// rather than quietly proceeding as though the project were understood.
pub const SOURCE_LAYOUTS: &[SourceLayout] = &[
    // ALBEDO · Phase N+ file-based routing, then the pre-Phase-N `App.tsx`.
    SourceLayout {
        name: "ALBEDO",
        root: "src",
        entries: &[
            "routes/index.tsx",
            "routes/index.jsx",
            "routes/index.ts",
            "routes/index.js",
            "App.tsx",
            "App.jsx",
            "App.ts",
            "App.js",
        ],
    },
    // ALBEDO with the sources at the top level rather than under `src/`.
    SourceLayout {
        name: "ALBEDO (flat)",
        root: "",
        entries: &["routes/index.tsx", "routes/index.jsx", "App.tsx", "App.jsx"],
    },
    // Next.js · App Router. `page` is the route file; `layout` is checked too
    // because a route group can push the first `page` deeper than the top level.
    SourceLayout {
        name: "Next.js App Router (src/)",
        root: "src",
        entries: &["app/page.tsx", "app/page.jsx", "app/layout.tsx"],
    },
    SourceLayout {
        name: "Next.js App Router",
        root: "",
        entries: &["app/page.tsx", "app/page.jsx", "app/layout.tsx"],
    },
    // Next.js · Pages Router.
    SourceLayout {
        name: "Next.js Pages Router (src/)",
        root: "src",
        entries: &["pages/index.tsx", "pages/index.jsx", "pages/_app.tsx"],
    },
    SourceLayout {
        name: "Next.js Pages Router",
        root: "",
        entries: &["pages/index.tsx", "pages/index.jsx", "pages/_app.tsx"],
    },
    // Remix, and React Router v7 in framework mode — same shape, same file.
    SourceLayout {
        name: "Remix / React Router",
        root: "app",
        entries: &["root.tsx", "root.jsx"],
    },
    // Expo Router — file-based, and the closest cousin to ALBEDO's own shape.
    SourceLayout {
        name: "Expo Router",
        root: "app",
        entries: &["index.tsx", "index.jsx", "_layout.tsx"],
    },
    // Vite, Create React App, and every hand-rolled bundler setup. Last,
    // because `src/main.tsx` and `src/index.tsx` are generic enough that a
    // framework project may also contain one.
    SourceLayout {
        name: "Vite / CRA",
        root: "src",
        entries: &[
            "main.tsx",
            "main.jsx",
            "index.tsx",
            "index.jsx",
            "App.tsx",
            "App.jsx",
        ],
    },
];

/// A layout that matched a real project on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLayout {
    /// The matched convention's name, for reporting.
    pub name: &'static str,
    /// Absolute source root.
    pub root: PathBuf,
    /// Entry module, relative to [`Self::root`].
    pub entry: String,
}

/// Find the source root and entry of a project that declared neither.
///
/// Returns the first convention in [`SOURCE_LAYOUTS`] whose root exists and one
/// of whose entries is a file. `None` means no known layout matched, which the
/// caller turns into an error that names what was looked for — a bare "no entry
/// module found" tells an author nothing about how to fix it.
#[must_use]
pub fn detect_source_layout(project_dir: &Path) -> Option<DetectedLayout> {
    for layout in SOURCE_LAYOUTS {
        let root = if layout.root.is_empty() {
            project_dir.to_path_buf()
        } else {
            project_dir.join(layout.root)
        };
        if !root.is_dir() {
            continue;
        }
        for entry in layout.entries {
            if root.join(entry).is_file() {
                return Some(DetectedLayout {
                    name: layout.name,
                    root,
                    entry: (*entry).to_string(),
                });
            }
        }
    }
    None
}

/// Resolve an entry spelling to `(root-relative entry, absolute path)`.
///
/// Tries the entry as root-relative first — that is what it means — and falls
/// back to project-relative, accepting it only when the file actually lands
/// inside the root. The fallback cannot smuggle a module in from outside the
/// source tree, because a path that does not sit under the root is rejected the
/// same as one that does not exist.
#[allow(clippy::type_complexity)]
fn resolve_entry_path(project_dir: &Path, root: &Path, entry: &str) -> Option<(String, PathBuf)> {
    let direct = root.join(entry);
    if direct.is_file() {
        return Some((entry.to_string(), direct));
    }

    let from_project = project_dir.join(entry);
    if !from_project.is_file() {
        return None;
    }
    // Compared through `canonicalize` so `src/../src/app/page.tsx` and a
    // symlinked root both answer the same question the filesystem would.
    let canonical_root = root.canonicalize().ok()?;
    let canonical_entry = from_project.canonicalize().ok()?;
    let relative = canonical_entry.strip_prefix(&canonical_root).ok()?;
    Some((
        relative.to_string_lossy().replace('\\', "/"),
        from_project,
    ))
}

/// Find an entry inside a root the author *did* declare.
///
/// Every convention's candidates are tried, in table order, because a declared
/// root says where the sources are and not which framework put them there.
fn detect_default_entry_module(root: &Path) -> Option<String> {
    for layout in SOURCE_LAYOUTS {
        for candidate in layout.entries {
            if root.join(candidate).is_file() {
                return Some((*candidate).to_string());
            }
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

/// The `forge` block of the project at `project_dir`, and nothing else.
///
/// Exists so `albedo init` can emit `.albedo/forge.d.ts` from the config it just
/// wrote without standing up a whole dev contract (which wants a resolvable
/// root, an entry, CLI overrides — none of which apply while scaffolding). Reads
/// through the same parser the dev and serve paths use, so the types a fresh
/// project gets are the types its first build would have produced.
///
/// # Errors
/// Propagates a config that cannot be found or parsed.
pub fn load_forge_declarations(
    project_dir: &Path,
) -> Result<BTreeMap<String, crate::forge::CollectionDecl>, String> {
    Ok(load_dev_config(project_dir, None)?.config.forge)
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
    // Phase N+ — file-based routes live under `src/routes/`, so the
    // dev contract's source root is `src/` (not `src/components/`,
    // which was the pre-Phase-N convention). Existing projects that
    // hand-set `root` in `albedo.config.ts` keep their override.
    "src"
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
            options.project_dir_override,
            Some(PathBuf::from("test-app/src/components"))
        );
        assert_eq!(
            options.root_override, None,
            "the positional argument is the PROJECT directory; the source root is `--root`"
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
    fn the_source_root_is_still_overridable_but_by_its_own_flag() {
        let options = parse_dev_cli_args(&["--root".to_string(), "app".to_string()]).unwrap();
        assert_eq!(options.root_override, Some(PathBuf::from("app")));
        assert_eq!(options.project_dir_override, None);
    }

    /// 🔴 Regression: `albedo build .` failed on the scaffold the CLI itself
    /// generates.
    ///
    /// The positional `[dir]` was wired to the **source root**, not the project
    /// directory. On a `src/`-layout project that meant `.` declared the root to
    /// be the project directory — and a declared root is authoritative, so
    /// layout detection was suppressed and the entry `routes/index.tsx` was
    /// looked for at the top level, where it is not.
    ///
    /// Four commands advertise `[dir]` (`dev`, `build`, `ship`, `doctor`) and
    /// none of them worked with an argument.
    #[test]
    fn a_dot_project_directory_resolves_the_same_as_no_argument() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        std::fs::create_dir_all(project.join("src").join("routes")).unwrap();
        std::fs::write(
            project.join("src").join("routes").join("index.tsx"),
            "export default function Index() { return <b>hi</b>; }",
        )
        .unwrap();

        let bare = resolve_dev_contract(&[], project).expect("no argument resolves");
        let dot = resolve_dev_contract(&[".".to_string()], project).expect("`.` resolves");
        assert_eq!(bare.root, dot.root, "`.` must not move the source root");
        assert_eq!(bare.entry, dot.entry);
    }

    /// The other half of the same bug, and the worse one: naming a sibling
    /// project read the config from the *current* directory while looking for
    /// sources in the other, so it could not work for any real project.
    #[test]
    fn naming_a_sibling_project_resolves_that_projects_own_layout() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("strangerapp");
        std::fs::create_dir_all(project.join("src").join("routes")).unwrap();
        std::fs::write(
            project.join("src").join("routes").join("index.tsx"),
            "export default function Index() { return <b>hi</b>; }",
        )
        .unwrap();

        let from_parent = resolve_dev_contract(&["strangerapp".to_string()], temp.path())
            .expect("a named sibling project resolves");
        let from_inside = resolve_dev_contract(&[], &project).expect("resolves from inside");
        assert_eq!(from_parent.root, from_inside.root);
        assert_eq!(from_parent.entry, from_inside.entry);
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

    /// Write a component file, creating parents.
    fn place(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("relative path has a parent")).unwrap();
        std::fs::write(&path, "export default function P(){return null;}").unwrap();
    }

    /// Every layout in the table, matched against the shape it names.
    ///
    /// Written as one table-driven test rather than nine, because the property
    /// under test is the *table* — that each convention resolves, and that none
    /// of them shadows a project belonging to another.
    #[test]
    fn every_known_layout_resolves_its_own_shape() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (label, file to place, expected layout name, expected entry)
            ("albedo", "src/routes/index.tsx", "ALBEDO", "routes/index.tsx"),
            ("albedo-legacy", "src/App.tsx", "ALBEDO", "App.tsx"),
            ("albedo-flat", "routes/index.tsx", "ALBEDO (flat)", "routes/index.tsx"),
            (
                "next-app-src",
                "src/app/page.tsx",
                "Next.js App Router (src/)",
                "app/page.tsx",
            ),
            ("next-app", "app/page.tsx", "Next.js App Router", "app/page.tsx"),
            (
                "next-pages-src",
                "src/pages/index.tsx",
                "Next.js Pages Router (src/)",
                "pages/index.tsx",
            ),
            (
                "next-pages",
                "pages/index.tsx",
                "Next.js Pages Router",
                "pages/index.tsx",
            ),
            ("remix", "app/root.tsx", "Remix / React Router", "root.tsx"),
            ("expo", "app/index.tsx", "Expo Router", "index.tsx"),
            ("vite", "src/main.tsx", "Vite / CRA", "main.tsx"),
            ("cra", "src/index.tsx", "Vite / CRA", "index.tsx"),
        ];

        for (label, file, expected_name, expected_entry) in cases {
            let temp = tempfile::tempdir().unwrap();
            place(temp.path(), file);
            let detected = detect_source_layout(temp.path())
                .unwrap_or_else(|| panic!("{label}: no layout matched for '{file}'"));
            assert_eq!(detected.name, *expected_name, "{label}: wrong layout");
            assert_eq!(detected.entry, *expected_entry, "{label}: wrong entry");
        }
    }

    /// 🔑 **ALBEDO's own convention wins outright.** A project that is ours and
    /// also happens to carry an `app/` directory must never be read as somebody
    /// else's — the table's order is the specification, and this pins it.
    #[test]
    fn an_albedo_project_is_never_mistaken_for_a_foreign_one() {
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), "src/routes/index.tsx");
        place(temp.path(), "src/app/page.tsx");
        place(temp.path(), "app/root.tsx");

        let detected = detect_source_layout(temp.path()).expect("a layout matched");
        assert_eq!(detected.name, "ALBEDO");
        assert_eq!(detected.entry, "routes/index.tsx");
    }

    /// Nothing recognisable must stay `None` rather than guessing at the first
    /// `.tsx` it trips over — a wrong entry produces a build that is confidently
    /// about the wrong thing.
    #[test]
    fn an_unrecognised_project_matches_nothing() {
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), "lib/deep/thing.tsx");
        assert!(detect_source_layout(temp.path()).is_none());
    }

    /// 🪤 The papercut this fixes: with a `src` root, the path that is true on
    /// disk (`src/app/page.tsx`) was the one rejected, and the error insisted a
    /// visible file did not exist.
    #[test]
    fn an_entry_may_be_spelled_relative_to_the_project_or_the_root() {
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), "src/app/page.tsx");
        let root = temp.path().join("src");

        let (root_relative, _) =
            resolve_entry_path(temp.path(), &root, "app/page.tsx").expect("root-relative");
        assert_eq!(root_relative, "app/page.tsx");

        let (rewritten, _) =
            resolve_entry_path(temp.path(), &root, "src/app/page.tsx").expect("project-relative");
        assert_eq!(
            rewritten, "app/page.tsx",
            "the project-relative spelling should be rewritten, not rejected"
        );
    }

    /// The fallback must not become a hole: a file outside the source root is
    /// still refused, however it is spelled.
    #[test]
    fn an_entry_outside_the_root_is_still_refused() {
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), "src/app/page.tsx");
        place(temp.path(), "elsewhere/sneaky.tsx");
        let root = temp.path().join("src");

        assert!(resolve_entry_path(temp.path(), &root, "elsewhere/sneaky.tsx").is_none());
    }

    #[test]
    fn file_based_routes_overlay_into_dev_contract_alongside_config_routes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src");
        std::fs::create_dir_all(root.join("routes").join("blog")).unwrap();
        std::fs::write(
            root.join("App.tsx"),
            "export default function App(){return null;}",
        )
        .unwrap();
        std::fs::write(
            root.join("routes").join("index.tsx"),
            "export default function Home(){return null;}",
        )
        .unwrap();
        std::fs::write(
            root.join("routes").join("about.tsx"),
            "export default function About(){return null;}",
        )
        .unwrap();
        std::fs::write(
            root.join("routes").join("blog").join("[slug].tsx"),
            "export default function Post(){return null;}",
        )
        .unwrap();

        // Config also declares a route — must coexist with the
        // file-based discoveries.
        std::fs::write(
            temp.path().join(DEV_CONFIG_JSON),
            r#"{
  "contract_version": 1,
  "root": "src",
  "entry": "App.tsx",
  "routes": { "/legacy": "App.tsx" }
}"#,
        )
        .unwrap();

        let resolved = resolve_dev_contract(&[], temp.path()).unwrap();

        // `routes/index.tsx` overrides the configured entry so the
        // file-based default actually paints at `/`.
        assert_eq!(resolved.entry, "routes/index.tsx");

        // Other discovered routes show up in the route map alongside
        // the config-declared `/legacy`.
        assert_eq!(
            resolved.routes.get("/about").map(String::as_str),
            Some("routes/about.tsx"),
        );
        assert_eq!(
            resolved.routes.get("/blog/[slug]").map(String::as_str),
            Some("routes/blog/[slug].tsx"),
        );
        assert_eq!(
            resolved.routes.get("/legacy").map(String::as_str),
            Some("App.tsx"),
        );
    }

    #[test]
    fn missing_routes_dir_falls_back_to_config_routes_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src");
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
  "root": "src",
  "entry": "App.tsx",
  "routes": { "/health": "App.tsx" }
}"#,
        )
        .unwrap();

        let resolved = resolve_dev_contract(&[], temp.path()).unwrap();
        assert_eq!(resolved.entry, "App.tsx");
        assert_eq!(resolved.routes.len(), 1);
        assert!(resolved.routes.contains_key("/health"));
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
