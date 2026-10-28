//! `albedo ship --target static` — the pages `albedo serve` sends, as files.
//!
//! 🔴 This target used to print `static export ready` and point at
//! `.albedo/dist`: build manifests, `_albedo/*.js` and a `public/` mirror, and
//! **not one HTML file**, because the server assembles every page per request.
//! Found 2026-09-30 on a 17-component, all-Tier-A site — `find .albedo/dist
//! -name '*.html'` came back empty. The success line was the whole export; a
//! folder uploaded to a static host on its word served no pages at all.
//!
//! ## 🔑 A page is exported by asking the server for it
//!
//! Every route is fetched through [`AlbedoServer::router`] — the stack `serve`
//! binds, the same boot, one in-process `GET` each — and its body is the file.
//! Nothing here re-assembles a shell out of the manifest. A second assembler
//! would be a second renderer, free to drift from the one visitors actually
//! get, and the drift would show up only on a host nobody here is watching.
//!
//! The one byte-level change is the CSRF bootstrap. `serve` mints a token per
//! visitor session; a file is served to every visitor, and the registry that
//! token belongs to dies with this process — so the export writes it empty,
//! which is also what makes two exports of one build byte-identical.
//!
//! ## What cannot be a file is refused by name — all of it, before a byte lands
//!
//! A static host answers a URL with a file and does nothing else, so anything
//! that needs the server to *do* something at request time cannot be exported:
//!
//! * a dynamic segment (`/room/[id]`) — nothing lists the values it takes;
//! * `export const auth = "required"` — nothing can check who is asking;
//! * a form bound to an action — nothing receives the POST;
//! * a route that opens the live lane — Tier-B/C components, shared-slot
//!   topics, partitions (the server's own `route_needs_live_lane` decides);
//! * an app that declares FORGE, AUTH, sources, uploads, webhooks, email,
//!   `src/middleware.ts` or `src/jobs.ts`.
//!
//! Partial exports are refused too. Shipping the routes that happen to work
//! and dropping the rest is the same lie as before at a smaller scale — a site
//! whose nav links into 404s — so the whole list is printed and nothing is
//! written. `--target binary` serves every route, and the refusal says so.

use crate::boot::{boot_production_server, ProductionServerOptions};
use crate::handlers::albedo_assets::EMBEDDED_ASSET_PATHS;
use crate::handlers::streaming::{csrf_bootstrap_script, route_needs_live_lane};
use crate::render::csrf::ALBEDO_SESSION_COOKIE;
use crate::renderer_runtime::RENDER_MANIFEST_FILENAME;
use crate::server::AlbedoServer;
use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use dom_render_compiler::manifest::schema::{RenderManifestV2, RouteAuth, RouteManifest};
use dom_render_compiler::runtime::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tower::ServiceExt;

/// A page is one streamed document; anything past this is not a page.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// How long one request may sit behind SHUTTER before the export gives up.
///
/// The export is one client asking for every URL in a burst, which is exactly
/// what a rate limiter exists to slow down — and it should: the export runs the
/// same stack `serve` does, admission control included. So a `429` is waited
/// out rather than bypassed. Past the default burst a large site pays a few
/// hundred milliseconds a page; this bounds a limiter that never admits.
const RATE_LIMIT_PATIENCE: Duration = Duration::from_secs(120);

/// Pause between attempts on a `429`. SHUTTER's `Retry-After` is whole seconds,
/// which rounds the next free unit (a fraction of a second at the default
/// read rate) up to a full one — five times slower than the budget allows.
const RATE_LIMIT_STEP: Duration = Duration::from_millis(200);

/// Where `albedo ship --target static` writes: `<project>/.albedo/static`.
///
/// Under `.albedo/` because the directory is **replaced wholesale** on every
/// export — a page deleted from the app must not keep shipping from the last
/// run — and that is only safe in a directory the framework owns.
pub fn default_out_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".albedo").join("static")
}

