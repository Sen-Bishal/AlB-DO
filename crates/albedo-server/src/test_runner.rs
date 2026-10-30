//! `albedo test` — the server half.
//!
//! A test file runs in a [`TestRealm`] (the compiler crate). Everything it asks
//! of the app lands here:
//!
//! * **`visit()` / `page.submit()`** go through the router `albedo serve`
//!   binds, in process, with the cookies and client address of the visitor
//!   making them. The server is the real one — [`AlbedoServer::run_until`]
//!   boots it, so FORGE, AUTH, the job runner and the middleware are all live —
//!   and the request carries `Sec-Fetch-Mode: navigate`, so a refusal comes back
//!   the way a browser gets it.
//! * **`render(<Card />)`** renders through the renderer the app uses for that
//!   component's tier: the build's Tier-A renderer ([`TierARenderer`], the very
//!   function that bakes pages) or the server's QuickJS renderer for Tier B
//!   and C. A component test therefore asserts against markup a visitor gets.
//! * **`signUp()` / `signIn()`** post to the app's own password endpoints.
//!
//! ## Isolation
//!
//! Each test file gets its own server, its own tokio runtime and its own
//! `forge.db` in a temporary directory: it starts from the config's seed rows
//! and nothing else, and it never touches the app's real database. Tests in one
//! file share that database, in order — the way requests to one server do.
//!
//! 🪤 **Exactly one database is open at any moment.** The libSQL teardown crash
//! (`STATUS_ACCESS_VIOLATION`) needs several substrates alive at once; a file's
//! server, its runtime (and every task holding the substrate) are shut down
//! before the next file's server boots, so the run never has two.

use crate::boot::{boot_production_server, ProductionServerOptions};
use crate::render::csrf::{CsrfRegistry, ALBEDO_SESSION_COOKIE};
use crate::renderer_runtime::{RendererRuntime, RENDER_MANIFEST_FILENAME};
use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::Router;
use dom_render_compiler::manifest::builder::TierARenderer;
use dom_render_compiler::manifest::schema::{ComponentManifestEntry, RenderManifestV2, Tier};
use dom_render_compiler::runtime::{CompiledProject, SessionId};
use dom_render_compiler::testing::{is_test_module, TestHost, TestOutcome, TestRealm};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// A page is one streamed document; anything past this is not a page.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Redirects followed before a request is called a loop.
const MAX_REDIRECTS: usize = 10;

/// How long a file's server may take to come up, or to wind down.
const SERVER_DEADLINE: Duration = Duration::from_secs(30);

/// What to run.
#[derive(Debug, Clone)]
pub struct TestRunOptions {
    /// The server every test file boots — what `albedo serve` would boot.
    /// `forge_db` and `port` are replaced per file.
    pub server: ProductionServerOptions,
    /// The test files, in the order to run them.
    pub files: Vec<PathBuf>,
    /// Run only tests whose full name contains this.
    pub name_filter: Option<String>,
    /// How long one test may run.
    pub timeout: Duration,
}

/// How one test ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    /// Every assertion held.
    Passed,
    /// Why, as the reporter prints it.
    Failed(String),
    /// `test.skip`, a sibling's `.only`, or outside `--filter`.
    Skipped,
    /// `test.todo` — reported, never run.
    Todo,
}

/// One test's result.
#[derive(Debug, Clone)]
pub struct TestReport {
    /// `describe` names and the test's own, joined with ` › `.
    pub name: String,
    /// How it ended.
    pub status: TestStatus,
    /// Wall time, hooks included.
    pub elapsed: Duration,
}

/// One file's results.
#[derive(Debug, Clone)]
pub struct FileReport {
    /// The test file.
    pub file: PathBuf,
    /// The file did not get as far as running tests — it failed to load, or
    /// the server it needs did not boot. Counts as a failure.
    pub error: Option<String>,
    /// Its tests, in the order the file registered them.
    pub tests: Vec<TestReport>,
    /// Wall time for the file, server boot included.
    pub elapsed: Duration,
}

impl FileReport {
    /// Did anything in this file fail?
    #[must_use]
    pub fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .tests
                .iter()
                .any(|test| matches!(test.status, TestStatus::Failed(_)))
    }
}

