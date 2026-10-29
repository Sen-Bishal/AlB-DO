//! The 2026-10-27 audit's critical and high findings, re-run through the real
//! binary: `init` → `build` → `serve` → HTTP, and `ship` for the last one.
//!
//! Each finding was *proved live* by the audit, not read in code, so each is
//! pinned the same way — against a fresh starter app, over the wire, with a
//! positive control beside every refusal so a dead lane or a broken action
//! cannot pass for a fix.
//!
//! - **ALB-01** · a public route's live lane carried the rows of a collection
//!   only an `auth = "required"` route reads (every route was granted every
//!   topic).
//! - **ALB-02** · any visitor could remove any row of a collection that is not
//!   identity-partitioned.
//! - **ALB-03** · a stored `javascript:` URL rendered into an `href` verbatim.
//! - **ALB-04** · `albedo ship` packed `.env` into the binary in plain text.

use base64::Engine as _;
use reqwest::StatusCode;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SECRET: &str = "GATED-ONLY-SECRET-4409";
const NOTICE: &str = "PUBLIC-NOTICE-7731";

/// Kills `albedo serve` when the test ends, pass or fail.
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

fn output(cmd: &mut Command, what: &str) -> (Output, String) {
    let out = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, text)
}

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let (out, text) = output(cmd, what);
    assert!(out.status.success(), "{what} failed ({:?})\n{text}", out.status.code());
    text
}

fn write(app: &Path, rel: &str, contents: &str) {
    let path = app.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Read only by a gated route.
const STAFF: &str = r#"import { useSharedSlot } from "albedo";

export const auth = "required";

export default function Staff() {
  const notes = useSharedSlot("staff_notes");
  return <ul>{notes.map((n) => <li key={n.id}>{n.note}</li>)}</ul>;
}
"#;

/// The positive control for ALB-01: a public route's own topic still streams.
const NOTICES: &str = r#"import { useSharedSlot } from "albedo";

export default function Notices() {
  const notices = useSharedSlot("notices");
  return <ul>{notices.map((n) => <li key={n.id}>{n.body}</li>)}</ul>;
}
"#;

/// A visitor-supplied URL in an href, and an action that removes by id — the
/// audit's ALB-02 and ALB-03 shapes on one page.
const LINKS: &str = r#"import { action, useSharedSlot } from "albedo";

export const add_link = action(({ form }) =>
  append("guestbook", { author: form.author, message: form.message }),
);

export const drop_link = action(({ form }) => remove("guestbook", form.id));

export default function Links() {
  const entries = useSharedSlot("guestbook");
  return (
    <section>
      <ul>
        {entries.map((e) => (
          <li key={e.id}>
            <a href={e.author}>{e.id}:{e.message}</a>
          </li>
        ))}
      </ul>
      <form action="action:add_link" method="POST">
        <input name="author" />
        <input name="message" />
        <button type="submit">add</button>
      </form>
      <form action="action:drop_link" method="POST">
        <input name="id" />
        <button type="submit">drop</button>
      </form>
    </section>
  );
}
"#;

/// `albedo init app`, two seeded collections, and the three probe routes.
fn project() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");

    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    let collections = format!(
        "forge: {{\n    staff_notes: {{ fields: {{ note: \"text\" }}, seed: [{{ note: \"{SECRET}\" }}] }},\n    \
         notices: {{ fields: {{ body: \"text\" }}, seed: [{{ body: \"{NOTICE}\" }}] }},\n"
    );
    assert!(config.contains("forge: {"), "the starter config declares a forge block");
    fs::write(&config_path, config.replacen("forge: {", &collections, 1)).expect("config");

    write(&app, "src/routes/staff.tsx", STAFF);
    write(&app, "src/routes/notices.tsx", NOTICES);
    write(&app, "src/routes/links.tsx", LINKS);
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// Every frame `/_albedo/patches?p={page}` sends in its first moments,
/// base64-decoded and concatenated. `Last-Event-ID` asks for the full resync,
/// which is what the audit's anonymous `curl` received.
async fn lane_bytes(base: &str, page: &str) -> Vec<u8> {
    let mut response = client()
        .get(format!("{base}/_albedo/patches?p={page}"))
        .header("last-event-id", "0")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("the lane answers");
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, response.chunk()).await {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let mut bytes = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data.trim()) {
                bytes.extend(decoded);
            }
        }
    }
    bytes
}

fn carries(bytes: &[u8], marker: &str) -> bool {
    bytes.windows(marker.len()).any(|window| window == marker.as_bytes())
}