/// One thing in the app a file cannot be, with every reason why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// A route as authored (`/room/[id]`), or the app-level surface that
    /// declared the server work (`forge`, `src/middleware.ts`).
    pub subject: String,
    /// Each thing about it that needs the server, in the words the author
    /// would search their source for.
    pub reasons: Vec<String>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.subject, self.reasons.join("; "))
    }
}

/// Why an export produced nothing.
#[derive(Debug)]
pub enum ExportError {
    /// Parts of the app need a running server. Every one, not the first — a
    /// list that only ever shows the next problem costs a round trip per item.
    Refused {
        /// App-level surfaces first, then routes in order.
        refusals: Vec<Refusal>,
        /// Set when the list is known to be short of complete, and why: the
        /// server did not boot in this shell, so no page was rendered to look
        /// for forms. Said out loud, because a list that is silently partial
        /// reads as "fix these and it will work".
        unchecked: Option<String>,
    },
    /// The export itself could not run: no build, a boot failure, a write.
    Failed(String),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused {
                refusals,
                unchecked,
            } => {
                write!(
                    f,
                    "static export refused — {} part(s) of this app need a running server:",
                    refusals.len()
                )?;
                for refusal in refusals {
                    write!(f, "\n  - {refusal}")?;
                }
                if let Some(why) = unchecked {
                    write!(f, "\nnot checked: {why}")?;
                }
                write!(
                    f,
                    "\nnothing was written. `albedo ship --target binary` (or docker / fly) \
                     ships the server with the app and serves every route."
                )
            }
            Self::Failed(message) => write!(f, "static export failed: {message}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// One written file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    /// The URL a static host answers with it (`/about`, `/fonts/a.woff2`).
    pub url: String,
    /// Where it landed, relative to the output directory.
    pub path: PathBuf,
    /// Its size on disk.
    pub bytes: u64,
}

/// What an export wrote.
#[derive(Debug, Clone)]
pub struct StaticSite {
    /// The directory to upload.
    pub out_dir: PathBuf,
    /// One per route, in route order: `/about` → `about/index.html`.
    pub pages: Vec<ExportedFile>,
    /// `public/` and the framework's client scripts.
    pub files: Vec<ExportedFile>,
}

impl StaticSite {
    /// Everything written, in bytes.
    pub fn total_bytes(&self) -> u64 {
        self.pages
            .iter()
            .chain(&self.files)
            .map(|file| file.bytes)
            .sum()
    }
}

/// Export a built project to `out_dir`, replacing whatever is there.
///
/// The caller runs the build first — `albedo ship` always has — so
/// `opts.dist_dir` holds a current manifest.
///
/// Must be called outside an async runtime: it boots the server the way
/// `serve` does (synchronously) and then drives it on a runtime of its own.
///
/// # Errors
/// [`ExportError::Refused`] naming every route and declaration a static host
/// cannot serve, or [`ExportError::Failed`] when the export itself breaks.
/// Either way nothing under `out_dir` has changed.
pub fn export_static_site(
    opts: &ProductionServerOptions,
    out_dir: &Path,
) -> Result<StaticSite, ExportError> {
    let manifest = read_manifest(&opts.dist_dir)?;

    // What the declarations and the manifest already say, before paying for a
    // boot: most refusals are visible from here.
    let mut refusals = declaration_refusals(opts);
    let route_refusals = manifest_refusals(&manifest);
    let already_refused: BTreeSet<String> = route_refusals
        .iter()
        .map(|refusal| refusal.subject.clone())
        .collect();
    refusals.extend(route_refusals);

    let server = match boot_production_server(opts) {
        Ok(server) => server,
        // Found on the first real app tried: a `sources` key unset in this
        // shell stopped the boot, and the author was told about an environment
        // variable instead of the routes that could never have been exported.
        // When the answer is already no, the list is the answer.
        Err(err) if !refusals.is_empty() => {
            return Err(ExportError::Refused {
                refusals,
                unchecked: Some(format!(
                    "the server did not boot in this shell ({err}), so no page was rendered \
                     to look for forms, and src/middleware.ts / src/jobs.ts were not read"
                )),
            })
        }
        Err(err) => {
            return Err(ExportError::Failed(format!(
                "the server this export renders through did not boot: {err}"
            )))
        }
    };
    refusals.extend(process_refusals(&server));

    // Every route the manifest did not already rule out is rendered, even when
    // something else has — so the list is complete rather than its first layer.
    // An already-refused route is not requested: a dynamic pattern is not a
    // URL, and asking for `/room/[id]` would render a room called `[id]`.
    let mut routes: Vec<&String> = manifest
        .routes
        .keys()
        .filter(|route| !already_refused.contains(route.as_str()))
        .collect();
    routes.sort();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| ExportError::Failed(format!("failed to start tokio runtime: {err}")))?;
    let (rendered, render_refusals) =
        runtime.block_on(render_through_server(&server, &routes))?;
    drop(server);
    drop(runtime);
    refusals.extend(render_refusals);