/// Every `*.test.{ts,tsx,js,jsx}` / `*.spec.…` under the project, sorted.
///
/// Skips `node_modules`, build output and hidden directories — the places a
/// test file found would be someone else's.
#[must_use]
pub fn discover_test_files(project_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(project_dir)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !(name.starts_with('.') || name == "node_modules" || name == "target" || name == "dist")
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && is_test_module(entry.path()))
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

/// The renderers `render()` reaches, built once per run from the build output.
struct Renderers {
    project_dir: PathBuf,
    components: Vec<ComponentManifestEntry>,
    tier_a: TierARenderer,
    quickjs: RendererRuntime,
}

impl Renderers {
    fn load(opts: &ProductionServerOptions, npm: &[(String, String, u64)]) -> Result<Self, String> {
        let manifest_path = opts.dist_dir.join(RENDER_MANIFEST_FILENAME);
        let manifest: RenderManifestV2 = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .map_err(|err| format!("could not read {}: {err}", manifest_path.display()))?,
        )
        .map_err(|err| format!("{} is unreadable: {err}", manifest_path.display()))?;
        let files: Vec<PathBuf> = manifest
            .components
            .iter()
            .map(|component| absolute_module(&opts.project_dir, &component.module_path))
            .collect();
        let tier_a = TierARenderer::load(&files);
        let mut quickjs = RendererRuntime::from_artifacts_dir(opts.dist_dir.clone())
            .map_err(|err| format!("the server's renderer did not load: {err}"))?;
        quickjs.install_npm_bundles(npm);
        Ok(Self {
            project_dir: opts.project_dir.clone(),
            components: manifest.components,
            tier_a,
            quickjs,
        })
    }

    fn render(&mut self, payload: &Value) -> Result<Value, String> {
        let module = str_field(payload, "module")?;
        let export = payload
            .get("export")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let props = payload.get("props").cloned().unwrap_or_else(|| json!({}));
        let host = payload.get("host").filter(|host| !host.is_null());

        let file = self.project_dir.join(module);
        let wanted = comparable(&file);
        let component = self
            .components
            .iter()
            .find(|component| {
                comparable(&absolute_module(&self.project_dir, &component.module_path)) == wanted
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{module} is not a component this build compiled. render() takes a \
                     component from the app's source tree — a file the build tiers."
                )
            })?;
        if export != "default" {
            return Err(format!(
                "render() renders a file's default export; `{export}` is a named export of \
                 {module}. Make it the default export of its own file, or test the page that \
                 uses it with visit()."
            ));
        }

        let html = match component.tier {
            Tier::A => self.tier_a.render(&file, &props)?,
            Tier::B | Tier::C => {
                let props_json = serde_json::to_string(&props).map_err(|err| err.to_string())?;
                let host_json = host.map(Value::to_string);
                self.quickjs
                    .render_component(&component.module_path, props_json, host_json)?
            }
        };
        let tier = match component.tier {
            Tier::A => "A",
            Tier::B => "B",
            Tier::C => "C",
        };
        Ok(json!({ "html": html, "tier": tier, "component": component.name }))
    }
}

/// A component's module path as the manifest records it, made absolute.
fn absolute_module(project_dir: &Path, module_path: &str) -> PathBuf {
    let path = PathBuf::from(module_path);
    if path.is_absolute() {
        path
    } else {
        project_dir.join(path)
    }
}