/// What a browser holds after loading `/links`: its tab cookie and the hidden
/// fields the renderer stamped into the forms.
struct Visitor {
    cookie: String,
    csrf: String,
    return_path: String,
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn cookie_named(response: &reqwest::Response, prefix: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with(prefix))
        .map(str::to_string)
}

async fn arrive(base: &str) -> Visitor {
    let response = client()
        .get(format!("{base}/links"))
        .send()
        .await
        .expect("the page answers");
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = cookie_named(&response, "__Host-albedo-session=").expect("a tab cookie");
    let html = response.text().await.expect("html");
    Visitor {
        cookie,
        csrf: hidden_field(&html, "_csrf").expect("a CSRF token"),
        return_path: hidden_field(&html, "_albedo_return").expect("a return path"),
    }
}

async fn act(base: &str, visitor: &Visitor, cookie: &str, action: &str, fields: &[(&str, &str)]) -> StatusCode {
    let mut form = vec![
        ("_csrf", visitor.csrf.as_str()),
        ("_albedo_return", visitor.return_path.as_str()),
    ];
    form.extend_from_slice(fields);
    client()
        .post(format!("{base}/_albedo/action/{action}"))
        .header("cookie", cookie)
        .form(&form)
        .send()
        .await
        .expect("the action answers")
        .status()
}

async fn links_page(base: &str) -> String {
    client()
        .get(format!("{base}/links"))
        .send()
        .await
        .expect("the page answers")
        .text()
        .await
        .expect("html")
}

/// The row id the page renders before `message`, as `{id}:{message}`.
fn row_id(html: &str, message: &str) -> Option<String> {
    let at = html.find(&format!(":{message}<"))?;
    let digits: String = html[..at]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then(|| digits.chars().rev().collect())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_audits_live_findings_stay_fixed() {
    let (home, _work, app) = project();
    run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    let (_server, base) = serve(home.path(), &app).await;

    // ── ALB-01 · a public lane carries only its own route's topics ─────────
    let notices = lane_bytes(&base, "/notices").await;
    assert!(
        carries(&notices, NOTICE),
        "control: a public route's own rows still stream (the lane works)"
    );
    for page in ["/", "/notices", "/sign-in", "/links", "/staff"] {
        let bytes = lane_bytes(&base, page).await;
        assert!(
            !carries(&bytes, SECRET),
            "an anonymous lane on {page} carried a row only `/staff` (auth required) reads"
        );
    }
    assert!(
        !carries(&lane_bytes(&base, "/").await, NOTICE),
        "`/` does not render `notices`, so its lane must not carry them"
    );

    // ── ALB-03 · a stored script URL never reaches an href ─────────────────
    let alice = arrive(&base).await;
    act(&base, &alice, &alice.cookie, "add_link", &[
        ("author", " JaVa\tScRiPt:alert(document.cookie)"),
        ("message", "HOSTILE-ROW"),
    ])
    .await;
    act(&base, &alice, &alice.cookie, "add_link", &[
        ("author", "https://alice.example/"),
        ("message", "ALICE-ROW"),
    ])
    .await;
    let page = links_page(&base).await;
    assert!(page.contains(":HOSTILE-ROW<"), "control: the row was written\n{page}");
    assert!(!page.contains("alert(document.cookie)"), "the payload reached the page\n{page}");
    assert!(page.contains("ALBEDO blocked a javascript: URL"), "replaced, not dropped\n{page}");
    assert!(page.contains(r#"href="https://alice.example/""#), "an ordinary URL is untouched");

    // ── ALB-02 · a stranger cannot remove someone else's row ───────────────
    let alice_row = row_id(&page, "ALICE-ROW").expect("alice's row id is on the page");
    let mallory = arrive(&base).await;
    let refused = act(&base, &mallory, &mallory.cookie, "drop_link", &[("id", &alice_row)]).await;
    assert!(
        links_page(&base).await.contains(":ALICE-ROW<"),
        "a stranger removed another visitor's row (status {refused})"
    );

    // …while a signed-in author can remove their own — the control that the
    // refusal above is the rule, not a broken action.
    let author = arrive(&base).await;
    let registered = client()
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &author.cookie)
        .form(&[
            ("_csrf", author.csrf.as_str()),
            ("_albedo_return", author.return_path.as_str()),
            ("email", "author@example.com"),
            ("password", "correct horse battery staple"),
        ])
        .send()
        .await
        .expect("register answers");
    let session = cookie_named(&registered, "__Host-albedo_session=").expect("registering signs in");
    let signed_in = format!("{}; {session}", author.cookie);
    act(&base, &author, &signed_in, "add_link", &[
        ("author", "https://author.example/"),
        ("message", "OWNED-ROW"),
    ])
    .await;
    let page = links_page(&base).await;
    let owned = row_id(&page, "OWNED-ROW").expect("the author's row id is on the page");
    let refused = act(&base, &author, &signed_in, "drop_link", &[("id", &alice_row)]).await;
    assert!(
        links_page(&base).await.contains(":ALICE-ROW<"),
        "a signed-in stranger removed an anonymous row (status {refused})"
    );
    act(&base, &author, &signed_in, "drop_link", &[("id", &owned)]).await;
    assert!(
        !links_page(&base).await.contains(":OWNED-ROW<"),
        "control: the author removes their own row"
    );
}

/// ALB-04 · a key in `.env` never travels, and a key in `public/` stops the
/// ship by name before anything is written.
#[test]
fn ship_never_packs_a_secret() {
    const KEY: &str = "sk_live_PROBE_MARKER_7731";
    let (home, _work, app) = project();
    fs::write(app.join(".env"), format!("STRIPE_SECRET_KEY={KEY}\n")).unwrap();

    // A secret in public/ is refused for every target, by name.
    write(&app, "public/.env", &format!("STRIPE_SECRET_KEY={KEY}\n"));
    let (out, text) = output(
        albedo(home.path(), &app).args(["ship", ".", "--target", "binary"]),
        "albedo ship (refused)",
    );
    assert!(!out.status.success(), "a key in public/ must stop the ship:\n{text}");
    assert!(text.contains("public/.env"), "the refusal names the file:\n{text}");
    fs::remove_file(app.join("public/.env")).unwrap();

    // The root .env is where it belongs, and it stays there.
    let text = run_ok(
        albedo(home.path(), &app).args(["ship", ".", "--target", "binary"]),
        "albedo ship --target binary",
    );
    let binary = fs::read_dir(&app)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".albedo-bin"))
        })
        .unwrap_or_else(|| panic!("ship wrote no binary:\n{text}"));
    let bytes = fs::read(&binary).unwrap();
    assert!(
        !bytes.windows(KEY.len()).any(|window| window == KEY.as_bytes()),
        "the shipped binary carries the .env key in plain text"
    );
}