    if !refusals.is_empty() {
        // App-level surfaces, then routes in order — the render pass found its
        // routes after the manifest's, and a reader scans for their route.
        refusals.sort_by(|left, right| {
            (left.subject.starts_with('/'), &left.subject)
                .cmp(&(right.subject.starts_with('/'), &right.subject))
        });
        return Err(ExportError::Refused {
            refusals,
            unchecked: None,
        });
    }

    write_site(opts, &rendered, out_dir)
}

/// The app's declared server surfaces. Each is work only a process can do,
/// so an app that declares one has no static form — even if no route used it,
/// dropping it silently would ship a different app than the one written.
pub fn declaration_refusals(opts: &ProductionServerOptions) -> Vec<Refusal> {
    fn names<'a>(keys: impl Iterator<Item = &'a String>) -> String {
        keys.map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    let mut refusals = Vec::new();
    let mut refuse = |subject: &str, reason: String| {
        refusals.push(Refusal {
            subject: subject.to_string(),
            reasons: vec![reason],
        });
    };

    if !opts.forge.is_empty() {
        refuse(
            "forge",
            format!(
                "declares collection(s) {} — rows live in a database the server holds",
                names(opts.forge.keys())
            ),
        );
    }
    if !opts.auth.providers.is_empty() {
        refuse(
            "auth",
            format!(
                "declares provider(s) {} — sign-in writes a session the server keeps",
                names(opts.auth.providers.keys())
            ),
        );
    }
    if !opts.sources.is_empty() {
        refuse(
            "sources",
            format!(
                "declares external source(s) {} — the server fetches and refreshes them",
                names(opts.sources.keys())
            ),
        );
    }
    if !opts.uploads.is_empty() {
        refuse(
            "uploads",
            format!(
                "declares bucket(s) {} — the server receives and stores the files",
                names(opts.uploads.keys())
            ),
        );
    }
    if !opts.webhooks.is_empty() {
        refuse(
            "webhooks",
            format!(
                "declares webhook(s) {} — the server receives and verifies them",
                names(opts.webhooks.keys())
            ),
        );
    }
    if opts.email.is_some() {
        refuse(
            "email",
            "declares an `email` block — the server sends the mail".to_string(),
        );
    }
    refusals
}

/// Routes the manifest already shows cannot be a file, in route order, plus
/// any component whose build-time render failed.
pub fn manifest_refusals(manifest: &RenderManifestV2) -> Vec<Refusal> {
    let mut routes: Vec<&RouteManifest> = manifest.routes.values().collect();
    routes.sort_by(|left, right| left.route.cmp(&right.route));

    let mut refusals: Vec<Refusal> = routes
        .into_iter()
        .filter_map(|route| {
            let reasons = route_reasons(route);
            (!reasons.is_empty()).then(|| Refusal {
                subject: route.route.clone(),
                reasons,
            })
        })
        .collect();

    // `serve` boots with these and reports each one missing from its pages. A
    // file freezes that absence: every visitor gets the page without the
    // component, and no boot report is printed to anyone.
    refusals.extend(manifest.static_render_failures.iter().map(|failure| Refusal {
        subject: failure.module_path.clone(),
        reasons: vec![format!(
            "`{}` failed its build-time render and is missing from every page that \
             renders it: {}",
            failure.component, failure.error
        )],
    }));
    refusals
}

