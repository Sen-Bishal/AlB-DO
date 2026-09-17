//! JOBS · 15.5 — `src/jobs.ts` actually running, from `albedo init`.
//!
//! ## Why this starts from the CLI
//!
//! For the reason `middleware_http` does, and the precedent is the same feature:
//! the first real build of a middleware file classified it as a **Tier-B
//! component**, because the scanner saw a default-exported function and did what
//! it does with those. A hand-written manifest fixture would have passed while
//! the real thing was broken.
//!
//! A jobs file is the same shape of input — a module in `src/` that is not a
//! route — so the only honest test is the real binary: `init` → write the file →
//! `build` → `serve`, and then watch the database for evidence that the body
//! ran.
//!
//! ## What "it ran" is proved by
//!
//! Not a log line and not a metric — **a row the body wrote**. A scheduled job
//! appends to the scaffold's own `guestbook` collection, and the assertion reads
//! it back over HTTP from the rendered page. That is end to end: schedule →
//! queue row → engine → FORGE write → render.
//!
//! ## Every assertion is paired with a control
//!
//! A runner that ran everything constantly would satisfy "did it run?". So the
//! `every 1s` job is checked beside a `@yearly` one that must **not** have run in
//! the same window, and the retrying job is checked for the *number* of attempts
//! rather than for having failed at all.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Two scheduled jobs and one that only runs when enqueued.
///
/// `tick` fires every second — fast enough for a test, and a real declaration
/// rather than a special test-only path.
/// `yearly` is the control: same file, same runner, must not fire.
const JOBS_TS: &str = r#"
import { job, append, enqueue } from "albedo/jobs";
import { STAMP } from "./lib/stamp";

export const tick = job({ schedule: "every 1s" }, (args, ctx) => {
  // Reads the imported module and the context, so the assertion below covers
  // the module graph and the principal, not merely "a function was called".
  if (STAMP !== "tick") throw new Error("the module graph did not load");
  if (ctx.user !== null) throw new Error("a plain scheduled run must be anonymous");
  append("guestbook", { author: STAMP, message: "fired" });
  // Idempotent by id: however many times `tick` fires, this is ONE row.
  enqueue("followup", { from: "tick" }, { id: "followup-once" });
  return "ok";
});

export const followup = job({}, (args) => {
  append("guestbook", { author: "followup", message: "from " + args.from });
});

export const yearly = job({ schedule: "@yearly" }, () => "must not run");

export const on_demand = job({ retries: 2 }, () => "never scheduled");

export const from_action = job({ retries: 1 }, (args) => {
  append("guestbook", { author: "from_action", message: "for " + args.who });
});
"#;

/// A form action that queues work instead of doing it — the transactional-email
/// shape, without an email provider in the test.
const QUEUE_ROUTE_TSX: &str = r#"
import { action } from "albedo";

export const queue_it = action(({ form }) => {
  enqueue("from_action", { who: form.who }, { id: "action-once" });
});

export default function QueuePage() {
  return (
    <main>
      <form action="action:queue_it" method="post">
        <input name="who" defaultValue="alice" />
        <button type="submit">go</button>
      </form>
    </main>
  );
}
"#;

const STAMP_TS: &str = r#"export const STAMP = "tick";"#;

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
    let port = free_port();
    let stderr = app.join("serve.stderr.log");
    let child = albedo(home, app)
        .args(["serve", ".", "--port", &port.to_string()])
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

/// The guestbook page's HTML, which is where a job's FORGE write shows up.
async fn guestbook(base: &str) -> String {
    reqwest::get(format!("{base}/guestbook"))
        .await
        .expect("guestbook renders")
        .text()
        .await
        .expect("body")
}