/// ALB-01's fix is a walk over everything a route renders, so pin the shapes a
/// walk can miss: a reader nested under a Tier-A parent (the manifest's node
/// lists leave those out), a reader that is a *named* export (a default-export
/// lookup has nothing to resolve), and a reader the layout renders (which every
/// route renders). A route missing one of these would render correct HTML and
/// then never update — the quiet failure, so it is asserted from the manifest.
#[test]
fn each_route_is_granted_exactly_the_topics_it_renders() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");

    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    fs::write(
        &config_path,
        config.replacen(
            "forge: {",
            "forge: {\n    alerts: { fields: { text: \"text\" } },\n    tally: { fields: { n: \"int\" } },\n",
            1,
        ),
    )
    .expect("config");
    write(
        &app,
        "src/components/Count.tsx",
        "import { useSharedSlot } from \"albedo\";\nexport function Count() {\n  const tally = useSharedSlot(\"tally\");\n  return <span>{tally.length}</span>;\n}\n",
    );
    write(
        &app,
        "src/components/Panel.tsx",
        "import { Count } from \"./Count\";\nexport default function Panel() {\n  return <aside><p>panel</p><Count /></aside>;\n}\n",
    );
    write(
        &app,
        "src/routes/nested.tsx",
        "import Panel from \"../components/Panel\";\nexport default function Nested() {\n  return <main><Panel /></main>;\n}\n",
    );
    write(
        &app,
        "src/components/Banner.tsx",
        "import { useSharedSlot } from \"albedo\";\nexport default function Banner() {\n  const alerts = useSharedSlot(\"alerts\");\n  return <div>{alerts.map((a) => <p key={a.id}>{a.text}</p>)}</div>;\n}\n",
    );
    let layout_path = app.join("src/routes/layout.tsx");
    let layout = fs::read_to_string(&layout_path).expect("layout");
    assert!(layout.contains("<children />"), "the starter layout has a children slot");
    fs::write(
        &layout_path,
        format!(
            "import Banner from \"../components/Banner\";\n{}",
            layout.replacen("<children />", "<Banner /><children />", 1)
        ),
    )
    .expect("layout");

    run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(app.join(".albedo/dist/render-manifest.v2.json")).expect("manifest"),
    )
    .expect("manifest is JSON");
    let topics = |route: &str| -> Vec<String> {
        let routes = &manifest["routes"];
        let entry = routes
            .get(route)
            .cloned()
            .or_else(|| {
                routes
                    .as_array()?
                    .iter()
                    .find(|entry| entry["route"] == route)
                    .cloned()
            })
            .unwrap_or_else(|| panic!("no route {route} in the manifest"));
        serde_json::from_value(entry["shared_slot_topics"].clone()).expect("a topic list")
    };

    assert_eq!(topics("/"), vec!["alerts"], "the layout's reader, and nothing a sibling page reads");
    assert_eq!(topics("/guestbook"), vec!["alerts", "guestbook"]);
    assert_eq!(
        topics("/nested"),
        vec!["alerts", "tally"],
        "a named-export reader under a Tier-A parent is rendered here, so it is granted here"
    );
}