/// A path in a form two spellings of one file agree on.
fn comparable(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// One visitor's cookies, and the address their requests come from.
struct Jar {
    cookies: BTreeMap<String, String>,
    peer: SocketAddr,
}

impl Jar {
    fn cookie_header(&self) -> Option<HeaderValue> {
        if self.cookies.is_empty() {
            return None;
        }
        let joined = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        HeaderValue::from_str(&joined).ok()
    }

    fn absorb(&mut self, headers: &HeaderMap) {
        for value in headers.get_all(header::SET_COOKIE) {
            let Ok(text) = value.to_str() else { continue };
            let mut parts = text.split(';');
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
                continue;
            };
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            let expired = parts.any(|attribute| {
                let attribute = attribute.trim().to_ascii_lowercase();
                attribute == "max-age=0" || attribute.starts_with("max-age=-")
            });
            if expired || value.is_empty() {
                self.cookies.remove(&name);
            } else {
                self.cookies.insert(name, value);
            }
        }
    }

    /// The tab session the server knows this visitor by, minting one if the
    /// visitor has none yet — what a browser has after its first page.
    fn tab_session(&mut self) -> SessionId {
        if let Some(existing) = self
            .cookies
            .get(ALBEDO_SESSION_COOKIE)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
        {
            return SessionId::new(existing);
        }
        let session = SessionId::random();
        self.cookies.insert(
            ALBEDO_SESSION_COOKIE.to_string(),
            session.as_uuid().to_string(),
        );
        session
    }
}

/// One answered request.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

/// What a test's calls land on, for one file.
struct Host {
    runtime: tokio::runtime::Handle,
    router: Router,
    csrf: Arc<CsrfRegistry>,
    renderers: Rc<RefCell<Renderers>>,
    jars: HashMap<String, Jar>,
    next_jar: u32,
    auth_cookie: String,
}

impl Host {
    fn jar(&mut self, id: &str) -> Result<&mut Jar, String> {
        self.jars
            .get_mut(id)
            .ok_or_else(|| format!("no visitor '{id}' — use a user from signUp() or signIn()"))
    }

    fn new_jar(&mut self) -> String {
        self.next_jar += 1;
        let n = self.next_jar;
        // Every visitor gets its own address, so SHUTTER rations them apart the
        // way it rations two people — a test that signs up six users is not one
        // client hammering the sign-up form.
        let ip = Ipv4Addr::new(10, ((n >> 16) & 0xff) as u8, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8);
        let id = format!("visitor-{n}");
        self.jars.insert(
            id.clone(),
            Jar {
                cookies: BTreeMap::new(),
                peer: SocketAddr::new(IpAddr::V4(ip), 40_000),
            },
        );
        id
    }

    /// One request through the router, as `jar`, with nothing followed.
    fn send(
        &mut self,
        jar: &str,
        method: &Method,
        path: &str,
        form: Option<&Map<String, Value>>,
        navigate: bool,
    ) -> Result<Answer, String> {
        let (cookie, peer) = {
            let jar = self.jar(jar)?;
            (jar.cookie_header(), jar.peer)
        };
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::HOST, "localhost")
            .header("sec-fetch-site", "same-origin");
        // 🪤 Said either way, never left out: with no `Sec-Fetch-Mode` the
        // server reads `Accept: text/html` as a navigation
        // (`auth_gate::is_document_navigation`), and a refused sign-in then
        // answers `303` back to the form instead of the status and sentence a
        // script call is owed.
        builder = if navigate {
            builder
                .header(header::ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8")
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-dest", "document")
        } else {
            builder
                .header(header::ACCEPT, "*/*")
                .header("sec-fetch-mode", "cors")
                .header("sec-fetch-dest", "empty")
        };
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let body = match form {
            Some(fields) => {
                builder = builder.header(
                    header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                );
                let mut encoded = url::form_urlencoded::Serializer::new(String::new());
                for (name, value) in fields {
                    let text = match value {
                        Value::String(text) => text.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    encoded.append_pair(name, &text);
                }
                Body::from(encoded.finish())
            }
            None => Body::empty(),
        };
        let mut request = builder
            .body(body)
            .map_err(|err| format!("{method} {path} is not a request: {err}"))?;
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));