fn route_reasons(route: &RouteManifest) -> Vec<String> {
    fn listed(names: impl Iterator<Item = String>) -> String {
        let mut names: Vec<String> = names.collect();
        names.sort();
        names.dedup();
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    let mut reasons = Vec::new();

    if route.route.contains('[') || route.route.contains('{') {
        reasons.push(
            "a dynamic segment — a static host has one file per URL, and nothing lists \
             the values this segment takes"
                .to_string(),
        );
    }
    if route.auth == RouteAuth::Required {
        reasons.push(
            "`export const auth = \"required\"` — a static host cannot check who is asking"
                .to_string(),
        );
    }
    if !route.action_ids.is_empty() {
        reasons.push(format!(
            "declares action(s) {} — a static host has nothing to POST them to",
            listed(route.action_ids.iter().map(|entry| entry.name.clone()))
        ));
    }

    // The server's own answer to "does this page open the live lane" — the
    // same function that stamps `__ALBEDO_LIVE__` into it. A page that opens the
    // lane on a static host retries `/_albedo/patches` against a 404 forever.
    if route_needs_live_lane(route) {
        let mut why = Vec::new();
        if !route.tier_b.is_empty() {
            why.push(format!(
                "renders per request: {}",
                listed(route.tier_b.iter().map(|node| node.component_id.clone()))
            ));
        }
        if !route.tier_c.is_empty() {
            why.push(format!(
                "islands: {}",
                listed(route.tier_c.iter().map(|node| node.component_id.clone()))
            ));
        }
        if !route.shared_slot_topics.is_empty() {
            why.push(format!(
                "live topic(s) {}",
                listed(route.shared_slot_topics.iter().cloned())
            ));
        }
        if !route.shared_slot_partitions.is_empty() {
            why.push("a partitioned collection".to_string());
        }
        reasons.push(format!(
            "needs the server's live lane ({})",
            if why.is_empty() {
                "the server says so".to_string()
            } else {
                why.join("; ")
            }
        ));
    }
    // Declared external data resolves on the server at request time whether or
    // not it also opens a lane.
    if !route.shared_slot_sources.is_empty() {
        reasons.push("reads a declared external source at request time".to_string());
    }
    reasons
}

fn process_refusals(server: &AlbedoServer) -> Vec<Refusal> {
    server
        .process_bound_entries()
        .into_iter()
        .map(|(what, entry)| Refusal {
            subject: entry,
            reasons: vec![match what {
                "middleware" => {
                    "middleware runs on every request, and a static host runs nothing".to_string()
                }
                _ => "jobs run on the server's clock, and a static host has none".to_string(),
            }],
        })
        .collect()
}

fn read_manifest(dist_dir: &Path) -> Result<RenderManifestV2, ExportError> {
    let path = dist_dir.join(RENDER_MANIFEST_FILENAME);
    let raw = std::fs::read_to_string(&path).map_err(|err| {
        ExportError::Failed(format!(
            "no build output at '{}' ({err}) — run `albedo build` first",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw)
        .map_err(|err| ExportError::Failed(format!("'{}' is not a manifest: {err}", path.display())))
}

/// Everything fetched through the server, held in memory until all of it has
/// passed — so a refusal found on the last page still leaves the disk alone.
struct Rendered {
    /// `(route, html)`, in route order.
    pages: Vec<(String, Vec<u8>)>,
    /// `(url, body)` for the framework's client scripts.
    assets: Vec<(String, Vec<u8>)>,
}

/// Fetch the client scripts and every given route. Returns what rendered
/// cleanly beside the routes that did not — a refused page is a finding, not a
/// reason to stop looking at the rest.
async fn render_through_server(
    server: &AlbedoServer,
    routes: &[&String],
) -> Result<(Rendered, Vec<Refusal>), ExportError> {
    let router = server.router();

    // One session for the whole export, chosen here so its token is known. The
    // token is what tells a form apart from prose: the fill pass stamps it into
    // every action form's hidden `_csrf` input and nowhere else but the
    // bootstrap script — 128 random bits no page text can contain by accident.
    let session = SessionId::random();
    let token = server.csrf_registry().token_for(session);
    let cookie = format!("{ALBEDO_SESSION_COOKIE}={}", session.as_uuid());
    let live_bootstrap = csrf_bootstrap_script(&token);
    let blank_bootstrap = csrf_bootstrap_script("");

    let mut assets = Vec::with_capacity(EMBEDDED_ASSET_PATHS.len());
    for &url in EMBEDDED_ASSET_PATHS {
        let fetched = fetch(&router, url, &cookie)
            .await
            .map_err(ExportError::Failed)?;
        if fetched.status != StatusCode::OK {
            return Err(ExportError::Failed(format!(
                "the server answered {} for its own client script {url}",
                fetched.status
            )));
        }
        assets.push((url.to_string(), fetched.body));
    }
    let asset_urls: BTreeSet<&str> = EMBEDDED_ASSET_PATHS.iter().copied().collect();

    let mut pages = Vec::with_capacity(routes.len());
    let mut refusals = Vec::new();
    for &route in routes {
        let fetched = fetch(&router, route, &cookie)
            .await
            .map_err(ExportError::Failed)?;

        let mut reasons = Vec::new();
        let is_html = fetched
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html"));
        if fetched.status != StatusCode::OK || !is_html {
            reasons.push(format!(
                "`GET {route}` answered {} {} — only a 200 page can become a file",
                fetched.status,
                fetched
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("(no content type)")
            ));
        }
        let html = match String::from_utf8(fetched.body) {
            Ok(html) => html,
            Err(_) => {
                reasons.push("the page is not UTF-8".to_string());
                String::new()
            }
        };
        let html = html.replacen(&live_bootstrap, &blank_bootstrap, 1);
        if html.contains(&token) {
            reasons.push(
                "carries a form bound to an action (its hidden `_csrf` input was filled) — \
                 a static host has nothing to POST it to"
                    .to_string(),
            );
        }
        for url in framework_references(&html) {
            if !asset_urls.contains(url) {
                reasons.push(format!("points at `{url}`, which only the server answers"));
            }
        }

        if reasons.is_empty() {
            pages.push((route.clone(), html.into_bytes()));
        } else {
            refusals.push(Refusal {
                subject: route.clone(),
                reasons,
            });
        }
    }

    Ok((Rendered { pages, assets }, refusals))
}

struct Fetched {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn fetch(router: &axum::Router, url: &str, cookie: &str) -> Result<Fetched, String> {
    let mut waited = Duration::ZERO;
    loop {
        let request = Request::builder()
            .method("GET")
            .uri(url)
            .header(header::ACCEPT, "text/html,*/*")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .map_err(|err| format!("GET {url} could not be built: {err}"))?;
        let response = router
            .clone()
            .oneshot(request)
            .await
            .map_err(|never| -> String { match never {} })?;

        if response.status() == StatusCode::TOO_MANY_REQUESTS && waited < RATE_LIMIT_PATIENCE {
            tokio::time::sleep(RATE_LIMIT_STEP).await;
            waited += RATE_LIMIT_STEP;
            continue;
        }

        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|err| format!("GET {url}: the body could not be read: {err}"))?;
        return Ok(Fetched {
            status,
            headers,
            body: body.to_vec(),
        });
    }
}

/// Every `/_albedo/...` URL the page loads, links to or posts to — through a
/// `src`, `href` or `action` attribute — query and fragment dropped.
///
/// `action` is the one the CSRF check cannot see: a plain form posting to a
/// framework endpoint (the auth routes' own forms) carries no action token,
/// and still has nowhere to go on a static host.
///
/// Attribute-anchored on purpose: a docs page that mentions `/_albedo/action`
/// in prose is not loading it.
fn framework_references(html: &str) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    for anchor in ["src=\"/_albedo/", "href=\"/_albedo/", "action=\"/_albedo/"] {
        let mut rest = html;
        while let Some(at) = rest.find(anchor) {
            let start = at + anchor.len() - "/_albedo/".len();
            let tail = &rest[start..];
            let end = tail.find('"').unwrap_or(tail.len());
            let url = &tail[..end];
            let url = url.split(['?', '#']).next().unwrap_or(url);
            found.insert(url);
            rest = &tail[end..];
        }
    }
    found
}

/// `/about` → `about/index.html`; `/` → `index.html`.
///
/// The directory form because it is the one every static host resolves
/// (`about.html` needs "pretty URLs" switched on at most of them).
fn page_path(route: &str) -> PathBuf {
    let mut path: PathBuf = route.split('/').filter(|part| !part.is_empty()).collect();
    path.push("index.html");
    path
}

fn url_path(url: &str) -> PathBuf {
    url.split('/').filter(|part| !part.is_empty()).collect()
}

fn write_site(
    opts: &ProductionServerOptions,
    rendered: &Rendered,
    out_dir: &Path,
) -> Result<StaticSite, ExportError> {
    let failed = |what: String| ExportError::Failed(what);

    // Every file's origin, keyed by where it lands, so two things that want
    // the same path are named instead of one silently overwriting the other.
    // Ordered as `serve` resolves: the build's `public/` mirror, then the
    // author's `public/` over it (the mount `serve` consults first), then the
    // client scripts over both (dispatched before `public/` is looked at).
    let mut files: BTreeMap<PathBuf, (String, Source)> = BTreeMap::new();
    for root in [opts.dist_dir.join("public"), opts.project_dir.join("public")] {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root).follow_links(true) {
            let entry = entry.map_err(|err| failed(format!("reading '{}': {err}", root.display())))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root)
                .map_err(|err| failed(err.to_string()))?
                .to_path_buf();
            let url = format!(
                "/{}",
                relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            );
            files.insert(relative, (url, Source::Disk(entry.path().to_path_buf())));
        }
    }
    for (url, body) in &rendered.assets {
        files.insert(url_path(url), (url.clone(), Source::Bytes(body)));
    }

    let mut clashes = Vec::new();
    let mut pages = Vec::with_capacity(rendered.pages.len());
    for (route, _) in &rendered.pages {
        let path = page_path(route);
        if let Some((url, _)) = files.get(&path) {
            clashes.push(Refusal {
                subject: route.clone(),
                reasons: vec![format!(
                    "its page lands at `{}`, where `{url}` already is — a static host can \
                     serve only one of them",
                    path.display()
                )],
            });
        }
        pages.push(path);
    }
    if !clashes.is_empty() {
        return Err(ExportError::Refused {
            refusals: clashes,
            unchecked: None,
        });
    }

    // Written beside the destination and swapped in whole, so an interrupted
    // export never leaves a half-written site that looks deployable — and the
    // previous export is replaced, never merged into.
    let staging = out_dir.with_extension("partial");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|err| failed(format!("clearing '{}': {err}", staging.display())))?;
    }

    let write = |relative: &Path, bytes: &[u8]| -> Result<u64, ExportError> {
        let target = staging.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| failed(format!("creating '{}': {err}", parent.display())))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|err| failed(format!("writing '{}': {err}", target.display())))?;
        Ok(bytes.len() as u64)
    };

    let mut exported_files = Vec::with_capacity(files.len());
    for (relative, (url, source)) in &files {
        let bytes = match source {
            Source::Disk(from) => {
                // `serve` sends a public file's bytes verbatim (`read_response`
                // is a `fs::read`), so a copy is the same response by
                // construction and keeps names a URL would have to escape.
                let target = staging.join(relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| failed(format!("creating '{}': {err}", parent.display())))?;
                }
                std::fs::copy(from, &target).map_err(|err| {
                    failed(format!("copying '{}': {err}", from.display()))
                })?
            }
            Source::Bytes(body) => write(relative, body)?,
        };
        exported_files.push(ExportedFile {
            url: url.clone(),
            path: relative.clone(),
            bytes,
        });
    }

    let mut exported_pages = Vec::with_capacity(pages.len());
    for ((route, html), path) in rendered.pages.iter().zip(pages) {
        let bytes = write(&path, html)?;
        exported_pages.push(ExportedFile {
            url: route.clone(),
            path,
            bytes,
        });
    }

    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir).map_err(|err| {
            failed(format!(
                "replacing '{}': {err} (is a server still serving it?)",
                out_dir.display()
            ))
        })?;
    }
    std::fs::rename(&staging, out_dir)
        .map_err(|err| failed(format!("finishing '{}': {err}", out_dir.display())))?;

    Ok(StaticSite {
        out_dir: out_dir.to_path_buf(),
        pages: exported_pages,
        files: exported_files,
    })
}

