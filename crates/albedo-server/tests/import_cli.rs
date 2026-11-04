//! `albedo import` — rows from another system stay changeable by their owners.
//!
//! A row is changed only by the signed-in user who added it unless its
//! collection says `edit: "anyone"`, and "who added it" is recorded by the
//! append. Rows a migration script inserts directly have no author, so no
//! request can ever update or remove them — every row an app brings with it.
//! This drives the whole path through the real binary: users sign up, the
//! server stops, `albedo import` writes rows owned by them, the server starts
//! again, and each user tries to change the rows through an action.

use reqwest::redirect::Policy;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("addr").port()
    };
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

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

async fn sign_up(client: &reqwest::Client, base: &str, email: &str) {
    let form = client.get(format!("{base}/sign-in")).send().await.expect("GET /sign-in");
    let tab = cookie_named(&form, "__Host-albedo-session").expect("a tab cookie");
    let html = form.text().await.expect("body");
    let csrf = hidden_field(&html, "_csrf").expect("a CSRF token");
    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[("_csrf", csrf.as_str()), ("email", email), ("password", PASSWORD)])
        .send()
        .await
        .expect("register responds");
    assert!(
        cookie_named(&registered, "__Host-albedo_session").is_some(),
        "registering {email} signs it in ({})",
        registered.status()
    );
}

async fn sign_in(client: &reqwest::Client, base: &str, email: &str) -> String {
    let form = client.get(format!("{base}/sign-in")).send().await.expect("GET /sign-in");
    let tab = cookie_named(&form, "__Host-albedo-session").expect("a tab cookie");
    let html = form.text().await.expect("body");
    let csrf = hidden_field(&html, "_csrf").expect("a CSRF token");
    let signed_in = client
        .post(format!("{base}/_albedo/auth/password/login"))
        .header("cookie", &tab)
        .form(&[("_csrf", csrf.as_str()), ("email", email), ("password", PASSWORD)])
        .send()
        .await
        .expect("login responds");
    let session = cookie_named(&signed_in, "__Host-albedo_session")
        .unwrap_or_else(|| panic!("signing {email} in ({})", signed_in.status()));
    format!("{tab}; {session}")
}

/// Ask to rename row `id` as `cookie`, through the page's own form.
async fn rename(client: &reqwest::Client, base: &str, cookie: &str, id: u32, message: &str) {
    let page = client
        .get(format!("{base}/edit"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("GET /edit")
        .text()
        .await
        .expect("body");
    let csrf = hidden_field(&page, "_csrf").expect("the form carries a CSRF token");
    let mut form = vec![("_csrf", csrf), ("id", id.to_string()), ("message", message.to_string())];
    for name in ["_albedo_return", "_albedo_intent"] {
        if let Some(value) = hidden_field(&page, name) {
            form.push((name, value));
        }
    }
    let _ = client
        .post(format!("{base}/_albedo/action/rename"))
        .header("cookie", cookie)
        .header("sec-fetch-mode", "navigate")
        .form(&form)
        .send()
        .await
        .expect("the rename responds");
}

async fn messages(client: &reqwest::Client, base: &str) -> String {
    client
        .get(format!("{base}/edit"))
        .send()
        .await
        .expect("GET /edit")
        .text()
        .await
        .expect("body")
}

const PASSWORD: &str = "import cli test passphrase";

const EDIT_PAGE: &str = r#"import { action, useSharedSlot } from "albedo";

interface Entry { id: number; author: string; message: string }

export const rename = action(({ form }) =>
  update("guestbook", Number(form.id), { message: form.message }),
);

export default function Edit() {
  const entries = useSharedSlot<Entry[]>("guestbook");
  return (
    <main>
      <ul>
        {entries.map((entry) => (
          <li key={entry.id}>{entry.id}:{entry.message}</li>
        ))}
      </ul>
      <form action="action:rename" method="POST">
        <input name="id" />
        <input name="message" />
        <button type="submit">rename</button>
      </form>
    </main>
  );
}
"#;

fn app(home: &Path, work: &Path) -> PathBuf {
    let (ok, text) = run(albedo(home, work).args(["init", "app"]), "albedo init");
    assert!(ok, "albedo init failed\n{text}");
    let app = work.join("app");
    fs::write(app.join("src/routes/edit.tsx"), EDIT_PAGE).unwrap();
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_rows_belong_to_the_users_they_name() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = app(home.path(), work.path());
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // The users exist first — rows are imported for people who already have
    // accounts.
    {
        let (_server, base) = serve(home.path(), &app).await;
        sign_up(&client, &base, "ada@example.test").await;
        sign_up(&client, &base, "bob@example.test").await;
    }

    // An author nobody signed up as: refused whole, nothing written.
    let unknown = app.join("unknown.jsonl");
    fs::write(
        &unknown,
        "{\"id\": 90, \"author\": \"x\", \"message\": \"should not land\", \"owner\": \"ada@example.test\"}\n\
         {\"id\": 91, \"author\": \"x\", \"message\": \"should not land\", \"owner\": \"eve@example.test\"}\n",
    )
    .unwrap();
    let (ok, text) = run(
        albedo(home.path(), &app).args(["import", "guestbook", "unknown.jsonl", "--author-email", "owner"]),
        "albedo import (unknown author)",
    );
    assert!(!ok, "an import naming a user who does not exist succeeded\n{text}");
    assert!(text.contains("no user eve@example.test"), "{text}");

    // The real import: two of Ada's rows, one of Bob's, one nobody's.
    let rows = app.join("rows.jsonl");
    fs::write(
        &rows,
        "{\"id\": 11, \"author\": \"Ada\", \"message\": \"ada one\", \"owner\": \"ADA@example.test\"}\n\
         {\"id\": 12, \"author\": \"Ada\", \"message\": \"ada two\", \"owner\": \"ada@example.test\"}\n\
         {\"id\": 21, \"author\": \"Bob\", \"message\": \"bob one\", \"owner\": \"bob@example.test\"}\n\
         {\"id\": 31, \"author\": \"?\", \"message\": \"nobody's\"}\n",
    )
    .unwrap();
    let (ok, text) = run(
        albedo(home.path(), &app).args(["import", "guestbook", "rows.jsonl", "--author-email", "owner"]),
        "albedo import",
    );
    assert!(ok, "albedo import failed\n{text}");
    assert!(text.contains("imported 4 row(s)"), "{text}");
    assert!(text.contains("1 row(s) have no author"), "{text}");

    let (_server, base) = serve(home.path(), &app).await;
    let page = messages(&client, &base).await;
    for line in ["11:ada one", "12:ada two", "21:bob one", "31:nobody's"] {
        assert!(page.contains(line), "the imported row {line:?} is not on the page\n{page}");
    }
    assert!(!page.contains("should not land"), "the refused import wrote rows\n{page}");

    let ada = sign_in(&client, &base, "ada@example.test").await;
    let bob = sign_in(&client, &base, "bob@example.test").await;

    rename(&client, &base, &ada, 11, "ada renamed her own").await;
    rename(&client, &base, &bob, 12, "bob renamed ada's").await;
    rename(&client, &base, &ada, 31, "ada renamed nobody's").await;

    let page = messages(&client, &base).await;
    assert!(page.contains("11:ada renamed her own"), "ada could not change her imported row\n{page}");
    assert!(page.contains("12:ada two"), "bob changed ada's imported row\n{page}");
    assert!(page.contains("31:nobody's"), "an authorless imported row was changed\n{page}");
}
