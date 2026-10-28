//! An island renders each request's own `params` and `user` — through the real
//! binary: `init` → `build` → `serve` → GET.
//!
//! 🔴 Reproduced 2026-10-15, fixed 2026-10-16. Islands were rendered once, at
//! boot, from props the build captured with no request in scope, so the page
//! around an island was right per URL and the island was not:
//!
//! - a `[id]` page that is itself an island (`useState` + a handler) rendered
//!   `counter ` on `/counter/7` and `/counter/zz` alike;
//! - `<Picker id={params.id} />` under a correct per-request page rendered
//!   `picker ` under both ids;
//! - `<WhoIsland name={user.id} />` under a page reading `user` rendered
//!   `who anon` to a signed-in reader whose page said their id.
//!
//! The build was green and every response 200. `route_params_http.rs` pins the
//! same consequence for pages; this pins it for islands, on both of the paths an
//! island is served by — binding mode (`useState` + handler) and A3 hydration
//! (`useEffect` sends an island there) — because a fix to one path would pass a
//! test of the other.
//!
//! Each page is asked for under two URLs (or two principals), and each answer is
//! checked for its own value AND for the absence of the other's anywhere in the
//! body — including the scripts the client wires the island from, which is
//! where a boot-time value would survive a correct first paint.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use reqwest::redirect::Policy;

/// Kills `albedo serve` when the test ends, pass or fail — a leaked server
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
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn run(cmd: &mut Command, what: &str) -> (bool, String) {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let (ok, text) = run(cmd, what);
    assert!(ok, "{what} failed\n{text}");
    text
}

/// The page is the island: state and a client handler make it Tier C, served
/// in binding mode.
const COUNTER_PAGE: &str = r#"import { useState } from "react";
export default function CounterPage({ params }) {
  const [n, setN] = useState(0);
  return <main><p id="c">counter {params.id}</p><button onClick={() => setN(n + 1)}>{n}</button></main>;
}
"#;

/// The same, with a mount effect — which sends an island to A3 hydration.
const EFFECT_PAGE: &str = r#"import { useState, useEffect } from "react";
export default function EffectPage({ params }) {
  const [n, setN] = useState(0);
  useEffect(() => { document.title = "fx " + params.id; }, []);
  return <main><p id="f">fx {params.id}</p><button onClick={() => setN(n + 1)}>{n}</button></main>;
}
"#;

/// A binding-mode island under a per-request page, passed the route's id.
const PICKER: &str = r#"import { useState } from "react";
export default function Picker({ id }) {
  const [n, setN] = useState(0);
  return <div><p id="k">picker {id}</p><button onClick={() => setN(n + 1)}>{n}</button></div>;
}
"#;

const PICKER_PAGE: &str = r#"import Picker from "../../components/Picker";
export default function PickerPage({ params }) {
  return <main><p id="pp">page {params.id}</p><Picker id={params.id} /></main>;
}
"#;

/// An A3 island under a per-request page, passed the route's id.
const TICKER: &str = r#"import { useState, useEffect } from "react";
export default function Ticker({ id }) {
  const [n, setN] = useState(0);
  useEffect(() => { document.title = "ticker " + id; }, []);
  return <div><p id="t">ticker {id}</p><button onClick={() => setN(n + 1)}>{n}</button></div>;
}
"#;

const TICKER_PAGE: &str = r#"import Ticker from "../../components/Ticker";
export default function TickerPage({ params }) {
  return <main><p id="tp">page {params.id}</p><Ticker id={params.id} /></main>;
}
"#;

/// An island passed the principal by a page that reads it.
const WHO_ISLAND: &str = r#"import { useState } from "react";
export default function WhoIsland({ name }) {
  const [n, setN] = useState(0);
  return <div><p id="w">who {name}</p><button onClick={() => setN(n + 1)}>{n}</button></div>;
}
"#;

const WHO_PAGE: &str = r#"import WhoIsland from "../components/WhoIsland";
export default function Who({ user }) {
  return <main><p id="wp">page {user ? user.id : "anon"}</p><WhoIsland name={user ? user.id : "anon"} /></main>;
}
"#;

