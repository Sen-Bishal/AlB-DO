//! EMAIL · 15.3 — mail, end to end, from `albedo init`.
//!
//! Starts where a stranger starts — the real binary, `init` → config → `build`
//! → `serve` — for the reason `jobs_http` gives: two of the last three features
//! passed hand-built fixtures while the real build classified their entry file
//! as a component. The `log` provider writes each message to
//! `.albedo/outbox/`, so every assertion about mail is made on **the message
//! that was produced**, not on a log line claiming one was.
//!
//! ## One sequential scenario per test
//!
//! Each test is one app and one server, driven through the whole flow in order,
//! because the flows are stateful by design (a token is issued, then spent;
//! a newer link voids an older one).
//!
//! ## Why every request carries its own `X-Forwarded-For`
//!
//! SHUTTER rations the credential class at 5 per caller, then 10 a minute —
//! the point of it. A single-address test making twenty credential requests
//! would be measuring the limiter. The server runs with `ALBEDO_TRUSTED_PROXIES`
//! set, which is the real production path for a deployment behind a balancer,
//! and each step arrives from its own forwarded address. The per-**mailbox**
//! budget is untouched by that, and one assertion below relies on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use reqwest::{redirect::Policy, StatusCode};

/// A page with a plain action that mails, and one that queues a job that mails.
const MAIL_ROUTE_TSX: &str = r#"
import { action } from "albedo";

export const send_it = action(({ form }) => {
  sendEmail(
    {
      to: form.to,
      subject: "Hello from an action",
      text: "sent by " + form.to,
      attachments: [{ filename: "note.txt", content: "aGVsbG8=", contentType: "text/plain" }],
    },
    { id: "action-mail-" + form.to },
  );
  enqueue("mail_job", { to: form.to });
});

export default function MailPage() {
  return (
    <main>
      <form action="action:send_it" method="post">
        <input name="to" defaultValue="dora@example.com" />
        <button type="submit">send</button>
      </form>
    </main>
  );
}
"#;

const JOBS_TS: &str = r#"
import { job, sendEmail } from "albedo/jobs";

export const mail_job = job({}, (args) => {
  sendEmail({ to: args.to, subject: "Hello from a job", html: "<p>queued by a job</p>" });
});
"#;

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

/// `albedo init`, then declare mail and the three auth flows that use it.
fn email_app(home: &Path, work: &Path, name: &str) -> PathBuf {
    run_ok(albedo(home, work).args(["init", name]), "albedo init");
    let app = work.join(name);
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("scaffold config");
    let config = config.replacen(
        "      password: {},",
        "      password: { reset: \"email\", verify: \"required\" },\n      magiclink: {},",
        1,
    );
    let end = config.rfind("};").expect("the config ends with `};`");
    let config = format!(
        "{}  email: {{ from: \"Mail Test <no-reply@example.com>\", provider: \"log\", \
         templates: {{ magicLink: {{ subject: \"Sign in to {{{{app}}}} now\" }} }} }},\n{}",
        &config[..end],
        &config[end..]
    );
    assert!(config.contains("verify: \"required\""), "the provider edit applied");
    fs::write(&config_path, config).expect("write config");
    app
}

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("addr").port()
    };
    let stderr = app.join("serve.stderr.log");
    let child = albedo(home, app)
        .args(["serve", ".", "--port", &port.to_string()])
        .env("ALBEDO_TRUSTED_PROXIES", "127.0.0.1/32")
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
        assert!(
            Instant::now() < deadline,
            "albedo serve never answered on {base}\nstderr: {}",
            fs::read_to_string(&stderr).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base)
}

/// A distinct forwarded address per request — see the module docs.
fn next_ip() -> String {
    static N: AtomicU32 = AtomicU32::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("10.9.{}.{}", n / 250, n % 250 + 1)
}

/// What a browser holds after loading a page: cookies and hidden fields.
struct Tab {
    cookie: String,
    csrf: String,
}