        let router = self.router.clone();
        let (status, headers, bytes) = self.runtime.block_on(async move {
            let response = router
                .oneshot(request)
                .await
                .map_err(|err| format!("{method} {path} failed: {err}"))?;
            let (parts, body) = response.into_parts();
            let bytes = to_bytes(body, MAX_BODY_BYTES)
                .await
                .map_err(|err| format!("{method} {path}: the body did not arrive: {err}"))?;
            Ok::<_, String>((parts.status, parts.headers, bytes))
        })?;
        self.jar(jar)?.absorb(&headers);
        Ok(Answer {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }

    /// A request the way a browser makes it: as a navigation, redirects followed.
    fn navigate(&mut self, payload: &Value) -> Result<Value, String> {
        let jar = str_field(payload, "jar")?.to_string();
        let mut method = Method::from_bytes(
            payload
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .as_bytes(),
        )
        .map_err(|_| "not an HTTP method".to_string())?;
        let mut path = str_field(payload, "path")?.to_string();
        if !path.starts_with('/') {
            return Err(format!("{path} is not a path on this app"));
        }
        let mut form = payload.get("form").and_then(Value::as_object).cloned();
        let follow = payload.get("follow").and_then(Value::as_bool).unwrap_or(true);
        let mut redirects = Vec::new();

        loop {
            let answer = self.send(&jar, &method, &path, form.as_ref(), true)?;
            let location = answer
                .headers
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let next = location.as_deref().and_then(same_app_path);
            if follow && answer.status.is_redirection() {
                if let Some(next) = next {
                    redirects.push(json!({ "status": answer.status.as_u16(), "url": path }));
                    if redirects.len() > MAX_REDIRECTS {
                        return Err(format!(
                            "more than {MAX_REDIRECTS} redirects starting from {}",
                            redirects[0]["url"]
                        ));
                    }
                    // 303 always, and 301/302 after a POST, become a GET — what
                    // every browser does. 307/308 resend as they were.
                    let keep = matches!(answer.status.as_u16(), 307 | 308);
                    if !keep {
                        method = Method::GET;
                        form = None;
                    }
                    path = next;
                    continue;
                }
            }
            return Ok(json!({
                "status": answer.status.as_u16(),
                "url": path,
                "headers": header_map(&answer.headers),
                "body": answer.body,
                "redirects": redirects,
            }));
        }
    }

    /// The app's own password endpoints, posted as a script would — so a
    /// refusal answers with its status and sentence instead of a redirect.
    fn auth(&mut self, payload: &Value) -> Result<Value, String> {
        let jar = str_field(payload, "jar")?.to_string();
        let kind = str_field(payload, "kind")?;
        let (endpoint, call) = match kind {
            "register" => ("/_albedo/auth/password/register", "signUp"),
            "login" => ("/_albedo/auth/password/login", "signIn"),
            "logout" => ("/_albedo/auth/logout", "signOut"),
            other => return Err(format!("no auth call '{other}'")),
        };
        let session = self.jar(&jar)?.tab_session();
        let mut form = Map::new();
        form.insert("_csrf".to_string(), Value::String(self.csrf.token_for(session)));
        let email = payload.get("email").and_then(Value::as_str).unwrap_or_default().to_string();
        if kind != "logout" {
            form.insert("email".to_string(), Value::String(email.clone()));
            form.insert(
                "password".to_string(),
                Value::String(str_field(payload, "password")?.to_string()),
            );
        }
        let who = if email.is_empty() { String::new() } else { format!("\"{email}\"") };
        let answer = self.send(&jar, &Method::POST, endpoint, Some(&form), false)?;

        if !(answer.status.is_redirection() || answer.status.is_success()) {
            return Err(format!(
                "{call}({who}) was refused — {}: {}",
                answer.status,
                first_paragraph(&answer.body).unwrap_or_else(|| "the server gave no reason".to_string())
            ));
        }
        let auth_cookie = self.auth_cookie.clone();
        let signed_in = self.jar(&jar)?.cookies.contains_key(&auth_cookie);
        if kind != "logout" && !signed_in {
            return Err(format!(
                "{call}({who}) was answered {} but no session came back. If the app requires a \
                 confirmed email (`verify: \"required\"`), a new account cannot sign in until it \
                 confirms.",
                answer.status
            ));
        }
        Ok(Value::Null)
    }
}

impl TestHost for Host {
    fn call(&mut self, op: &str, payload: Value) -> Result<Value, String> {
        match op {
            "jar" => Ok(Value::String(self.new_jar())),
            "request" => self.navigate(&payload),
            "auth" => self.auth(&payload),
            "render" => self.renderers.borrow_mut().render(&payload),
            other => Err(format!("albedo test has no host call '{other}'")),
        }
    }
}

fn str_field<'a>(payload: &'a Value, name: &str) -> Result<&'a str, String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the call is missing '{name}'"))
}