fn write(app: &Path, rel: &str, contents: &str) {
    let path = app.join("src").join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// `albedo init app`; returns (home, work, app).
fn scaffold() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");
    (home, work, app)
}

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    probe.local_addr().expect("addr").port()
}

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    let port = free_port();
    let child = albedo(home, app)
        .args(["serve", ".", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(fs::File::create(app.join("serve.stderr.log")).expect("stderr log"))
        .spawn()
        .expect("albedo serve starts");
    let server = Server { child };
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while reqwest::get(format!("{base}/_albedo/runtime.js")).await.is_err() {
        assert!(Instant::now() < deadline, "albedo serve never answered on {base}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base)
}

async fn page(client: &reqwest::Client, base: &str, path: &str, cookie: Option<&str>) -> String {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|err| panic!("GET {path}: {err}"));
    assert_eq!(response.status().as_u16(), 200, "GET {path}");
    response.text().await.expect("body")
}

/// The text content of the element carrying `id="{id}"`.
fn text_of(html: &str, path: &str, id: &str) -> String {
    let marker = format!(r#"id="{id}""#);
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("GET {path}: no element {marker}\n{html}"));
    let open = at + html[at..].find('>').expect("the element's tag closes") + 1;
    let close = open + html[open..].find("</").expect("the element closes");
    html[open..close].to_string()
}

/// Nothing a client-reference stub wrote may reach a browser: the serve pass
/// replaces every placeholder that carried props.
fn assert_no_carried_placeholder(html: &str, path: &str) {
    assert!(
        !html.contains(r#"data-albedo-tier="c" data-albedo-props"#),
        "GET {path}: a placeholder's carried props reached the page\n{html}"
    );
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn cookie_named(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with(&format!("{name}=")))
        .map(str::to_string)
}

/// Register a fresh account through the scaffold's own form and return the
/// cookies a browser would then hold.
async fn sign_up(client: &reqwest::Client, base: &str, email: &str) -> String {
    let form = client
        .get(format!("{base}/sign-in"))
        .send()
        .await
        .expect("the sign-in page responds");
    let tab = cookie_named(&form, "__Host-albedo-session").expect("the render mints a tab cookie");
    let html = form.text().await.expect("body");
    let csrf = hidden_field(&html, "_csrf").expect("the form carries a CSRF token");
    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[
            ("_csrf", csrf.as_str()),
            ("_albedo_return", "/who"),
            ("email", email),
            // A throwaway credential for an account that lives in this temp dir.
            ("password", "island props test passphrase"),
        ])
        .send()
        .await
        .expect("register responds");
    let session = cookie_named(&registered, "__Host-albedo_session")
        .unwrap_or_else(|| panic!("registering {email} signs it in ({})", registered.status()));
    format!("{tab}; {session}")
}

#[tokio::test(flavor = "multi_thread")]
async fn every_island_renders_its_own_requests_params_and_user() {
    let (home, _work, app) = scaffold();
    write(&app, "routes/counter/[id].tsx", COUNTER_PAGE);
    write(&app, "routes/fx/[id].tsx", EFFECT_PAGE);
    write(&app, "components/Picker.tsx", PICKER);
    write(&app, "routes/picker/[id].tsx", PICKER_PAGE);
    write(&app, "components/Ticker.tsx", TICKER);
    write(&app, "routes/ticker/[id].tsx", TICKER_PAGE);
    write(&app, "components/WhoIsland.tsx", WHO_ISLAND);
    write(&app, "routes/who.tsx", WHO_PAGE);

    // The fixture is what it claims to be: every one of these is an island.
    // Were tiering to stop making them islands this test would pass while
    // testing nothing, so it says so instead.
    let built = run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    for island in ["CounterPage", "EffectPage", "Picker", "Ticker", "WhoIsland"] {
        let line = built
            .lines()
            .map(str::trim)
            .find(|line| line.split_whitespace().nth(1) == Some(island))
            .unwrap_or_else(|| panic!("no tier line for {island}:\n{built}"));
        assert!(line.starts_with("C "), "{island} must be an island: {line}");
    }

    let (_server, base) = serve(home.path(), &app).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // (element id, URL, its text, the other URL, its text)
    let cases = [
        ("c", "/counter/7", "counter 7", "/counter/zz", "counter zz"),
        ("f", "/fx/7", "fx 7", "/fx/zz", "fx zz"),
        ("k", "/picker/7", "picker 7", "/picker/zz", "picker zz"),
        ("t", "/ticker/7", "ticker 7", "/ticker/zz", "ticker zz"),
    ];
    for (id, first, first_text, second, second_text) in cases {
        let one = page(&client, &base, first, None).await;
        let two = page(&client, &base, second, None).await;
        assert_eq!(text_of(&one, first, id), first_text, "GET {first}");
        assert_eq!(text_of(&two, second, id), second_text, "GET {second}");
        // Anywhere in the body — a binding-mode island is re-painted from the
        // HTML its `boot(…)` script carries, so a boot-time value surviving
        // there would undo a correct first paint on the first click.
        assert!(!one.contains(second_text), "GET {first} carries {second_text:?}");
        assert!(!two.contains(first_text), "GET {second} carries {first_text:?}");
        assert_no_carried_placeholder(&one, first);
        assert_no_carried_placeholder(&two, second);
    }

    // A binding-mode island is wired by exactly one script — the one rendered
    // for this request, not that one beside the boot render's.
    let counter = page(&client, &base, "/counter/7", None).await;
    assert_eq!(counter.matches("__albedoReactive.boot(").count(), 1, "{counter}");
    assert!(counter.contains(r#"counter 7<\/p>"#), "the install carries this request's markup");

    // An A3 island hydrates from the props on its root, which are this
    // request's; the payload's are the boot render's, shared by every visitor.
    let ticker = page(&client, &base, "/ticker/zz", None).await;
    assert!(
        ticker.contains(r#"data-albedo-props="{&quot;id&quot;:&quot;zz&quot;}""#),
        "the island root carries this request's props\n{ticker}"
    );
    let effect = page(&client, &base, "/fx/7", None).await;
    assert!(
        effect.contains(r#"data-albedo-props="{&quot;params&quot;:{&quot;id&quot;:&quot;7&quot;}}""#),
        "a page that is an island carries this request's params\n{effect}"
    );

    // `user`, three ways: anonymous and two different people.
    let anonymous = page(&client, &base, "/who", None).await;
    assert_eq!(text_of(&anonymous, "/who", "w"), "who anon");
    let ada = sign_up(&client, &base, "ada@example.test").await;
    let bob = sign_up(&client, &base, "bob@example.test").await;
    let as_ada = page(&client, &base, "/who", Some(&ada)).await;
    let as_bob = page(&client, &base, "/who", Some(&bob)).await;
    let ada_id = text_of(&as_ada, "/who", "wp").trim_start_matches("page ").to_string();
    let bob_id = text_of(&as_bob, "/who", "wp").trim_start_matches("page ").to_string();
    assert!(ada_id.starts_with("u_") && bob_id.starts_with("u_"), "{ada_id} / {bob_id}");
    assert_ne!(ada_id, bob_id, "two accounts, two principals");
    assert_eq!(text_of(&as_ada, "/who", "w"), format!("who {ada_id}"));
    assert_eq!(text_of(&as_bob, "/who", "w"), format!("who {bob_id}"));
    assert!(!as_ada.contains(&bob_id), "Ada's page carries Bob's id");
    assert!(!as_bob.contains(&ada_id), "Bob's page carries Ada's id");
    assert!(!anonymous.contains(&ada_id) && !anonymous.contains(&bob_id));
    for (html, who) in [(&anonymous, "anonymous"), (&as_ada, "ada"), (&as_bob, "bob")] {
        assert_no_carried_placeholder(html, &format!("/who as {who}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_layout_that_reads_params_or_user_is_refused_by_name() {
    let (home, _work, app) = scaffold();
    write(
        &app,
        "routes/shop/[cat]/index.tsx",
        "export default function ShopPage({ params }) { return <main>{params.cat}</main>; }\n",
    );
    write(
        &app,
        "routes/shop/[cat]/layout.tsx",
        "export default function ShopLayout({ params, children }) {\n  \
         return <section>{params ? params.cat : \"none\"}{children}</section>;\n}\n",
    );
    write(
        &app,
        "routes/account/layout.tsx",
        "export default function AccountLayout(props) {\n  \
         return <section>{props.user ? props.user.id : \"anon\"}{props.children}</section>;\n}\n",
    );
    write(
        &app,
        "routes/account/index.tsx",
        "export default function Account() { return <main>account</main>; }\n",
    );

    let (ok, out) = run(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    assert!(!ok, "a layout reading the request must fail the build\n{out}");
    assert!(out.contains("a layout cannot read `params` or `user`"), "{out}");
    assert!(
        out.contains("shop/[cat]/layout.tsx (ShopLayout) reads route `params`"),
        "the refusal names the layout and the prop\n{out}"
    );
    assert!(
        out.contains("account/layout.tsx (AccountLayout) reads `user`"),
        "a read through the props object is a read\n{out}"
    );

    // An opaque `props` that never reads either is not refused — and the tier
    // report no longer calls a layout "rendered per request", which no layout
    // ever is.
    write(
        &app,
        "routes/shop/[cat]/layout.tsx",
        "export default function ShopLayout(props) {\n  \
         return <section>{props.children}</section>;\n}\n",
    );
    write(
        &app,
        "routes/account/layout.tsx",
        "export default function AccountLayout({ children }) {\n  \
         return <section>{children}</section>;\n}\n",
    );
    let built = run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    let line = built
        .lines()
        .map(str::trim)
        .find(|line| line.split_whitespace().nth(1) == Some("ShopLayout"))
        .unwrap_or_else(|| panic!("no tier line for ShopLayout:\n{built}"));
    assert!(!line.contains("per request"), "{line}");
}
