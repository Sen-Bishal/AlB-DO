//! A `[segment]` route answers each URL with that URL's `params` — through the
//! real binary: `init` → `build` → `serve` → GET.
//!
//! 🔴 Reproduced 2026-09-28, fixed 2026-10-07: a dynamic page with no hooks, no
//! `async` and no handler was Tier A, rendered ONCE at build time with `params`
//! as `{}`. `/post/7` and `/post/hello` were the same page and neither id was in
//! it; a catch-all rendered `null`. The build stayed green and every URL served
//! 200, so nothing reported it. `tests/route_params_tier.rs` pins the tier the
//! compiler picks; this pins the consequence a reader actually sees, because a
//! right tier with the prop left unresolved would pass that test and still
//! serve every URL the same page.
//!
//! Each page is asked for under two different URLs, and each answer is checked
//! for its own value AND for the absence of the other's: a server that baked
//! either value once would pass a one-URL check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{what} failed ({:?})\n{text}", out.status.code());
    text
}

/// The page the brief reproduced the bug with, verbatim.
const POST: &str = r#"export default function Post({ params }) {
  return <main><p id="p">post {params.id}</p></main>;
}
"#;

/// `props.params` — the signature scan cannot see which keys `props` is read
/// for, so it must round toward "reads it".
const VIA_PROPS: &str = r#"export default function ViaProps(props) {
  return <main><p id="v">via {props.params.id}</p></main>;
}
"#;

const FILES: &str = r#"export default function Files({ params }) {
  return <main><p id="f">{JSON.stringify(params)}</p></main>;
}
"#;

const SHOP: &str = r#"export default function Shop({ params }) {
  return <main><p id="s">{JSON.stringify(params)}</p></main>;
}
"#;

/// A decoded value in the two other places a page puts one: an attribute, and a
/// link back to itself built the way an app builds links.
const LINK: &str = r#"export default function Link({ params }) {
  return <main><a id="l" title={params.id} href={"/link/" + encodeURIComponent(params.id)}>{params.id}</a></main>;
}
"#;

fn write(app: &Path, rel: &str, contents: &str) {
    let path = app.join("src/routes").join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// `albedo init app` plus the five dynamic pages; returns (home, work, app).
fn project() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");
    write(&app, "post/[id].tsx", POST);
    write(&app, "viaprops/[id].tsx", VIA_PROPS);
    write(&app, "files/[...path].tsx", FILES);
    write(&app, "shop/[[...filter]].tsx", SHOP);
    write(&app, "link/[id].tsx", LINK);
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

/// The body of a 200 response to `path`, sent exactly as written — `reqwest`
/// keeps percent-escapes as given, so `%2F` reaches the server as `%2F`.
async fn page(base: &str, path: &str) -> String {
    let response = reqwest::get(format!("{base}{path}"))
        .await
        .unwrap_or_else(|err| panic!("GET {path}: {err}"));
    assert_eq!(response.status().as_u16(), 200, "GET {path}");
    response.text().await.expect("body")
}

/// The element carrying `id="{id}"`: its opening tag, and its text content.
fn element<'a>(html: &'a str, path: &str, id: &str) -> (&'a str, &'a str) {
    let marker = format!(r#"id="{id}""#);
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("GET {path}: no element {marker}\n{html}"));
    let tag = html[..at].rfind('<').expect("the element's tag opens");
    let open = at + html[at..].find('>').expect("the element's tag closes") + 1;
    let close = open + html[open..].find("</").expect("the element closes");
    (&html[tag..open], &html[open..close])
}