/// The path part of a `Location` that stays on this app, or `None` for one
/// that leaves it (an OAuth provider, a payment page) — followed no further.
fn same_app_path(location: &str) -> Option<String> {
    if location.starts_with('/') && !location.starts_with("//") {
        return Some(location.to_string());
    }
    let url = url::Url::parse(location).ok()?;
    let host = url.host_str()?;
    if host == "localhost" || host == "127.0.0.1" || host == "[::1]" {
        let mut path = url.path().to_string();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        return Some(path);
    }
    None
}

fn header_map(headers: &HeaderMap) -> Value {
    let mut out = Map::new();
    for name in headers.keys() {
        let joined = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(if name == header::SET_COOKIE { "\n" } else { ", " });
        out.insert(name.as_str().to_string(), Value::String(joined));
    }
    Value::Object(out)
}

/// The text of the first `<p>` — where the auth refusal page says why.
fn first_paragraph(html: &str) -> Option<String> {
    let start = html.find("<p>")? + 3;
    let end = html[start..].find("</p>")? + start;
    let text = html[start..end]
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    Some(text.trim().to_string())
}

/// Run the files, reporting each as it finishes.
///
/// # Errors
/// The run could not start at all — no build, or the renderers did not load.
/// A file that fails to load or boot is a failed [`FileReport`], not an error.
pub fn run_tests(
    opts: &TestRunOptions,
    mut on_file: impl FnMut(&FileReport),
) -> Result<Vec<FileReport>, String> {
    let server = &opts.server;
    // The npm bundles the build left, the same ones `serve` installs — so a
    // component that imports a package loads in a test as it does in a page.
    let compiled =
        CompiledProject::load_from_dir_with_prebuilt_npm(&server.source_root, &server.dist_dir)
            .map_err(|err| {
                format!(
                    "failed to load the source tree at {}: {err}",
                    server.source_root.display()
                )
            })?;
    let npm: Vec<(String, String, u64)> = compiled
        .npm_bundles()
        .iter()
        .flat_map(|bundle| bundle.artifacts.iter())
        .map(|artifact| (artifact.key.clone(), artifact.script.clone(), artifact.source_hash))
        .collect();
    drop(compiled);
    let renderers = Rc::new(RefCell::new(Renderers::load(server, &npm)?));
    let auth_cookie = server
        .auth
        .session
        .cookie
        .clone()
        .unwrap_or_else(|| dom_render_compiler::auth::DEFAULT_SESSION_COOKIE.to_string());

    let mut reports = Vec::with_capacity(opts.files.len());
    for file in &opts.files {
        let started = Instant::now();
        let mut report = FileReport {
            file: file.clone(),
            error: None,
            tests: Vec::new(),
            elapsed: Duration::ZERO,
        };
        if let Err(error) = run_file(opts, file, &npm, &renderers, &auth_cookie, &mut report) {
            report.error = Some(error);
        }
        report.elapsed = started.elapsed();
        on_file(&report);
        reports.push(report);
    }
    Ok(reports)
}