async fn open_tab(client: &reqwest::Client, url: &str) -> (Tab, reqwest::StatusCode, String) {
    let response = client
        .get(url)
        .header("x-forwarded-for", next_ip())
        .send()
        .await
        .expect("page responds");
    let status = response.status();
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let html = response.text().await.expect("body");
    let csrf = hidden(&html, "_csrf").unwrap_or_default();
    (Tab { cookie, csrf }, status, html)
}

fn hidden(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    tab: &Tab,
    extra_cookie: Option<&str>,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    let mut body = vec![("_csrf".to_string(), tab.csrf.clone())];
    body.extend(fields.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())));
    let cookie = match extra_cookie {
        Some(extra) => format!("{}; {extra}", tab.cookie),
        None => tab.cookie.clone(),
    };
    client
        .post(format!("{base}{path}"))
        .header("cookie", cookie)
        .header("x-forwarded-for", next_ip())
        .form(&body)
        .send()
        .await
        .expect("endpoint responds")
}

fn session_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-albedo_session=") && !v.contains("Max-Age=0"))
        .map(|v| v.split(';').next().unwrap_or(v).to_string())
}

/// One message from the outbox: recipient, subject, decoded text.
#[derive(Debug, Clone)]
struct Mail {
    written: std::time::SystemTime,
    to: String,
    subject: String,
    text: String,
    raw: String,
}

fn read_outbox(app: &Path) -> Vec<Mail> {
    let Ok(entries) = fs::read_dir(app.join(".albedo").join("outbox")) else {
        return Vec::new();
    };
    let mut mails: Vec<Mail> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let raw = fs::read_to_string(entry.path()).ok()?;
            let header = |name: &str| -> String {
                raw.lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                    .unwrap_or_default()
                    .to_string()
            };
            Some(Mail {
                written: entry.metadata().ok()?.modified().ok()?,
                to: header("To"),
                subject: header("Subject"),
                text: decoded_text(&raw),
                raw,
            })
        })
        .collect();
    // Written order. File names are queue row ids, which are random — and a
    // retried row overwrites its own file, which is why they are not stamped.
    mails.sort_by_key(|m| m.written);
    mails
}