// ── Column projection ──────────────────────────────────────────────────────

const PRIVATE_EMAIL: &str = "ada-private@probe.test";

/// Reads the collection for its count and no column.
const COUNT: &str = r#"import { useSharedSlot } from "albedo";

export default function Count() {
  const waitlist = useSharedSlot("waitlist");
  return <p>{waitlist.length} on the list</p>;
}
"#;

/// Reads names only — and takes sign-ups, which write emails it never shows.
const NAMES: &str = r#"import { action, useSharedSlot } from "albedo";

export const join = action(({ form }) =>
  append("waitlist", { name: form.name, email: form.email }),
);

export default function Names() {
  const waitlist = useSharedSlot("waitlist");
  return (
    <section>
      <ul>{waitlist.map((w) => <li key={w.id}>{w.name}</li>)}</ul>
      <form action="action:join" method="POST">
        <input name="name" />
        <input name="email" />
        <button type="submit">join</button>
      </form>
    </section>
  );
}
"#;

/// The one page that renders the emails, behind a sign-in.
const ADMIN: &str = r#"import { useSharedSlot } from "albedo";

export const auth = "required";

export default function Admin() {
  const waitlist = useSharedSlot("waitlist");
  return <ul>{waitlist.map((w) => <li key={w.id}>{w.email}</li>)}</ul>;
}
"#;

/// An open lane, read in windows, so a write can land between two reads.
async fn open_lane(base: &str, page: &str) -> reqwest::Response {
    client()
        .get(format!("{base}/_albedo/patches?p={page}"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("the lane answers")
}

async fn read_for(response: &mut reqwest::Response, window: Duration) -> (usize, Vec<u8>) {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, response.chunk()).await {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let mut frames = 0;
    let mut bytes = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data.trim()) {
                frames += 1;
                bytes.extend(decoded);
            }
        }
    }
    (frames, bytes)
}