fn run_file(
    opts: &TestRunOptions,
    file: &Path,
    npm: &[(String, String, u64)],
    renderers: &Rc<RefCell<Renderers>>,
    auth_cookie: &str,
    report: &mut FileReport,
) -> Result<(), String> {
    // Load first: a file that does not compile is reported without paying for
    // a server boot.
    let mut realm = TestRealm::new(&opts.server.project_dir, npm)?;
    let cases = realm.load_file(file)?;

    let scratch = tempfile::tempdir()
        .map_err(|err| format!("could not make a scratch directory: {err}"))?;
    let mut server_opts = opts.server.clone();
    server_opts.forge_db = Some(scratch.path().join("forge.db"));
    // Bound on a port nobody chose, though no test reaches it over TCP: the
    // listener is part of the boot `serve` does, and requests go to the router
    // in process.
    server_opts.host = "127.0.0.1".to_string();
    server_opts.port = 0;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start a tokio runtime: {err}"))?;
    let result = (|| -> Result<(), String> {
        let server = boot_production_server(&server_opts)
            .map_err(|err| format!("the app did not boot: {err}"))?;
        let router = server.router();
        let csrf = server.csrf_registry();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = runtime.spawn(server.run_until(
            move |_report| {
                let _ = ready_tx.send(());
            },
            async move {
                let _ = stop_rx.await;
            },
        ));
        let ready = ready_rx.recv_timeout(SERVER_DEADLINE);
        if ready.is_err() {
            let reason = if serving.is_finished() {
                match runtime.block_on(serving) {
                    Ok(Err(err)) => format!("the app did not boot: {err}"),
                    Ok(Ok(())) => "the app stopped before it was ready".to_string(),
                    Err(err) => format!("the app's server panicked while booting: {err}"),
                }
            } else {
                format!("the app was not ready after {} s", SERVER_DEADLINE.as_secs())
            };
            return Err(reason);
        }

        let host = Rc::new(RefCell::new(Host {
            runtime: runtime.handle().clone(),
            router,
            csrf,
            renderers: Rc::clone(renderers),
            jars: HashMap::new(),
            next_jar: 0,
            auth_cookie: auth_cookie.to_string(),
        }));
        let filter = opts.name_filter.as_deref();
        for (index, case) in cases.iter().enumerate() {
            let filtered_out = filter.is_some_and(|text| !case.name.contains(text));
            let started = Instant::now();
            let status = if case.todo {
                TestStatus::Todo
            } else if case.skip || filtered_out {
                TestStatus::Skipped
            } else {
                match realm.run(index, host.clone(), opts.timeout) {
                    TestOutcome::Passed => TestStatus::Passed,
                    TestOutcome::Failed(reason) => TestStatus::Failed(reason),
                }
            };
            report.tests.push(TestReport {
                name: case.name.clone(),
                status,
                elapsed: started.elapsed(),
            });
        }
        drop(host);

        let _ = stop_tx.send(());
        let _ = runtime.block_on(async {
            tokio::time::timeout(SERVER_DEADLINE, serving).await
        });
        Ok(())
    })();
    drop(realm);
    // Every task that still holds this file's database ends here, before the
    // next file opens its own.
    runtime.shutdown_timeout(SERVER_DEADLINE);
    drop(scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_are_followed_only_while_they_stay_on_the_app() {
        assert_eq!(same_app_path("/sign-in?next=%2F").as_deref(), Some("/sign-in?next=%2F"));
        assert_eq!(same_app_path("http://localhost:3000/a?b=1").as_deref(), Some("/a?b=1"));
        assert_eq!(same_app_path("//evil.example/x"), None);
        assert_eq!(same_app_path("https://github.com/login/oauth/authorize"), None);
    }

    #[test]
    fn a_jar_keeps_what_the_server_sets_and_forgets_what_it_expires() {
        let mut jar = Jar {
            cookies: BTreeMap::new(),
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        };
        let mut headers = HeaderMap::new();
        headers.append(header::SET_COOKIE, HeaderValue::from_static("a=1; Path=/; HttpOnly"));
        headers.append(header::SET_COOKIE, HeaderValue::from_static("b=2; Max-Age=3600"));
        jar.absorb(&headers);
        assert_eq!(jar.cookie_header().unwrap(), "a=1; b=2");

        let mut expiring = HeaderMap::new();
        expiring.append(header::SET_COOKIE, HeaderValue::from_static("a=; Max-Age=0"));
        jar.absorb(&expiring);
        assert_eq!(jar.cookie_header().unwrap(), "b=2");
    }

    #[test]
    fn test_files_are_found_outside_dependencies_and_build_output() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "src/components/Card.test.tsx",
            "src/routes/index.tsx",
            "tests/flow.spec.ts",
            "node_modules/pkg/a.test.js",
            ".albedo/dist/b.test.js",
        ] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "").unwrap();
        }
        let found: Vec<String> = discover_test_files(dir.path())
            .iter()
            .map(|path| {
                path.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(found, vec!["src/components/Card.test.tsx", "tests/flow.spec.ts"]);
    }
}