/// Poll the rendered page until `needle` appears, or fail with what it did say.
///
/// 🪤 **Two seconds, not 200 ms.** A tighter poll is itself a burst of reads and
/// SHUTTER rate-limits it — the page then answers
/// `{"error":"rate_limited"}` and the assertion fails having proved nothing
/// about the job. The first version of this test did exactly that.
async fn wait_for(base: &str, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        let last = guestbook(base).await;
        if last.contains(needle) {
            return last;
        }
        assert!(
            Instant::now() < deadline,
            "`{needle}` never appeared on /guestbook within {within:?}.\nLast render:\n{}",
            &last[..last.len().min(4000)]
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A scheduled job's body executes, its durable write lands, and the job it
/// enqueued runs too — proved on the rendered page and in the queue's own record.
#[tokio::test]
async fn a_scheduled_job_writes_to_forge_and_the_job_it_enqueued_runs() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(
        albedo(home.path(), work.path()).args(["init", "jobapp"]),
        "albedo init",
    );
    let app = work.path().join("jobapp");
    fs::create_dir_all(app.join("src/lib")).unwrap();
    // A relative import, so the module graph is exercised rather than assumed:
    // a jobs file that reaches another project module must load both.
    fs::write(app.join("src/lib/stamp.ts"), STAMP_TS).unwrap();
    fs::write(app.join("src/jobs.ts"), JOBS_TS).unwrap();

    let build = albedo(home.path(), &app)
        .arg("build")
        .output()
        .expect("albedo build runs");
    assert!(
        build.status.success(),
        "a valid jobs file must build\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );
    let report = String::from_utf8_lossy(&build.stdout).to_string();
    // 🪤 The exact defect middleware hit: a non-route module in `src/` being
    // classified as a component. Asserted on the build's own report.
    assert!(
        !report.contains("jobs  ") && !report.contains("B jobs"),
        "jobs.ts must not be tiered as a component:\n{report}"
    );

    let html = {
        let (_server, base) = serve(home.path(), &app).await;
        // The proof a write landed, read back off the page that renders the
        // collection: schedule → queue row → engine → FORGE write → render.
        let html = wait_for(&base, "fired", Duration::from_secs(45)).await;
        // And the job the body enqueued, which only runs if `enqueue()` wrote a
        // row that the runner then claimed.
        wait_for(&base, "from tick", Duration::from_secs(30)).await;
        html
    };
    tokio::time::sleep(Duration::from_millis(500)).await;

    let rows = job_rows(&app.join("forge.db")).await;
    let _ = &html;

    // The idempotent enqueue: `tick` fires once a second and enqueues under a
    // fixed id every time, so there must be exactly ONE followup row however
    // many times it fired.
    let followups = rows.iter().filter(|(name, _, _)| name == "followup").count();
    assert_eq!(
        followups, 1,
        "an enqueue with a fixed id must land once however often it is called:\n{}",
        describe(&rows)
    );

    // The proof: the body ran and returned. A body that threw — a missing
    // module in the graph, a principal that was not anonymous — would be
    // `failed` with its own message here instead.
    let done: Vec<&(String, String, String)> = rows
        .iter()
        .filter(|(name, state, _)| name == "tick" && state == "done")
        .collect();
    assert!(
        !done.is_empty(),
        "the `every 1s` job never completed. Rows:\n{}",
        describe(&rows)
    );
    // Roughly one per second, so several in a five-second window — and this is
    // also the assertion that would catch a runner firing a tick repeatedly.
    assert!(
        done.len() >= 2 && done.len() <= 12,
        "expected a handful of one-per-second fires in ~5s, got {}:\n{}",
        done.len(),
        describe(&rows)
    );

    // CONTROL — same file, same runner, a schedule that cannot have come due.
    // Without this, a runner that ignored schedules and ran everything would
    // satisfy the assertion above.
    assert!(
        !rows.iter().any(|(name, _, _)| name == "yearly"),
        "the @yearly job was queued in a test that ran for seconds:\n{}",
        describe(&rows)
    );

    // CONTROL — a job with no schedule must never be queued on its own.
    assert!(
        !rows.iter().any(|(name, _, _)| name == "on_demand"),
        "an enqueue-only job ran without being enqueued:\n{}",
        describe(&rows)
    );
}

/// Every job row as `(name, state, last_error)`.
async fn job_rows(db_path: &Path) -> Vec<(String, String, String)> {
    use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate, SqlValue};
    use dom_render_compiler::jobs::queue::JOBS;

    let db = LibSqlSubstrate::open_local(db_path)
        .await
        .expect("the app's database opens");
    let rows = db
        .query(
            &format!("SELECT name, state, COALESCE(last_error, '') FROM {JOBS}"),
            &[],
        )
        .await
        .expect("the jobs table reads back");
    rows.rows
        .iter()
        .map(|row| {
            let text = |i: usize| {
                row.get(i)
                    .and_then(SqlValue::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            (text(0), text(1), text(2))
        })
        .collect()
}

fn describe(rows: &[(String, String, String)]) -> String {
    rows.iter()
        .map(|(name, state, error)| format!("  {name} [{state}] {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A jobs file that cannot run must fail `albedo build` — not boot, and
/// certainly not the first fire.
#[tokio::test]
async fn a_broken_schedule_fails_the_build_and_names_the_job() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(
        albedo(home.path(), work.path()).args(["init", "badjobs"]),
        "albedo init",
    );
    let app = work.path().join("badjobs");
    fs::write(
        app.join("src/jobs.ts"),
        r#"
            import { job } from "albedo/jobs";
            export const nightly = job({ schedule: "0 3 * *" }, () => {});
        "#,
    )
    .unwrap();

    let build = albedo(home.path(), &app)
        .arg("build")
        .output()
        .expect("albedo build runs");
    assert!(
        !build.status.success(),
        "a four-field cron expression must fail the build, not run every minute"
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        output.contains("nightly"),
        "the failure must name the job:\n{output}"
    );
    assert!(
        output.contains("0 3 * *"),
        "and the string it could not read:\n{output}"
    );
}

/// `enqueue()` from a form action: the request returns, the work runs after.
///
/// This is the shape 15.3 email needs and the reason this slice came before it.
/// A password reset cannot do its upstream call inline — a provider 503 would
/// turn a sign-up into a failed sign-up — so the action queues and returns, and
/// the retry is the queue's problem.
#[tokio::test]
async fn an_action_can_queue_work_that_runs_after_the_request_returns() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(
        albedo(home.path(), work.path()).args(["init", "queueapp"]),
        "albedo init",
    );
    let app = work.path().join("queueapp");
    fs::create_dir_all(app.join("src/lib")).unwrap();
    fs::write(app.join("src/lib/stamp.ts"), STAMP_TS).unwrap();
    // Only the enqueue-reachable job; no schedules, so nothing else can be
    // responsible for the row this test looks for.
    fs::write(
        app.join("src/jobs.ts"),
        r#"
            import { job, append } from "albedo/jobs";
            export const from_action = job({ retries: 1 }, (args) => {
              append("guestbook", { author: "from_action", message: "for " + args.who });
            });
        "#,
    )
    .unwrap();
    fs::write(app.join("src/routes/queue.tsx"), QUEUE_ROUTE_TSX).unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    let (_server, base) = serve(home.path(), &app).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    let response = client
        .get(format!("{base}/queue"))
        .send()
        .await
        .expect("the queue page renders");
    // Carried by hand rather than by a cookie store: this crate's `reqwest` is
    // built without the `cookies` feature, and the CSRF token is a cookie/field
    // pair — sending the field without the cookie is refused.
    let cookies: String = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let page = response.text().await.expect("body");

    // Submit the form the way a browser would: its own action URL, its own
    // hidden fields (CSRF included), and the visible input.
    let action_url = form_action(&page).expect("the form carries an action URL");
    let mut form: Vec<(String, String)> = hidden_fields(&page);
    form.push(("who".to_string(), "alice".to_string()));

    let posted = client
        .post(format!("{base}{action_url}"))
        .header(reqwest::header::COOKIE, &cookies)
        .form(&form)
        .send()
        .await
        .expect("the action accepts the form");
    assert!(
        posted.status().is_success() || posted.status().is_redirection(),
        "the action must accept the submit, got {}",
        posted.status()
    );

    // The row only exists if `enqueue()` wrote a queue row AND the runner
    // claimed it AND the body's `append` was applied.
    wait_for(&base, "for alice", Duration::from_secs(30)).await;

    let rows = job_rows(&app.join("forge.db")).await;
    let landed = rows
        .iter()
        .filter(|(name, state, _)| name == "from_action" && state == "done")
        .count();
    assert_eq!(
        landed, 1,
        "exactly one queued job should have run:\n{}",
        describe(&rows)
    );
}

/// The `action="…"` URL of the first form on the page.
///
/// Reads the attribute out of the whole tag rather than assuming it comes
/// first — the renderer is free to order attributes however it likes, and a
/// parser that assumed otherwise failed on the real output.
fn form_action(html: &str) -> Option<String> {
    let open = html.find("<form")?;
    let rest = &html[open..];
    let tag = &rest[..rest.find('>')?];
    let start = tag.find("action=\"")? + "action=\"".len();
    let value = &tag[start..];
    Some(value[..value.find('"')?].to_string())
}

/// Every `<input type="hidden">` on the page, as name/value pairs.
fn hidden_fields(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in html.split("<input").skip(1) {
        let Some(end) = chunk.find('>') else { continue };
        let tag = &chunk[..end];
        if !tag.contains("type=\"hidden\"") {
            continue;
        }
        let attr = |name: &str| -> Option<String> {
            let needle = format!("{name}=\"");
            let start = tag.find(&needle)? + needle.len();
            let rest = &tag[start..];
            Some(rest[..rest.find('"')?].to_string())
        };
        if let (Some(name), Some(value)) = (attr("name"), attr("value")) {
            out.push((name, value));
        }
    }
    out
}

/// The framework's own work reaches Rust, not the engine.
///
/// ## Why the row is injected rather than waited for
///
/// `albedo:expire_sessions` is `@hourly`, so a test that waited for it would
/// wait up to an hour. Injecting a **due row with the built-in's real name** and
/// watching the runner take it proves the thing actually in question — that a
/// claimed row whose name is under the framework's prefix dispatches to Rust and
/// succeeds — without inventing a second, faster code path that production would
/// never use.
///
/// The purge itself is pinned separately by `tests/auth_session_store.rs`. This
/// closes the other half: that something now *calls* it. Until 15.5 nothing did
/// — its only caller in the repository was that test.
#[tokio::test]
async fn a_builtin_job_is_claimed_and_run_by_the_framework_itself() {
    use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate, SqlValue};
    use dom_render_compiler::jobs::queue::JOBS;

    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(
        albedo(home.path(), work.path()).args(["init", "builtinapp"]),
        "albedo init",
    );
    let app = work.path().join("builtinapp");
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    // First boot: creates forge.db and the jobs table, then gets out of the way
    // so the test can write to the same file without contending for the writer.
    {
        let (_server, base) = serve(home.path(), &app).await;
        let _ = guestbook(&base).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let db_path = app.join("forge.db");
    let due_id = "albedo:expire_sessions@2026-01-01T00:00:00Z";
    {
        let db = LibSqlSubstrate::open_local(&db_path)
            .await
            .expect("the app's own database opens");
        db.execute(
            &format!(
                "INSERT OR REPLACE INTO {JOBS} \
                 (id, name, args, principal, state, run_at, attempts, max_attempts, \
                  lease_until, cursor, last_error, created_at, updated_at) \
                 VALUES (?, 'albedo:expire_sessions', '{{}}', NULL, 'pending', 1, 0, 3, \
                  NULL, NULL, NULL, 1, 1)"
            ),
            &[SqlValue::Text(due_id.to_string())],
        )
        .await
        .expect("the due row is written");
    }

    // Second boot: the runner should find it immediately — `run_at` is 1970.
    {
        let (_server, _base) = serve(home.path(), &app).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let db = LibSqlSubstrate::open_local(&db_path)
        .await
        .expect("reopens");
    let rows = db
        .query(
            &format!("SELECT state, last_error FROM {JOBS} WHERE id = ?"),
            &[SqlValue::Text(due_id.to_string())],
        )
        .await
        .expect("the row reads back");
    let row = rows.rows.first().expect("the injected row still exists");
    let state = row.get(0).and_then(SqlValue::as_str).unwrap_or("<none>");
    let error = row.get(1).and_then(SqlValue::as_str).unwrap_or("");

    assert_eq!(
        state, "done",
        "a built-in job must be claimed and run by the framework. state={state}, error={error}"
    );
}

/// The scheduler must survive a restart without re-firing a tick it already
/// ran — the exactly-once claim, at the level an operator experiences it.
#[tokio::test]
async fn a_restart_does_not_replay_a_tick_that_already_ran() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");

    run_ok(
        albedo(home.path(), work.path()).args(["init", "restartapp"]),
        "albedo init",
    );
    let app = work.path().join("restartapp");
    // Hourly: one fire is materialised at most, and the window this test runs
    // in cannot contain a second one — so any duplicate is a replay, not a
    // second legitimate tick.
    fs::write(
        app.join("src/jobs.ts"),
        r#"
            import { job } from "albedo/jobs";
            export const hourly = job({ schedule: "@hourly" }, () => {
                append("guestbook", { author: "hourly", message: "once" });
            });
        "#,
    )
    .unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    // First boot. Nothing is expected to fire — `@hourly`'s next instant is up
    // to an hour away — so this establishes the baseline.
    let before = {
        let (_server, base) = serve(home.path(), &app).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        guestbook(&base).await.matches("once").count()
    };

    // Second boot over the same forge.db.
    let (_server, base) = serve(home.path(), &app).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = guestbook(&base).await.matches("once").count();

    assert_eq!(
        before, after,
        "a restart re-fired an hourly tick: {before} rows before, {after} after. The fire id \
         is derived from the instant precisely so that a second process, or a second boot, \
         inserts nothing"
    );
}