/// The base64 `text/plain` part, decoded — or the html part when there is none.
fn decoded_text(raw: &str) -> String {
    for kind in ["text/plain", "text/html"] {
        let marker = format!("Content-Type: {kind}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
        if let Some(start) = raw.find(&marker) {
            let body: String = raw[start + marker.len()..]
                .lines()
                .take_while(|l| !l.is_empty() && !l.starts_with("--"))
                .collect();
            return String::from_utf8(STANDARD.decode(body).unwrap_or_default()).unwrap_or_default();
        }
    }
    String::new()
}

/// Wait for the `nth` (0-based) message to `to` whose subject contains `subject`.
async fn wait_for_mail(app: &Path, to: &str, subject: &str, nth: usize) -> Mail {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let matching: Vec<Mail> = read_outbox(app)
            .into_iter()
            .filter(|m| m.to.contains(to) && m.subject.contains(subject))
            .collect();
        if let Some(mail) = matching.get(nth) {
            return mail.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no message #{nth} to {to} with subject `{subject}` arrived. Outbox:\n{:#?}\nserver stderr:\n{}",
            read_outbox(app),
            fs::read_to_string(app.join("serve.stderr.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// The token a mailed link carries.
fn token_in(mail: &Mail) -> String {
    let start = mail.text.find("token=").expect("the mail carries a link") + "token=".len();
    mail.text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client")
}

/// 🔑 The three AUTH flows that needed a mail channel, over real HTTP.
#[tokio::test]
async fn reset_verify_and_magic_link_work_end_to_end() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let app = email_app(home.path(), work.path(), "mailauth");
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");
    let (_server, base) = serve(home.path(), &app).await;
    let client = client();
    let (tab, status, _) = open_tab(&client, &format!("{base}/sign-in")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!tab.csrf.is_empty(), "the sign-in page arms its forms");

    // ── verify: sign-up opens NO session, and mails a link ────────────────
    let reg = post(&client, &base, "/_albedo/auth/password/register", &tab, None, &[
        ("email", "ada@example.com"), ("password", "a-long-passphrase"), ("next", "/guestbook"),
    ]).await;
    assert_eq!(reg.status(), StatusCode::SEE_OTHER);
    assert!(session_cookie(&reg).is_none(), "an unconfirmed sign-up must not be signed in");
    let first = wait_for_mail(&app, "ada@example.com", "Confirm your email", 0).await;

    // ── the same address again answers IDENTICALLY, and the owner is told ──
    let again = post(&client, &base, "/_albedo/auth/password/register", &tab, None, &[
        ("email", "ada@example.com"), ("password", "someone-elses-guess"),
    ]).await;
    assert_eq!(again.status(), reg.status(), "a taken address must not be distinguishable");
    assert_eq!(
        again.headers().get("location"),
        reg.headers().get("location"),
        "not even by where it redirects"
    );
    assert!(session_cookie(&again).is_none());
    let notice = wait_for_mail(&app, "ada@example.com", "Sign-up attempt", 0).await;
    assert!(!notice.text.contains("token="), "the notice grants nothing: {}", notice.text);

    // ── the right password is refused until the address is proven ─────────
    let login = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "ada@example.com"), ("password", "a-long-passphrase"),
        ("_albedo_return", "/guestbook"),
    ]).await;
    assert_eq!(login.status(), StatusCode::FORBIDDEN);
    assert!(session_cookie(&login).is_none());
    assert!(login.text().await.unwrap().contains("Confirm your email"));
    // …and a fresh link is sent, which VOIDS the first one.
    let second = wait_for_mail(&app, "ada@example.com", "Confirm your email", 1).await;
    let (stale, fresh) = (token_in(&first), token_in(&second));
    assert_ne!(stale, fresh);

    // The mailbox budget: a burst of three, all spent on ada. A fourth link
    // is not sent — and the response does not say so.
    let throttled = post(&client, &base, "/_albedo/auth/password/forgot", &tab, None, &[
        ("email", "ada@example.com"),
    ]).await;
    assert_eq!(throttled.status(), StatusCode::SEE_OTHER);

    // ── the landing page: GET renders, never spends ────────────────────────
    let (_, status, _) = open_tab(&client, &format!("{base}/_albedo/auth/email/verify?token={stale}")).await;
    assert_eq!(status, StatusCode::GONE, "a superseded link is dead");
    let verify_url = format!("{base}/_albedo/auth/email/verify?token={fresh}");
    for _ in 0..2 {
        // Twice: a mail scanner's prefetch must leave the link working.
        let (_, status, html) = open_tab(&client, &verify_url).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains(&format!("value=\"{fresh}\"")), "the form carries the token");
    }
    let (page, _, _) = open_tab(&client, &verify_url).await;
    let verified = post(&client, &base, "/_albedo/auth/email/verify", &page, None, &[("token", &fresh)]).await;
    assert_eq!(verified.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        verified.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/guestbook"),
        "the re-sent link lands where the login that asked for it would have"
    );
    assert!(session_cookie(&verified).is_some(), "confirming signs you in");
    let replay = post(&client, &base, "/_albedo/auth/email/verify", &page, None, &[("token", &fresh)]).await;
    assert_eq!(replay.status(), StatusCode::GONE, "a link works once");

    let login = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "ada@example.com"), ("password", "a-long-passphrase"),
    ]).await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert!(session_cookie(&login).is_some(), "a confirmed account signs in");

    // ── reset, on a fresh mailbox ──────────────────────────────────────────
    let reg = post(&client, &base, "/_albedo/auth/password/register", &tab, None, &[
        ("email", "bob@example.com"), ("password", "bobs-first-password"),
    ]).await;
    assert_eq!(reg.status(), StatusCode::SEE_OTHER);
    let confirm = token_in(&wait_for_mail(&app, "bob@example.com", "Confirm your email", 0).await);
    let bob_session = session_cookie(
        &post(&client, &base, "/_albedo/auth/email/verify", &tab, None, &[("token", &confirm)]).await,
    )
    .expect("bob is signed in");

    let forgot = post(&client, &base, "/_albedo/auth/password/forgot", &tab, None, &[("email", "bob@example.com")]).await;
    assert_eq!(forgot.status(), StatusCode::SEE_OTHER);
    // An address with no account answers the same, and nothing is mailed.
    let nobody = post(&client, &base, "/_albedo/auth/password/forgot", &tab, None, &[("email", "nobody@example.com")]).await;
    assert_eq!(nobody.status(), forgot.status());
    assert_eq!(nobody.headers().get("location"), forgot.headers().get("location"));

    let reset_token = token_in(&wait_for_mail(&app, "bob@example.com", "Reset your", 0).await);
    let reset_url = format!("{base}/_albedo/auth/password/reset?token={reset_token}");
    let (page, status, html) = open_tab(&client, &reset_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("type=\"password\""), "{html}");

    // Too short: refused, and the link survives.
    let short = post(&client, &base, "/_albedo/auth/password/reset", &page, None, &[
        ("token", &reset_token), ("password", "short"),
    ]).await;
    assert_eq!(short.status(), StatusCode::BAD_REQUEST);
    assert_eq!(open_tab(&client, &reset_url).await.1, StatusCode::OK, "a policy refusal must not spend the link");

    let done = post(&client, &base, "/_albedo/auth/password/reset", &page, Some(&bob_session), &[
        ("token", &reset_token), ("password", "bobs-new-password"),
    ]).await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    let after_reset = session_cookie(&done).expect("a reset signs you in");
    assert_ne!(after_reset, bob_session, "on a NEW session");
    assert_eq!(open_tab(&client, &reset_url).await.1, StatusCode::GONE, "and the link is spent");

    let old = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "bob@example.com"), ("password", "bobs-first-password"),
    ]).await;
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED, "the old password is gone");
    let new = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "bob@example.com"), ("password", "bobs-new-password"),
    ]).await;
    assert_eq!(new.status(), StatusCode::SEE_OTHER);

    // ── magic link: sign-in AND sign-up, one call ──────────────────────────
    let start = post(&client, &base, "/_albedo/auth/magiclink/start", &tab, None, &[
        ("email", "carol@example.com"), ("next", "/guestbook"),
    ]).await;
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    // The subject is the app's override; the body is still the built-in.
    let magic = wait_for_mail(&app, "carol@example.com", "Sign in to Mail Test now", 0).await;
    assert!(
        magic.text.contains("http://localhost:"),
        "the link is built from configuration, not from a request header: {}",
        magic.text
    );
    let magic_token = token_in(&magic);
    let magic_url = format!("{base}/_albedo/auth/magiclink/signin?token={magic_token}");
    let (page, status, _) = open_tab(&client, &magic_url).await;
    assert_eq!(status, StatusCode::OK);
    let signed_in = post(&client, &base, "/_albedo/auth/magiclink/signin", &page, None, &[("token", &magic_token)]).await;
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    assert_eq!(signed_in.headers().get("location").and_then(|v| v.to_str().ok()), Some("/guestbook"));
    assert!(session_cookie(&signed_in).is_some());

    // ── a magic-link user sets a FIRST password through "forgot password" ──
    let forgot = post(&client, &base, "/_albedo/auth/password/forgot", &tab, None, &[("email", "carol@example.com")]).await;
    assert_eq!(forgot.status(), StatusCode::SEE_OTHER);
    let first_pw = token_in(&wait_for_mail(&app, "carol@example.com", "Reset your", 0).await);
    let (page, _, _) = open_tab(&client, &format!("{base}/_albedo/auth/password/reset?token={first_pw}")).await;
    let set = post(&client, &base, "/_albedo/auth/password/reset", &page, None, &[
        ("token", &first_pw), ("password", "carols-password"),
    ]).await;
    assert_eq!(set.status(), StatusCode::SEE_OTHER);
    let login = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "carol@example.com"), ("password", "carols-password"),
    ]).await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER, "one person, two ways in");
    // …and a password SIGN-UP for her proven address is "taken", quietly.
    let squat = post(&client, &base, "/_albedo/auth/password/register", &tab, None, &[
        ("email", "carol@example.com"), ("password", "a-strangers-guess"),
    ]).await;
    assert_eq!(squat.status(), StatusCode::SEE_OTHER);
    wait_for_mail(&app, "carol@example.com", "Sign-up attempt", 0).await;

    // ── the pre-account-takeover defence, observed ─────────────────────────
    // A stranger registers erin's address with a password they know and never
    // confirms it. Erin signs in by link; the stranger's password must die.
    let planted = post(&client, &base, "/_albedo/auth/password/register", &tab, None, &[
        ("email", "erin@example.com"), ("password", "the-strangers-password"),
    ]).await;
    assert_eq!(planted.status(), StatusCode::SEE_OTHER);
    wait_for_mail(&app, "erin@example.com", "Confirm your email", 0).await;
    post(&client, &base, "/_albedo/auth/magiclink/start", &tab, None, &[("email", "erin@example.com")]).await;
    let erin = token_in(&wait_for_mail(&app, "erin@example.com", "Sign in to", 0).await);
    let (page, _, _) = open_tab(&client, &format!("{base}/_albedo/auth/magiclink/signin?token={erin}")).await;
    let claimed = post(&client, &base, "/_albedo/auth/magiclink/signin", &page, None, &[("token", &erin)]).await;
    assert!(session_cookie(&claimed).is_some(), "the mailbox owner is signed in");
    let stranger = post(&client, &base, "/_albedo/auth/password/login", &tab, None, &[
        ("email", "erin@example.com"), ("password", "the-strangers-password"),
    ]).await;
    assert_eq!(
        stranger.status(),
        StatusCode::UNAUTHORIZED,
        "an unproven password must not survive its address being claimed"
    );

    // Mail for addresses that asked for nothing never went out.
    assert!(
        !read_outbox(&app).iter().any(|m| m.to.contains("nobody@")),
        "an address with no account must receive nothing"
    );
    assert_eq!(
        read_outbox(&app).iter().filter(|m| m.to.contains("ada@")).count(),
        3,
        "ada's fourth link was over her mailbox's budget"
    );
    // 🔒 A POST without the page's CSRF token spends nothing.
    let forged = client
        .post(format!("{base}/_albedo/auth/password/forgot"))
        .header("x-forwarded-for", next_ip())
        .form(&[("email", "bob@example.com")])
        .send()
        .await
        .expect("responds");
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);
}