enum Source<'a> {
    Disk(PathBuf),
    Bytes(&'a [u8]),
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::manifest::schema::{HtmlShell, RouteActionEntry};

    fn route(path: &str) -> RouteManifest {
        RouteManifest {
            route: path.to_string(),
            shell: HtmlShell {
                doctype_and_head: String::new(),
                body_open: String::new(),
                body_close: String::new(),
                shim_script: String::new(),
            },
            tier_a_root: Vec::new(),
            tier_b: Vec::new(),
            tier_c: Vec::new(),
            shared_slot_topics: Vec::new(),
            auth: RouteAuth::Public,
            shared_slot_partitions: Vec::new(),
            shared_slot_sources: Vec::new(),
            shared_slot_columns: Default::default(),
            action_ids: Vec::new(),
            layout_chain: Vec::new(),
            error_component: None,
            loading_component: None,
            metadata: Default::default(),
            dynamic_metadata: None,
        }
    }

    fn manifest(routes: Vec<RouteManifest>) -> RenderManifestV2 {
        let mut manifest = RenderManifestV2::legacy_defaults();
        manifest.routes = routes
            .into_iter()
            .map(|route| (route.route.clone(), route))
            .collect();
        manifest
    }

    /// The control: a plain Tier-A route is refused for nothing. Without it,
    /// a checker that refused every route would pass every test below.
    #[test]
    fn a_plain_static_route_is_not_refused() {
        assert!(manifest_refusals(&manifest(vec![route("/"), route("/about")])).is_empty());
    }

    #[test]
    fn each_kind_of_server_work_is_refused_by_route() {
        let mut gated = route("/dash");
        gated.auth = RouteAuth::Required;
        let mut acting = route("/guestbook");
        acting.action_ids = vec![RouteActionEntry {
            action_id: 7,
            name: "sign".to_string(),
        }];
        acting.shared_slot_topics = vec!["guestbook".to_string()];

        let refusals = manifest_refusals(&manifest(vec![
            route("/"),
            route("/room/[id]"),
            gated,
            acting,
        ]));
        let subjects: Vec<&str> = refusals.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["/dash", "/guestbook", "/room/[id]"]);

        let text = refusals
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("/room/[id]: a dynamic segment"), "{text}");
        assert!(text.contains("/dash: `export const auth"), "{text}");
        assert!(text.contains("action(s) `sign`"), "{text}");
        assert!(text.contains("live topic(s) `guestbook`"), "{text}");
    }

    #[test]
    fn framework_references_are_read_from_attributes_only() {
        let html = concat!(
            r#"<script type="module" src="/_albedo/runtime.js?v=1"></script>"#,
            r#"<link rel="modulepreload" href="/_albedo/npm/chunk.abc.js">"#,
            r#"<form method="post" action="/_albedo/auth/password/login">"#,
            r#"<p>POST to "/_albedo/action" from a form</p>"#,
        );
        let found: Vec<&str> = framework_references(html).into_iter().collect();
        assert_eq!(
            found,
            [
                "/_albedo/auth/password/login",
                "/_albedo/npm/chunk.abc.js",
                "/_albedo/runtime.js"
            ]
        );
    }

    #[test]
    fn pages_land_where_static_hosts_look() {
        assert_eq!(page_path("/"), PathBuf::from("index.html"));
        assert_eq!(
            page_path("/docs/intro"),
            ["docs", "intro", "index.html"].iter().collect::<PathBuf>()
        );
    }
}
