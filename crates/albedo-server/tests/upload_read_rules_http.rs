//! Who may read an upload back — through the real binary: `init` → declare
//! buckets → `serve` → sign up → upload through an action form → read.
//!
//! 🔴 Until 2026-10-30, `GET /_albedo/uploads/{id}` served any recorded file to
//! anyone who asked for its id. The id is the SHA-256 of the bytes, so it is not
//! a secret — anyone holding the file can compute it — and the route therefore
//! also told a prober whether a known file had ever been uploaded. Records were
//! unique per `(bytes, bucket)` under `INSERT OR IGNORE`, so a second person
//! uploading the same file got no record at all, and the read took whichever row
//! a `LIMIT 1` with no `ORDER BY` returned.
//!
//! Now a bucket declares `read: "owner" | "signed-in" | "public"` (owner when
//! omitted), each uploader keeps their own record, and a refused read is
//! indistinguishable from a file that was never stored.

use reqwest::redirect::Policy;
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{what} failed\n{text}");
    text
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

/// A visitor: the cookies a browser would hold.
#[derive(Clone)]
struct Visitor {
    cookie: String,
}

/// A fresh anonymous visitor — the tab cookie the first page mints.
async fn stranger(client: &reqwest::Client, base: &str) -> Visitor {
    let page = client.get(format!("{base}/files")).send().await.expect("GET /files");
    let tab = cookie_named(&page, "__Host-albedo-session").expect("the render mints a tab cookie");
    Visitor { cookie: tab }
}

/// Register an account through the scaffold's own form.
async fn sign_up(client: &reqwest::Client, base: &str, email: &str) -> Visitor {
    let form = client.get(format!("{base}/sign-in")).send().await.expect("GET /sign-in");
    let tab = cookie_named(&form, "__Host-albedo-session").expect("the render mints a tab cookie");
    let html = form.text().await.expect("body");
    let csrf = hidden_field(&html, "_csrf").expect("the form carries a CSRF token");
    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[
            ("_csrf", csrf.as_str()),
            ("email", email),
            // A throwaway credential for an account that lives in this temp dir.
            ("password", "upload read rules passphrase"),
        ])
        .send()
        .await
        .expect("register responds");
    let session = cookie_named(&registered, "__Host-albedo_session")
        .unwrap_or_else(|| panic!("registering {email} signs it in ({})", registered.status()));
    Visitor {
        cookie: format!("{tab}; {session}"),
    }
}