/// `sendEmail()` from an action and from a job; the queue row is scrubbed once
/// the message is out.
#[tokio::test]
async fn send_email_from_an_action_and_a_job_lands_once_and_is_scrubbed() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let app = email_app(home.path(), work.path(), "mailapp");
    fs::write(app.join("src/routes/mail.tsx"), MAIL_ROUTE_TSX).unwrap();
    fs::write(app.join("src/jobs.ts"), JOBS_TS).unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    {
        let (_server, base) = serve(home.path(), &app).await;
        let client = client();
        let (tab, status, html) = open_tab(&client, &format!("{base}/mail")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let action = {
            let open = html.find("<form").expect("a form");
            let tag = &html[open..open + html[open..].find('>').expect("tag end")];
            let start = tag.find("action=\"").expect("action attr") + "action=\"".len();
            tag[start..start + tag[start..].find('"').unwrap()].to_string()
        };
        // Twice, with the same idempotency id: ONE action email.
        for _ in 0..2 {
            let posted = post(&client, &base, &action, &tab, None, &[("to", "dora@example.com")]).await;
            assert!(
                posted.status().is_success() || posted.status().is_redirection(),
                "the action accepts the submit: {}",
                posted.status()
            );
        }
        let from_action = wait_for_mail(&app, "dora@example.com", "Hello from an action", 0).await;
        assert!(from_action.text.contains("sent by dora@example.com"), "{from_action:?}");
        assert!(
            from_action.raw.contains("Content-Disposition: attachment; filename=\"note.txt\"\r\n")
                && from_action.raw.contains("aGVsbG8="),
            "the inline attachment travels with the message:\n{}",
            from_action.raw
        );
        let from_job = wait_for_mail(&app, "dora@example.com", "Hello from a job", 0).await;
        assert!(from_job.text.contains("queued by a job"), "{from_job:?}");
        // The job ran twice (no id), so two job emails; the action email is once.
        wait_for_mail(&app, "dora@example.com", "Hello from a job", 1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            read_outbox(&app).iter().filter(|m| m.subject.contains("from an action")).count(),
            1,
            "a repeated `sendEmail` with the same id is one email"
        );

        // A header-injection attempt throws in the action and queues nothing.
        let _ = post(&client, &base, &action, &tab, None, &[("to", "eve@example.com\r\nBcc: all@example.com")]).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !read_outbox(&app).iter().any(|m| m.to.contains("eve@")),
            "an address with a line break must never become a message"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Every sent row's body is gone from the queue.
    use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate, SqlValue};
    let db = LibSqlSubstrate::open_local(&app.join("forge.db")).await.expect("db opens");
    let rows = db
        .query(
            "SELECT state, args FROM albedo_jobs WHERE name = 'albedo:send_email'",
            &[],
        )
        .await
        .expect("reads");
    assert_eq!(rows.rows.len(), 3, "one action email + two job emails");
    for row in &rows.rows {
        let text = |i: usize| row.get(i).and_then(SqlValue::as_str).unwrap_or_default().to_string();
        assert_eq!(text(0), "done");
        assert_eq!(text(1), "{}", "a delivered message's body must be scrubbed");
    }
}

