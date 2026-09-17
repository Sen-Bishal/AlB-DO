//! MIDDLEWARE · 15.6 — `src/middleware.ts` over real HTTP, from `albedo init`.
//!
//! ## Why this starts from the CLI and not from a manifest fixture
//!
//! Every other HTTP test in this directory hand-writes a render manifest. That
//! is exactly the input this feature would get wrong: the first real build of a
//! middleware file listed it as a **Tier-B component** — `B middleware  imports
//! npm` — because the scanner saw a default-exported function and did what it
//! does with those. A fixture would have had no such entry, and would have
//! passed. So this runs the real binary: `init` → write the files → `build` →
//! `serve`, and asks over a socket.
//!
//! ## Every refusal is paired with a control
//!
//! A middleware that refused everything would satisfy each "is it refused?"
//! assertion. So the gated rewrite is shown refused *and then served* to the
//! same client once it signs in, the out-of-scope lane is checked beside an
//! in-scope one, and the throw is checked beside a request that succeeds on the
//! same server.

use reqwest::{redirect::Policy, StatusCode};
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MIDDLEWARE_TS: &str = r#"
import { next, redirect, rewrite, respond } from "albedo/middleware";
import { BLOCKED } from "./lib/policy";

export default async function middleware(request, { user }) {
  if (request.path === "/old") return redirect("/guestbook");
  if (request.path === "/alias") return rewrite("/guestbook");
  if (request.path === "/secret-alias") return rewrite("/dash");
  if (request.path === BLOCKED) return respond("nope", { status: 403 });
  if (request.path === "/boom") throw new Error("kaput");
  if (request.path === "/" && request.query === "flag=one") {
    const res = await fetch("__FLAGS__/flags", { headers: { authorization: "Bearer upstream-secret" } });
    const flags = await res.json();
    if (!flags.open) return respond("closed", { status: 503 });
    return next({ headers: { "x-flag": String(flags.open) + " " + res.status } });
  }
  if (request.path === "/" && request.query === "flag=two") {
    const [a, b] = await Promise.all([fetch("__FLAGS__/flags?n=1"), fetch("__FLAGS__/flags?n=2")]);
    return next({ headers: { "x-both": (await a.text()) + (await b.text()) } });
  }
  if (request.path === "/undeclared") {
    // The LIVE upstream, under a name that was not declared: `localhost`
    // resolves to loopback, and only `127.0.0.1` is a declared base. A refusal
    // here can only be the egress policy — the server is up and would answer.
    try {
      await fetch("__FLAGS_UNDECLARED__/flags");
      return next({ headers: { "x-egress": "allowed" } });
    } catch (e) {
      return respond(e.message, { status: 502 });
    }
  }
  if (request.path === "/steal") {
    return next({ headers: { "set-cookie": "__Host-albedo_session=forged; Path=/" } });
  }
  return next({
    headers: {
      "x-mw": request.method + " " + request.path + " " + (user ? "user" : "anon"),
      "x-mw-cookies": Object.keys(request.cookies).sort().join(","),
    },
  });
}
"#;

const POLICY_TS: &str = r#"export const BLOCKED = "/blocked";"#;

const DASH_TSX: &str = r#"
export const auth = "required";
export default function Dash() { return <main>secret dashboard</main>; }
"#;