/// Submit `bytes` as `filename` to the form for `action`, the way a browser's
/// own multipart submit does. Returns the status.
async fn upload(
    client: &reqwest::Client,
    base: &str,
    who: &Visitor,
    action: &str,
    bucket: &str,
    filename: &str,
    bytes: &[u8],
) -> (u16, String) {
    let page = client
        .get(format!("{base}/files"))
        .header("cookie", &who.cookie)
        .send()
        .await
        .expect("GET /files")
        .text()
        .await
        .expect("body");
    let marker = format!(r#"data-albedo-action="{action}""#);
    let at = page.find(&marker).unwrap_or_else(|| panic!("no form for {action}\n{page}"));
    let form = &page[at..at + page[at..].find("</form>").expect("the form closes")];

    let boundary = "----albedo-upload-test-boundary";
    let mut body = Vec::new();
    for name in ["_csrf", "_albedo_return", "_albedo_intent"] {
        if let Some(value) = hidden_field(form, name) {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\ncontent-disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"{bucket}\"; \
             filename=\"{filename}\"\r\ncontent-type: application/json\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let response = client
        .post(format!("{base}/_albedo/action/{action}"))
        .header("cookie", &who.cookie)
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .header("sec-fetch-mode", "navigate")
        .body(body)
        .send()
        .await
        .expect("the upload responds");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

/// `(status, body, content-disposition, cache-control)` for a read as `who`.
async fn read(
    client: &reqwest::Client,
    base: &str,
    who: Option<&Visitor>,
    id: &str,
    if_none_match: Option<&str>,
) -> (u16, Vec<u8>, String, String) {
    let mut request = client.get(format!("{base}/_albedo/uploads/{id}"));
    if let Some(who) = who {
        request = request.header("cookie", &who.cookie);
    }
    if let Some(etag) = if_none_match {
        request = request.header("if-none-match", etag);
    }
    let response = request.send().await.expect("the read responds");
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let (disposition, cache) = (header("content-disposition"), header("cache-control"));
    let body = response.bytes().await.expect("body").to_vec();
    (status, body, disposition, cache)
}

fn id_of(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

const FILES_PAGE: &str = r#"import { action } from "albedo";

export const keep_private = action(() => null);
export const keep_public = action(() => null);

export default function Files() {
  return (
    <main>
      <form action="action:keep_private" method="POST" enctype="multipart/form-data">
        <input type="file" name="private_docs" />
        <button type="submit">keep</button>
      </form>
      <form action="action:keep_public" method="POST" enctype="multipart/form-data">
        <input type="file" name="public_pics" />
        <button type="submit">share</button>
      </form>
    </main>
  );
}
"#;

fn app_with_buckets(home: &Path, work: &Path) -> PathBuf {
    run_ok(albedo(home, work).args(["init", "app"]), "albedo init");
    let app = work.join("app");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).unwrap();
    let anchor = "  auth: {\n";
    assert!(config.contains(anchor), "the scaffold's auth block moved");
    fs::write(
        &config_path,
        config.replacen(
            anchor,
            "  uploads: {\n    \
             private_docs: { accept: [\"application/json\"] },\n    \
             public_pics: { accept: [\"application/json\"], read: \"public\" },\n  \
             },\n\n  auth: {\n",
            1,
        ),
    )
    .unwrap();
    fs::write(app.join("src/routes/files.tsx"), FILES_PAGE).unwrap();
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_is_read_back_only_by_whom_its_bucket_allows() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = app_with_buckets(home.path(), work.path());
    let (_server, base) = serve(home.path(), &app).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    let ada = sign_up(&client, &base, "ada@example.test").await;
    let bob = sign_up(&client, &base, "bob@example.test").await;
    let anon = stranger(&client, &base).await;

    // ── owner-only bucket (the default) ──
    let secret = br#"{"payslip": 4200}"#;
    let id = id_of(secret);
    let (status, body) = upload(&client, &base, &ada, "keep_private", "private_docs", "ada.json", secret).await;
    assert!(status == 303 || status == 200, "ada's upload: {status} {body}");

    let (status, bytes, disposition, _) = read(&client, &base, Some(&ada), &id, None).await;
    assert_eq!(status, 200, "the uploader reads her own file");
    assert_eq!(bytes, secret);
    assert!(disposition.contains("ada.json"), "{disposition}");

    // Refused reads look exactly like a file nobody ever stored.
    let never = id_of(b"never uploaded by anyone");
    let missing = read(&client, &base, Some(&bob), &never, None).await;
    assert_eq!(missing.0, 404);
    for (who, label) in [(Some(&bob), "another user"), (Some(&anon), "a stranger"), (None, "no cookies")] {
        let refused = read(&client, &base, who, &id, None).await;
        assert_eq!(
            (refused.0, &refused.1, &refused.3),
            (missing.0, &missing.1, &missing.3),
            "{label} must get exactly what a never-stored id gets"
        );
    }
    // Revalidation is not a way round it.
    let etag = format!("\"{id}\"");
    assert_eq!(read(&client, &base, Some(&bob), &id, Some(&etag)).await.0, 404);
    assert_eq!(read(&client, &base, Some(&ada), &id, Some(&etag)).await.0, 304);

    // The same bytes from a second person: their own record, their own name.
    let (status, body) = upload(&client, &base, &bob, "keep_private", "private_docs", "bob.json", secret).await;
    assert!(status == 303 || status == 200, "bob's upload: {status} {body}");
    let (status, _, disposition, _) = read(&client, &base, Some(&bob), &id, None).await;
    assert_eq!(status, 200, "bob reads the file he uploaded");
    assert!(disposition.contains("bob.json"), "bob gets his own record: {disposition}");
    let (_, _, disposition, _) = read(&client, &base, Some(&ada), &id, None).await;
    assert!(disposition.contains("ada.json"), "ada still gets hers: {disposition}");

    // Nobody can own an anonymous upload, so an owner-only bucket refuses one.
    let (status, body) = upload(&client, &base, &anon, "keep_private", "private_docs", "x.json", b"{}").await;
    assert_eq!(status, 401, "an anonymous upload into an owner-only bucket: {body}");
    assert!(body.contains("sign in to upload"), "{body}");

    // ── public bucket ──
    let picture = br#"{"cat": true}"#;
    let (status, body) = upload(&client, &base, &anon, "keep_public", "public_pics", "cat.json", picture).await;
    assert!(status == 303 || status == 200, "anonymous public upload: {status} {body}");
    let public_id = id_of(picture);
    assert_eq!(read(&client, &base, None, &public_id, None).await.0, 200);
    assert_eq!(read(&client, &base, Some(&bob), &public_id, None).await.0, 200);
}