/// `magiclink` beside a password provider that lets unconfirmed accounts sign in
/// fails the BUILD — the configuration in which proving an address could hand
/// the prover a stranger's account.
#[test]
fn magic_link_beside_an_unconfirmed_password_provider_fails_the_build() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    run_ok(albedo(home.path(), work.path()).args(["init", "squat"]), "albedo init");
    let app = work.path().join("squat");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).unwrap().replacen(
        "      password: {},",
        "      password: { reset: \"email\" },\n      magiclink: {},",
        1,
    );
    let end = config.rfind("};").unwrap();
    let config = format!(
        "{}  email: {{ from: \"T <t@example.com>\", provider: \"log\" }},\n{}",
        &config[..end],
        &config[end..]
    );
    fs::write(&config_path, config).unwrap();

    let build = albedo(home.path(), &app).arg("build").output().expect("runs");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(!build.status.success(), "must fail:\n{output}");
    assert!(output.contains("verify: \\\"required\\\"") || output.contains("verify: \"required\""), "names the fix:\n{output}");
}

/// `reset: "email"` with nowhere to send it fails the BUILD, naming the fix.
#[test]
fn an_auth_flow_that_mails_without_an_email_block_fails_the_build() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    run_ok(albedo(home.path(), work.path()).args(["init", "nomail"]), "albedo init");
    let app = work.path().join("nomail");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replacen("      password: {},", "      password: { reset: \"email\" },", 1);
    fs::write(&config_path, config).unwrap();

    let build = albedo(home.path(), &app).arg("build").output().expect("runs");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(!build.status.success(), "must fail:\n{output}");
    assert!(output.contains("email"), "{output}");
    assert!(output.contains("password"), "names the provider:\n{output}");
}