/// A page's live lane carries the columns that page renders, and no others —
/// on the seed every tab joins with, and on every later write.
///
/// 🔴 Before: `/count` renders a number and its lane carried every email on
/// the list; `/names` renders names and carried them too. Both are public.
#[tokio::test(flavor = "multi_thread")]
async fn a_lane_carries_only_the_columns_its_page_renders() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    fs::write(
        &config_path,
        config.replacen(
            "forge: {",
            &format!(
                "forge: {{\n    waitlist: {{ fields: {{ name: \"text\", email: \"text\" }}, \
                 seed: [{{ name: \"Ada\", email: \"{PRIVATE_EMAIL}\" }}] }},\n"
            ),
            1,
        ),
    )
    .expect("config");
    write(&app, "src/routes/count.tsx", COUNT);
    write(&app, "src/routes/names.tsx", NAMES);
    write(&app, "src/routes/admin.tsx", ADMIN);
    run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    let (_server, base) = serve(home.path(), &app).await;

    // ── the seed ──
    let mut count = open_lane(&base, "/count").await;
    let mut names = open_lane(&base, "/names").await;
    let (count_frames, count_seed) = read_for(&mut count, Duration::from_millis(800)).await;
    let (_, names_seed) = read_for(&mut names, Duration::from_millis(800)).await;
    assert!(count_frames > 0, "control: /count's lane seeds");
    assert!(carries(&names_seed, "Ada"), "control: /names is sent the names it renders");
    assert!(!carries(&names_seed, PRIVATE_EMAIL), "/names was sent an email it never renders");
    assert!(!carries(&count_seed, PRIVATE_EMAIL), "/count was sent an email it never renders");
    assert!(!carries(&count_seed, "Ada"), "/count renders a number, not names");

    // The pages themselves never carried it either.
    for page in ["/count", "/names"] {
        let html = client()
            .get(format!("{base}{page}"))
            .send()
            .await
            .expect("page")
            .text()
            .await
            .expect("html");
        assert!(!html.contains(PRIVATE_EMAIL), "{page}'s HTML carries an email");
    }

    // ── a later write ──
    let response = client()
        .get(format!("{base}/names"))
        .send()
        .await
        .expect("page");
    let cookie = cookie_named(&response, "__Host-albedo-session=").expect("a tab cookie");
    let html = response.text().await.expect("html");
    let joined = client()
        .post(format!("{base}/_albedo/action/join"))
        .header("cookie", &cookie)
        .form(&[
            ("_csrf", hidden_field(&html, "_csrf").expect("csrf").as_str()),
            ("_albedo_return", hidden_field(&html, "_albedo_return").expect("return").as_str()),
            ("name", "Eve"),
            ("email", "eve-private@probe.test"),
        ])
        .send()
        .await
        .expect("join answers");
    assert!(joined.status().is_redirection() || joined.status().is_success(), "join: {}", joined.status());

    let (count_after, count_live) = read_for(&mut count, Duration::from_millis(1500)).await;
    let (_, names_live) = read_for(&mut names, Duration::from_millis(1500)).await;
    assert!(count_after > 0, "control: the write reached /count's lane");
    assert!(carries(&names_live, "Eve"), "control: the write reached /names with the new name");
    assert!(!carries(&names_live, "eve-private"), "/names was sent the new email");
    assert!(!carries(&count_live, "eve-private"), "/count was sent the new email");
    assert!(!carries(&count_live, "Eve"), "/count was sent the new name");
}

// ── 10-29 review #1 · a middleware refusal reaches the live lanes ──────────

const PAYROLL: &str = "ADMIN-ONLY-PAYROLL-5150";
const ADMIN_EMAIL: &str = "admin@example.com";

/// A role check — the one gate `export const auth` cannot express, so the one
/// an app has to write in middleware.
const ROLE_MIDDLEWARE: &str = r#"import { next, respond } from "albedo/middleware";

export default function middleware(request, { user }) {
  if (request.path.startsWith("/payroll") && (!user || user.email !== "admin@example.com")) {
    return respond("admins only", { status: 403 });
  }
  return next();
}
"#;

/// The only route that reads `payroll`, so a lane carrying it can only have
/// been granted this route.
const PAYROLL_PAGE: &str = r#"import { useSharedSlot } from "albedo";

export const auth = "required";

export default function Payroll() {
  const rows = useSharedSlot("payroll");
  return <ul>{rows.map((r) => <li key={r.id}>{r.line}</li>)}</ul>;
}
"#;

/// `/_albedo/patches?p={page}`'s first moments, sent as `cookie`.
async fn lane_bytes_as(base: &str, page: &str, cookie: &str) -> Vec<u8> {
    let mut response = client()
        .get(format!("{base}/_albedo/patches?p={page}"))
        .header("cookie", cookie)
        .header("last-event-id", "0")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("the lane answers");
    read_for(&mut response, Duration::from_millis(1500)).await.1
}

/// Open a PHOSPHOR trunk as `cookie`, subscribe it to `page`, and return the
/// routes the server denied and every frame the trunk then carried.
async fn phosphor_as(base: &str, page: &str, cookie: &str) -> (Vec<String>, Vec<u8>) {
    let mut trunk = client()
        .get(format!("{base}/_albedo/phosphor"))
        .header("cookie", cookie)
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("the trunk opens");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    let lane = loop {
        if let Some(at) = text.find(r#""lane":""#) {
            let rest = &text[at + 8..];
            if let Some(end) = rest.find('"') {
                break rest[..end].to_string();
            }
        }
        let chunk = tokio::time::timeout_at(deadline, trunk.chunk())
            .await
            .expect("the trunk says hello in time")
            .expect("the trunk reads")
            .expect("the trunk stays open");
        text.push_str(&String::from_utf8_lossy(&chunk));
    };

    let body = serde_json::json!({ "lane": lane, "add": [{ "p": page, "n": "probe" }] });
    let outcome = client()
        .post(format!("{base}/_albedo/phosphor/routes"))
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("subscribe answers")
        .text()
        .await
        .expect("subscribe body");
    let outcome: serde_json::Value = serde_json::from_str(&outcome)
        .unwrap_or_else(|err| panic!("subscribe JSON ({err}): {outcome}"));
    let denied = outcome["denied"]
        .as_array()
        .map(|items| items.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, trunk.chunk()).await {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let mut frames = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(data.trim()) else { continue };
        if let Some(frame) = envelope["f"].as_str() {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(frame) {
                frames.extend(decoded);
            }
        }
    }
    (denied, frames)
}