/// The text content of the element carrying `id="{id}"` in a 200 response.
async fn text_of(base: &str, path: &str, id: &str) -> String {
    let html = page(base, path).await;
    element(&html, path, id).1.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn each_url_of_a_dynamic_route_renders_its_own_params() {
    let (home, _work, app) = project();

    // The build names the reason in its tier report — `params`, not `user`.
    let built = run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    for page in ["Post", "ViaProps", "Files", "Shop", "Link"] {
        let line = built
            .lines()
            .map(str::trim)
            .find(|line| line.split_whitespace().nth(1) == Some(page))
            .unwrap_or_else(|| panic!("no tier line for {page}:\n{built}"));
        assert!(
            line.starts_with("B ") && line.contains("reads route `params`"),
            "{page} must be rendered per request, for `params`: {line}"
        );
    }

    let (_server, base) = serve(home.path(), &app).await;

    // Two URLs per page; each answer has its own value and not the other's.
    let cases = [
        ("p", "/post/7", "post 7", "/post/hello", "post hello"),
        ("v", "/viaprops/7", "via 7", "/viaprops/zz", "via zz"),
        // A rest segment arrives as ONE string joined with `/` (docs/routing).
        (
            "f",
            "/files/a/b",
            r#"{"path":"a/b"}"#,
            "/files/x",
            r#"{"path":"x"}"#,
        ),
        // An optional rest segment that matched nothing is absent.
        (
            "s",
            "/shop",
            "{}",
            "/shop/red/large",
            r#"{"filter":"red/large"}"#,
        ),
    ];
    for (id, first, first_text, second, second_text) in cases {
        let one = text_of(&base, first, id).await;
        let two = text_of(&base, second, id).await;
        // JSON.stringify output is text content, so its quotes stay literal and
        // nothing in these values needs escaping.
        assert_eq!(one, first_text, "GET {first}");
        assert_eq!(two, second_text, "GET {second}");
        assert_ne!(one, two, "{first} and {second} must be different pages");
    }

    // A value from the URL reaches the page as text, escaped like any other.
    assert_eq!(text_of(&base, "/post/a&b", "p").await, "post a&amp;b");

    // 🔴 2026-10-15: params arrived still percent-encoded — `/post/caf%C3%A9`
    // gave `params.id === "caf%C3%A9"`, so a page looking up a non-ASCII slug
    // missed. Each captured segment is now decoded once the literal `/`s have
    // chosen the route.
    let decoded = [
        ("p", "/post/caf%C3%A9", "post café"),
        // Decoding is what lets `<` reach a page at all — hyper answers 400 to
        // a raw one — so the decoded value must still be escaped text.
        ("p", "/post/%3Cscript%3E", "post &lt;script&gt;"),
        // `%2F` decodes to `/` inside the one segment `[id]` matched; it does
        // not make the URL a two-segment path.
        ("p", "/post/a%2Fb", "post a/b"),
        // A rest capture decodes each segment and rejoins them with `/`, so an
        // encoded slash and a real one give the same string.
        ("f", "/files/a%2Fb/caf%C3%A9", r#"{"path":"a/b/café"}"#),
    ];
    for (id, path, expected) in decoded {
        assert_eq!(text_of(&base, path, id).await, expected, "GET {path}");
    }

    // The same decoded value in an attribute and in a link the page builds back
    // to itself: escaped in the one, and `encodeURIComponent` of it is the URL
    // that was asked for — the round trip a `%2F` left encoded would break, and
    // so would decoding twice (`%2541` is `%41` once, `A` twice).
    let hostile = "/link/%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E%2541";
    let html = page(&base, hostile).await;
    let (tag, text) = element(&html, hostile, "l");
    // In text a `"` is harmless and stays literal; in the attribute it is not.
    assert_eq!(
        text, r#""&gt;&lt;script&gt;alert(1)&lt;/script&gt;%41"#,
        "GET {hostile}: text"
    );
    assert!(
        tag.contains(r#"title="&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;%41""#),
        "GET {hostile}: title attribute: {tag}"
    );
    assert!(
        tag.contains(&format!(r#"href="{hostile}""#)),
        "GET {hostile}: href attribute: {tag}"
    );
    assert!(!html.contains("<script>alert(1)"), "GET {hostile}:\n{html}");

    // A segment whose bytes are not UTF-8 has no string to become, so it is
    // refused by name rather than handed over raw or as U+FFFD.
    let refused = reqwest::get(format!("{base}/post/%FF"))
        .await
        .expect("GET /post/%FF");
    assert_eq!(refused.status().as_u16(), 400, "GET /post/%FF");
    let body = refused.text().await.expect("body");
    assert!(body.contains("route param `id`"), "GET /post/%FF: {body}");
}