/// A stand-in third-party API on loopback. Declared as a `source` in the app's
/// config, which is what admits a loopback host through `serve`'s egress policy
/// — so a call to it succeeding and a call to an undeclared loopback failing are
/// the same rule, seen from both sides.
async fn upstream() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let auth = Arc::new(Mutex::new(Vec::new()));
    let (h, a) = (Arc::clone(&hits), Arc::clone(&auth));
    let app = axum::Router::new()
        .route(
            "/flags",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let (h, a) = (Arc::clone(&h), Arc::clone(&a));
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                        a.lock().unwrap().push(value.to_string());
                    }
                    ([("content-type", "application/json")], r#"{"open":true}"#)
                }
            }),
        )
        // The declared route the source's refresh loop polls. Kept apart from
        // `/flags` so its traffic cannot satisfy an assertion about the middleware.
        .route("/status", axum::routing::get(|| async { "{}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("upstream binds");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), hits, auth)
}

/// Kills the server when the test ends, pass or fail — a leaked `albedo serve`
/// holds the port and the binary open, and the next `cargo build` fails on it.
struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd
}

fn run_ok(cmd: &mut Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    assert!(
        out.status.success(),
        "{what} failed ({:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    probe.local_addr().expect("addr").port()
}

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    serve_with_env(home, app, &[]).await
}

async fn serve_with_env(home: &Path, app: &Path, env: &[(&str, &str)]) -> (Server, String) {
    let port = free_port();
    let stderr = app.join("serve.stderr.log");
    let child = albedo(home, app)
        .args(["serve", ".", "--port", &port.to_string()])
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).expect("stderr log"))
        .spawn()
        .expect("albedo serve starts");
    let server = Server { child };
    let base = format!("http://127.0.0.1:{port}");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if reqwest::get(format!("{base}/_albedo/runtime.js")).await.is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "albedo serve never answered on {base}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base)
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Sign up through the scaffold's own sign-in page, the way a stranger does, and
/// return the cookie header a browser would now send.
async fn sign_up(client: &reqwest::Client, base: &str) -> String {
    let page = client
        .get(format!("{base}/sign-in"))
        .send()
        .await
        .expect("sign-in page");
    assert_eq!(page.status(), StatusCode::OK);
    let tab = page
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| v.starts_with("__Host-albedo-session="))
        .expect("the render mints a tab session")
        .to_string();
    let html = page.text().await.expect("html");
    let csrf = hidden_field(&html, "_csrf").expect("the form carries a CSRF token");
    let return_path = hidden_field(&html, "_albedo_return").expect("the form carries a return path");

    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[
            ("_csrf", csrf.as_str()),
            ("_albedo_return", return_path.as_str()),
            ("email", "ada@example.com"),
            ("password", "a-long-passphrase"),
        ])
        .send()
        .await
        .expect("register responds");
    assert_eq!(registered.status(), StatusCode::SEE_OTHER, "sign-up answers PRG");
    let session = registered
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| v.starts_with("__Host-albedo_session="))
        .expect("sign-up opens a session")
        .to_string();
    format!("{tab}; {session}; theme=dark")
}