/// Register `email` from a fresh tab and return its tab + session cookies.
async fn signed_in_as(base: &str, email: &str) -> String {
    let visitor = arrive(base).await;
    let registered = client()
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &visitor.cookie)
        .form(&[
            ("_csrf", visitor.csrf.as_str()),
            ("_albedo_return", visitor.return_path.as_str()),
            ("email", email),
            ("password", "correct horse battery staple"),
        ])
        .send()
        .await
        .expect("register answers");
    let session = cookie_named(&registered, "__Host-albedo_session=")
        .unwrap_or_else(|| panic!("registering {email} signs in ({})", registered.status()));
    format!("{}; {session}", visitor.cookie)
}

async fn payroll_page(base: &str, cookie: &str) -> (StatusCode, String) {
    let response = client()
        .get(format!("{base}/payroll"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("the page answers");
    (response.status(), response.text().await.expect("body"))
}

/// A page `src/middleware.ts` refuses is refused on both live lanes too.
///
/// 🔴 Before: the middleware ran only on app paths and the lanes checked only
/// `export const auth`, so a signed-in non-admin got 403 from `GET /payroll`
/// and every payroll row from `/_albedo/patches?p=/payroll` and from a
/// PHOSPHOR subscribe.
#[tokio::test(flavor = "multi_thread")]
async fn a_middleware_refusal_also_closes_the_live_lanes() {
    let (home, _work, app) = project();
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    fs::write(
        &config_path,
        config.replacen(
            "forge: {",
            &format!(
                "forge: {{\n    payroll: {{ fields: {{ line: \"text\" }}, seed: [{{ line: \"{PAYROLL}\" }}] }},\n"
            ),
            1,
        ),
    )
    .expect("config");
    write(&app, "src/middleware.ts", ROLE_MIDDLEWARE);
    write(&app, "src/routes/payroll.tsx", PAYROLL_PAGE);
    run_ok(albedo(home.path(), &app).args(["build", "."]), "albedo build");
    let (_server, base) = serve(home.path(), &app).await;

    let admin = signed_in_as(&base, ADMIN_EMAIL).await;
    let staff = signed_in_as(&base, "staff@example.com").await;

    // The page itself: refused to staff, served to the admin.
    let (status, _) = payroll_page(&base, &staff).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "control: the middleware refuses staff the page");
    let (status, html) = payroll_page(&base, &admin).await;
    assert_eq!(status, StatusCode::OK, "control: the admin gets the page");
    assert!(html.contains(PAYROLL), "control: the page renders the row");

    // The patches lane.
    assert!(
        carries(&lane_bytes_as(&base, "/payroll", &admin).await, PAYROLL),
        "control: the admin's patches lane carries payroll"
    );
    assert!(
        !carries(&lane_bytes_as(&base, "/payroll", &staff).await, PAYROLL),
        "staff's patches lane carried payroll the middleware refused them"
    );

    // The PHOSPHOR lane.
    let (denied, frames) = phosphor_as(&base, "/payroll", &admin).await;
    assert!(denied.is_empty(), "control: the admin's subscribe is granted ({denied:?})");
    assert!(carries(&frames, PAYROLL), "control: the admin's trunk is seeded with payroll");
    let (denied, frames) = phosphor_as(&base, "/payroll", &staff).await;
    assert_eq!(denied, vec!["/payroll".to_string()], "staff's subscribe to /payroll is denied");
    assert!(!carries(&frames, PAYROLL), "staff's trunk carried payroll the middleware refused them");

    // A public route stays live for staff: the gate refuses a page, it does
    // not switch the lanes off.
    let (denied, frames) = phosphor_as(&base, "/notices", &staff).await;
    assert!(denied.is_empty(), "control: staff may subscribe to /notices ({denied:?})");
    assert!(carries(&frames, NOTICE), "control: staff's /notices trunk is seeded");
}