#[tokio::test]
async fn a_scaffolded_app_s_middleware_intercepts_every_app_request_and_grants_nothing() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(albedo(home.path(), work.path()).args(["init", "mwapp"]), "albedo init");
    let app = work.path().join("mwapp");
    fs::create_dir_all(app.join("src/lib")).unwrap();
    fs::create_dir_all(app.join("public")).unwrap();
    let (flags_base, flag_hits, flag_auth) = upstream().await;
    fs::write(
        app.join("src/middleware.ts"),
        MIDDLEWARE_TS
            .replace("__FLAGS_UNDECLARED__", &flags_base.replace("127.0.0.1", "localhost"))
            .replace("__FLAGS__", &flags_base),
    )
    .unwrap();
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("scaffold config");
    assert!(config.contains("export default {"), "the scaffold config shape changed");
    fs::write(
        &config_path,
        config.replacen(
            "export default {",
            &format!(
                "export default {{\n  sources: {{ flags: {{ base: \"{flags_base}\", routes: {{ status: {{ path: \"/status\" }} }} }} }},"
            ),
            1,
        ),
    )
    .unwrap();
    fs::write(app.join("src/lib/policy.ts"), POLICY_TS).unwrap();
    fs::write(app.join("src/routes/dash.tsx"), DASH_TSX).unwrap();
    fs::write(app.join("public/hello.txt"), "hello asset").unwrap();

    let build = albedo(home.path(), &app).arg("build").output().expect("albedo build runs");
    let build_report = String::from_utf8_lossy(&build.stdout).to_string()
        + &String::from_utf8_lossy(&build.stderr);
    assert!(build.status.success(), "albedo build failed:\n{build_report}");
    // The regression that motivated starting from the CLI, checked on the
    // artifact rather than the report: the tier table is decorated for a
    // terminal, and a string search over it passed with the bug reintroduced.
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(app.join(".albedo/dist/render-manifest.v2.json")).expect("manifest"),
    )
    .expect("manifest is JSON");
    let components: Vec<&str> = manifest["components"]
        .as_array()
        .expect("components")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(components.contains(&"Dash"), "control: routes are components: {components:?}");
    assert!(
        !components.iter().any(|name| name.eq_ignore_ascii_case("middleware")),
        "the middleware was compiled as a component: {components:?}"
    );

    let (server, base) = serve(home.path(), &app).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let get = |path: &str| client.get(format!("{base}{path}")).send();

    // ── continue, with headers — on a page and on a public/ file ──
    let home_page = get("/").await.unwrap();
    assert_eq!(home_page.status(), StatusCode::OK);
    assert_eq!(header(&home_page, "x-mw").as_deref(), Some("GET / anon"));

    let asset = get("/hello.txt").await.unwrap();
    assert_eq!(header(&asset, "x-mw").as_deref(), Some("GET /hello.txt anon"));
    assert_eq!(asset.text().await.unwrap(), "hello asset");

    // ── redirect, respond, rewrite ──
    let old = get("/old").await.unwrap();
    assert_eq!(old.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(header(&old, "location").as_deref(), Some("/guestbook"));

    // Imported through a relative module — the graph loaded `lib/policy.ts` first.
    let blocked = get("/blocked").await.unwrap();
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert_eq!(blocked.text().await.unwrap(), "nope");

    let guestbook = get("/guestbook").await.unwrap().text().await.unwrap();
    let alias = get("/alias").await.unwrap();
    assert_eq!(alias.status(), StatusCode::OK);
    assert!(
        alias.text().await.unwrap().contains("guestbook"),
        "a rewrite serves the target route"
    );
    assert!(guestbook.contains("guestbook"), "control: the target itself renders");

    // ── scope: an action is intercepted, the framework's own lanes are not ──
    let action = client
        .post(format!("{base}/_albedo/action"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(header(&action, "x-mw").as_deref(), Some("POST /_albedo/action anon"));
    let runtime = get("/_albedo/runtime.js").await.unwrap();
    assert_eq!(runtime.status(), StatusCode::OK);
    assert!(header(&runtime, "x-mw").is_none(), "the client runtime is not app traffic");

    // ── fetch(): out through APERTURE, answer read back on the replay ──
    let flagged = get("/?flag=one").await.unwrap();
    assert_eq!(flagged.status(), StatusCode::OK);
    assert_eq!(header(&flagged, "x-flag").as_deref(), Some("true 200"));
    assert!(flag_hits.load(Ordering::SeqCst) >= 1, "the upstream was really called");
    assert!(
        flag_auth.lock().unwrap().iter().any(|v| v == "Bearer upstream-secret"),
        "the request's own headers went out with it"
    );

    let both = get("/?flag=two").await.unwrap();
    assert_eq!(both.status(), StatusCode::OK);
    assert_eq!(
        header(&both, "x-both").as_deref(),
        Some(r#"{"open":true}{"open":true}"#),
        "Promise.all over two calls"
    );

    // The same rule from the other side: an undeclared loopback is refused
    // before a connection is attempted, and the middleware observes it as a
    // rejected fetch it can handle.
    let hits_before = flag_hits.load(Ordering::SeqCst);
    let undeclared = get("/undeclared").await.unwrap();
    assert_eq!(undeclared.status(), StatusCode::BAD_GATEWAY);
    let refusal = undeclared.text().await.unwrap();
    assert!(refusal.contains("egress refused"), "refused by policy, not by a dead port: {refusal}");
    assert_eq!(
        flag_hits.load(Ordering::SeqCst),
        hits_before,
        "the live upstream never saw the undeclared call"
    );

    // ── the outbound budget is charged BEFORE a call leaves ──
    // Two calls per request against a burst of ten: the bucket runs dry within
    // a handful of requests. The refused request must not have reached upstream.
    let mut refused = None;
    for _ in 0..20 {
        let before = flag_hits.load(Ordering::SeqCst);
        let response = get("/?flag=two").await.unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            refused = Some((before, flag_hits.load(Ordering::SeqCst)));
            break;
        }
    }
    let (before, after) = refused.expect("a middleware fanning out must eventually hit its outbound budget");
    assert_eq!(before, after, "a refused fetch() is refused before it is sent");

    // ── failure is loud, and never a continue ──
    let boom = get("/boom").await.unwrap();
    assert_eq!(boom.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let steal = get("/steal").await.unwrap();
    assert_eq!(
        steal.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a middleware may not set the framework's session cookie"
    );
    assert!(steal.headers().get("set-cookie").is_none());
    assert_eq!(get("/").await.unwrap().status(), StatusCode::OK, "control: the server still serves");

    // ── 🔑 it cannot grant: a rewrite onto a gated route is gated ──
    let secret = get("/secret-alias").await.unwrap();
    assert_eq!(secret.status(), StatusCode::UNAUTHORIZED);

    let cookies = sign_up(&client, &base).await;
    let signed_in = client
        .get(format!("{base}/secret-alias"))
        .header("cookie", &cookies)
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), StatusCode::OK, "control: the same rewrite, signed in");
    assert!(signed_in.text().await.unwrap().contains("secret dashboard"));

    // ── the body sees the principal, and never the session token ──
    let seen = client
        .get(format!("{base}/"))
        .header("cookie", &cookies)
        .send()
        .await
        .unwrap();
    assert_eq!(header(&seen, "x-mw").as_deref(), Some("GET / user"));
    assert_eq!(
        header(&seen, "x-mw-cookies").as_deref(),
        Some("theme"),
        "the framework cookies were filtered out of request.cookies"
    );

    drop(server);
    let stderr = fs::read_to_string(app.join("serve.stderr.log")).unwrap_or_default();
    assert!(
        stderr.contains("kaput"),
        "a throwing middleware must reach the terminal, not only tracing:\n{stderr}"
    );
}

/// 🔴 A middleware that never returns must cost one request timeout, not an
/// engine for the life of the process.
///
/// A request timeout used to drop only the future waiting on the engine; the
/// engine's thread went on running the loop. One such request per engine and
/// the pool was gone — every later request that needed an engine (a middleware,
/// a render, an action) waited out its own timeout behind it. So this spins
/// **every** engine at once, and then requires the server to be answering
/// normally within seconds of the timeout passing.
///
/// The request timeout is lowered through `ALBEDO_REQUEST_TIMEOUT_MS`, which is
/// also the engines' job budget. That variable did nothing on `serve` until the
/// environment was layered onto its config, and this test took 16 s waiting out
/// the default — so the spin phase is timed too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_middleware_that_never_returns_costs_a_timeout_not_an_engine() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    run_ok(albedo(home.path(), work.path()).args(["init", "spinapp"]), "albedo init");
    let app = work.path().join("spinapp");
    fs::write(
        app.join("src/middleware.ts"),
        r#"
import { next } from "albedo/middleware";
export default function middleware(request) {
  if (request.path === "/spin") { while (true) {} }
  return next({ headers: { "x-mw": "ok" } });
}
"#,
    )
    .unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");
    let (server, base) =
        serve_with_env(
            home.path(),
            &app,
            &[
                ("ALBEDO_REQUEST_TIMEOUT_MS", "1500"),
                // ~50 requests inside two seconds is a burst SHUTTER rightly
                // refuses from one visitor; each request here is its own.
                ("ALBEDO_TRUSTED_PROXIES", "127.0.0.1/32"),
            ],
        )
        .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let visitor = std::sync::atomic::AtomicU32::new(1);
    let as_visitor = |path: &str| {
        let n = visitor.fetch_add(1, Ordering::Relaxed);
        client
            .get(format!("{base}{path}"))
            .header("x-forwarded-for", format!("10.9.{}.{}", n / 250, n % 250 + 1))
            .send()
    };

    // CONTROL — the server serves through the middleware before anything spins.
    let before = as_visitor("/").await.unwrap();
    assert_eq!(before.status(), StatusCode::OK);
    assert_eq!(header(&before, "x-mw").as_deref(), Some("ok"));

    // One spinning request per engine: `serve` sizes the pool to the host's
    // parallelism, and so does this.
    let engines = std::thread::available_parallelism().map_or(4, |n| n.get());
    let spinning = Instant::now();
    let spins = futures_util::future::join_all(
        (0..engines).map(|_| as_visitor("/spin")),
    )
    .await;
    for spin in spins {
        let spin = spin.expect("a spinning request is answered, not abandoned");
        assert_eq!(
            spin.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a middleware that never returns ends in the request timeout"
        );
    }
    assert!(
        spinning.elapsed() < Duration::from_secs(10),
        "🔴 the spins took {:?} — `ALBEDO_REQUEST_TIMEOUT_MS=1500` was ignored and the 15 s \
         default applied",
        spinning.elapsed()
    );

    // 🔑 The assertion. Every engine was running an endless loop a moment ago;
    // with nothing stopping them, each of these waits out a full request
    // timeout of its own and fails.
    let started = Instant::now();
    let after = futures_util::future::join_all(
        (0..engines).map(|_| as_visitor("/")),
    )
    .await;
    for response in after {
        let response = response.expect("answered");
        assert_eq!(response.status(), StatusCode::OK, "the pool did not recover");
        assert_eq!(header(&response, "x-mw").as_deref(), Some("ok"));
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "🔴 serving after the spins took {:?} — the engines were still running the loops",
        started.elapsed()
    );

    drop(server);
    let stderr = fs::read_to_string(app.join("serve.stderr.log")).unwrap_or_default();
    assert!(
        stderr.contains("ran past the request timeout (1500 ms)"),
        "an interrupted script must reach the terminal:\n{stderr}"
    );
}
