//! Finding `src/jobs.ts` and reading what it declares.
//!
//! Everything here is read from the already-parsed project, so `albedo build`
//! and boot reach the same answer from the same facts — the rule
//! [`crate::preflight`] exists to keep.
//!
//! ## The shape
//!
//! ```ts
//! import { job } from "albedo/jobs";
//!
//! export const expire_sessions = job(
//!   { schedule: "@hourly" },
//!   async () => { /* … */ },
//! );
//!
//! export const send_receipt = job(
//!   { retries: 5 },
//!   async ({ order }) => { /* … */ },
//! );
//! ```
//!
//! The schedule sits with the body rather than in `albedo.config.ts` for the
//! reason 15.6 put the matcher in `middleware.ts`: a name in one file pointing
//! at a function in another is a pair that can drift, and the drift is silent —
//! the job simply stops running, which is the failure nobody notices.
//!
//! ## Identity, and why there is no `as: "system"`
//!
//! A job runs **for** a principal, never **as** the framework. There are three
//! ways it gets one and none of them is a bypass:
//!
//! - enqueued from an action → it inherits the caller's principal, so the work
//!   runs as the user who caused it;
//! - scheduled, plain → anonymous, exactly like an unauthenticated request, and
//!   an identity-partitioned read is refused exactly as it is there;
//! - scheduled with [`FanOut::Users`] → the tick expands into one run per
//!   principal, each carrying exactly one.
//!
//! That third case is the whole point. A per-user nightly digest is the reason
//! frameworks grow a god-mode principal, and the fan-out delivers it while the
//! number of partition bypasses in this codebase stays **zero**. There is no
//! privileged identity to leak, to forget to check, or to inherit by accident.

use std::collections::BTreeMap;
use std::time::Duration;

use swc_ecma_ast::{Expr, Lit, Prop, PropName, PropOrSpread};

use super::schedule::{Schedule, ScheduleError};
use crate::runtime::compiled::CompiledProject;

/// The file names a jobs module may have, relative to the project root (`src/`).
pub const ENTRY_NAMES: &[&str] = &["jobs.ts", "jobs.tsx", "jobs.js", "jobs.jsx"];

/// The import the declaration comes from: `import { job } from "albedo/jobs"`.
pub const JOBS_MODULE: &str = "albedo/jobs";

/// How long a run gets before it is interrupted, when it declares nothing.
///
/// Generous next to a request — a job is the place slow work was moved *to* —
/// and still finite, because the engine running it is one of a small pool and a
/// body that never returns would otherwise retire a slot for the life of the
/// process.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Ceiling on a declared timeout.
///
/// A bound on declarations, not on ambition: work that genuinely needs longer
/// than this wants to be several enqueued jobs, so that a restart costs one
/// step rather than the whole thing.
pub const MAX_TIMEOUT_MS: u64 = 15 * 60 * 1000;

/// Attempts a job gets when it declares nothing.
///
/// One. Retrying by default would silently re-run every body that is not
/// idempotent, and "it ran twice" is a worse first experience than "it failed
/// and said so".
pub const DEFAULT_ATTEMPTS: u32 = 1;

/// Ceiling on declared retries.
pub const MAX_RETRIES: u32 = 50;

/// How many principals one pass of a fan-out expands.
///
/// The number that keeps a 100 000-user digest from being a 100 000-row write
/// at 03:00: each claim takes this many, enqueues them, advances the cursor and
/// yields the row. Write amplification is O(batch), not O(principals).
pub const FANOUT_BATCH: usize = 500;

/// What a scheduled job expands over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanOut {
    /// One run per registered principal, each carrying that principal.
    Users,
}

impl FanOut {
    /// The spelling a declaration uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Users => "users",
        }
    }
}

/// One `export const <name> = job({ … }, handler)`.
#[derive(Debug, Clone)]
pub struct JobDecl {
    /// The export name. This is the job's identity everywhere — in the queue
    /// row, in `enqueue()`, in the fire id.
    pub name: String,
    /// When it runs on its own. `None` means it runs only when enqueued.
    pub schedule: Option<Schedule>,
    /// The schedule string exactly as written, for error messages and reports.
    pub schedule_source: Option<String>,
    /// What a scheduled run expands over.
    pub fan_out: Option<FanOut>,
    /// Total attempts, including the first.
    pub max_attempts: u32,
    /// Per-run deadline.
    pub timeout: Duration,
}

/// A project's jobs, as the build found them.
#[derive(Debug, Clone)]
pub struct JobsDecl {
    /// The entry's project-relative spec, e.g. `jobs.ts`.
    pub entry: String,
    /// Every declared job, in source order.
    pub jobs: Vec<JobDecl>,
}

impl JobsDecl {
    /// The job with this name, if it is declared.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&JobDecl> {
        self.jobs.iter().find(|job| job.name == name)
    }

    /// Every job that runs on a schedule.
    pub fn scheduled(&self) -> impl Iterator<Item = &JobDecl> {
        self.jobs.iter().filter(|job| job.schedule.is_some())
    }
}

/// Find and validate the project's jobs. `Ok(None)` when there are none.
///
/// # Errors
/// Every problem found, each naming the file and the export.
pub fn declaration(project: &CompiledProject) -> Result<Option<JobsDecl>, Vec<String>> {
    let mut problems = Vec::new();
    let root = project.project().root();

    // The same refusal `middleware.ts` gets, for the same reason: a file at the
    // project root is never loaded, and a guard that silently guards nothing is
    // worse than no guard.
    if let Some(project_dir) = root.parent() {
        for name in ENTRY_NAMES {
            if project_dir.join(name).is_file() && !same_dir(project_dir, root) {
                problems.push(format!(
                    "{name} is at the project root, where it is never loaded. Move it into \
                     {}/ — the directory `root` in albedo.config.ts points at",
                    root.file_name().and_then(|n| n.to_str()).unwrap_or("src")
                ));
            }
        }
    }

    let found: Vec<&str> = ENTRY_NAMES
        .iter()
        .copied()
        .filter(|name| project.module(name).is_some())
        .collect();

    let entry = match found.as_slice() {
        [] => {
            return if problems.is_empty() {
                Ok(None)
            } else {
                Err(problems)
            }
        }
        [one] => *one,
        many => {
            problems.push(format!(
                "more than one jobs file: {} — there can be only one, because two would \
                 declare schedules nothing orders",
                many.join(", ")
            ));
            return Err(problems);
        }
    };

    let module = project.module(entry).expect("found above");

    // The local name `job` was bound to, so a renamed import still works and an
    // unrelated local function called `job` does not.
    let local_job_name = module
        .imports
        .iter()
        .find(|(_, binding)| binding.source == JOBS_MODULE && binding.export_name == "job")
        .map(|(local, _)| local.clone());

    let mut jobs: Vec<JobDecl> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();

    for (name, expr) in &module.module_constants {
        let Some(args) = job_call_args(expr, local_job_name.as_deref()) else {
            continue;
        };
        if seen.insert(name.clone(), ()).is_some() {
            problems.push(format!(
                "{entry}: `{name}` is declared twice — a job's name is its identity in the \
                 queue, so two would share rows"
            ));
            continue;
        }
        match lower(name, args) {
            Ok(decl) => jobs.push(decl),
            Err(messages) => {
                problems.extend(messages.into_iter().map(|m| format!("{entry}: {m}")));
            }
        }
    }

    // A jobs file that declares nothing is a file whose author believes
    // something is scheduled. Say so rather than booting with an empty runner.
    if jobs.is_empty() && problems.is_empty() {
        problems.push(format!(
            "{entry} declares no jobs. Export at least one: \
             `export const nightly = job({{ schedule: \"@daily\" }}, async () => {{ … }})` — \
             or delete the file"
        ));
    }

    if let Err(message) = crate::middleware::declare::module_graph(project, entry) {
        problems.push(message);
    }

    if problems.is_empty() {
        Ok(Some(JobsDecl {
            entry: entry.to_string(),
            jobs,
        }))
    } else {
        Err(problems)
    }
}

/// The entry and every project module it reaches, dependencies first.
///
/// Deliberately the middleware traversal rather than a second copy: the
/// question *"what does this entry load, in what order"* has one answer, and
/// this codebase's expensive scars are all two implementations of one contract
/// drifting apart (see `project_slot_value_encoding`).
///
/// # Errors
/// A relative import that names no project module.
pub fn module_graph(
    project: &CompiledProject,
    entry: &str,
) -> Result<Vec<(String, String)>, String> {
    crate::middleware::declare::module_graph(project, entry)
}

/// The argument list of a `job(...)` call, if that is what this expression is.
fn job_call_args<'a>(expr: &'a Expr, local_job_name: Option<&str>) -> Option<&'a [swc_ecma_ast::ExprOrSpread]> {
    let Expr::Call(call) = unparen(expr) else {
        return None;
    };
    let callee = call.callee.as_expr()?;
    let Expr::Ident(ident) = unparen(callee) else {
        return None;
    };
    // Bound to the import, not merely spelled `job`. A file with its own
    // `const job = …` must not have its consts read as declarations.
    if Some(ident.sym.as_ref()) != local_job_name {
        return None;
    }
    Some(&call.args)
}

/// Read one declaration's options object.
fn lower(name: &str, args: &[swc_ecma_ast::ExprOrSpread]) -> Result<JobDecl, Vec<String>> {
    let mut problems = Vec::new();

    if args.is_empty() {
        return Err(vec![format!(
            "`{name}` calls job() with no arguments. It takes options and a handler: \
             `job({{ schedule: \"@daily\" }}, async () => {{ … }})`"
        )]);
    }
    if args.len() < 2 {
        problems.push(format!(
            "`{name}` calls job() with one argument. It takes options *and* a handler: \
             `job({{ … }}, async () => {{ … }})`"
        ));
    }

    let Expr::Object(object) = unparen(&args[0].expr) else {
        return Err(vec![format!(
            "`{name}`'s first argument must be an object literal — it is read at build time, \
             so a computed one has nothing to read"
        )]);
    };

    let mut schedule_source: Option<String> = None;
    let mut fan_out: Option<FanOut> = None;
    let mut retries: Option<u32> = None;
    let mut timeout_ms: Option<u64> = None;

    for prop in &object.props {
        let PropOrSpread::Prop(prop) = prop else {
            problems.push(format!(
                "`{name}` cannot use a spread in its options — every key must be written out, \
                 because they are read at build time"
            ));
            continue;
        };
        let Prop::KeyValue(kv) = &**prop else {
            problems.push(format!("`{name}`'s options must use `key: value` properties"));
            continue;
        };
        let key = match &kv.key {
            PropName::Ident(ident) => ident.sym.to_string(),
            PropName::Str(s) => s.value.to_string(),
            _ => {
                problems.push(format!("`{name}`'s option keys must be plain names"));
                continue;
            }
        };
        match key.as_str() {
            "schedule" => match string_literal(&kv.value) {
                Some(value) => schedule_source = Some(value),
                None => problems.push(format!(
                    "`{name}.schedule` must be a string literal: \"0 3 * * *\", \"@daily\", \
                     or \"every 5m\""
                )),
            },
            "over" => match string_literal(&kv.value).as_deref() {
                Some("users") => fan_out = Some(FanOut::Users),
                Some(other) => problems.push(format!(
                    "`{name}.over` is \"{other}\"; the only fan-out is \"users\""
                )),
                None => problems.push(format!("`{name}.over` must be the string \"users\"")),
            },
            "retries" => match number_literal(&kv.value) {
                Some(value) if value <= f64::from(MAX_RETRIES) && value >= 0.0 => {
                    retries = Some(value as u32);
                }
                Some(_) => problems.push(format!(
                    "`{name}.retries` must be between 0 and {MAX_RETRIES}"
                )),
                None => problems.push(format!("`{name}.retries` must be a number literal")),
            },
            "timeout" => match string_literal(&kv.value) {
                Some(value) => match parse_timeout(&value) {
                    Ok(ms) => timeout_ms = Some(ms),
                    Err(message) => problems.push(format!("`{name}.timeout` {message}")),
                },
                None => problems.push(format!(
                    "`{name}.timeout` must be a string literal: \"30s\", \"2m\""
                )),
            },
            other => problems.push(format!(
                "`{name}.{other}` is not an option; the options are \
                 schedule, over, retries and timeout"
            )),
        }
    }

    let schedule = match &schedule_source {
        None => None,
        Some(source) => match Schedule::parse(source) {
            Ok(schedule) => Some(schedule),
            Err(err) => {
                problems.push(format!("`{name}.schedule`: {err}"));
                None
            }
        },
    };

    // A fan-out with no tick has nothing to expand. Silently ignoring `over`
    // would leave an author believing a per-user job exists when the only way
    // to reach it is an enqueue that already carries a principal.
    if fan_out.is_some() && schedule_source.is_none() {
        problems.push(format!(
            "`{name}` declares `over` without `schedule`. A fan-out expands a scheduled fire \
             into one run per principal; an enqueued job already carries the principal that \
             enqueued it"
        ));
    }

    if !problems.is_empty() {
        return Err(problems);
    }

    Ok(JobDecl {
        name: name.to_string(),
        schedule,
        schedule_source,
        fan_out,
        max_attempts: retries.map_or(DEFAULT_ATTEMPTS, |r| r.saturating_add(1)),
        timeout: Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
    })
}

fn parse_timeout(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("is \"{value}\"; write a unit too, like \"30s\" or \"2m\""))?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("is \"{value}\"; the number could not be read"))?;
    let ms = match unit.trim() {
        "ms" => number,
        "s" | "sec" | "secs" | "second" | "seconds" => number.saturating_mul(1000),
        "m" | "min" | "mins" | "minute" | "minutes" => number.saturating_mul(60_000),
        other => return Err(format!("has an unknown unit \"{other}\"; use ms, s or m")),
    };
    if ms == 0 {
        return Err("is zero, so the body would be interrupted before it started".to_string());
    }
    if ms > MAX_TIMEOUT_MS {
        return Err(format!(
            "is longer than the {MAX_TIMEOUT_MS}ms ceiling. Work that needs longer wants to be \
             several enqueued jobs, so a restart costs one step instead of all of it"
        ));
    }
    Ok(ms)
}

fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match unparen(expr) {
        Expr::Lit(Lit::Str(s)) => Some(s.value.to_string()),
        Expr::Tpl(tpl) if tpl.exprs.is_empty() && tpl.quasis.len() == 1 => tpl.quasis[0]
            .cooked
            .as_ref()
            .map(|cooked| cooked.to_string()),
        _ => None,
    }
}

fn number_literal(expr: &Expr) -> Option<f64> {
    match unparen(expr) {
        Expr::Lit(Lit::Num(n)) => Some(n.value),
        _ => None,
    }
}

fn unparen(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(paren) => unparen(&paren.expr),
        Expr::TsAs(as_expr) => unparen(&as_expr.expr),
        Expr::TsConstAssertion(assertion) => unparen(&assertion.expr),
        Expr::TsSatisfies(satisfies) => unparen(&satisfies.expr),
        other => other,
    }
}

/// Surfaced so `albedo build`'s reporting can print a schedule without
/// re-deriving it. Not used by the runner, which holds the parsed [`Schedule`].
impl std::fmt::Display for JobDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.schedule_source, self.fan_out) {
            (Some(source), Some(fan)) => {
                write!(f, "{} · {source} · per {}", self.name, fan.as_str())
            }
            (Some(source), None) => write!(f, "{} · {source}", self.name),
            (None, _) => write!(f, "{} · on enqueue", self.name),
        }
    }
}

/// Re-exported so callers can name the parse failure without reaching into
/// [`super::schedule`].
pub type DeclaredScheduleError = ScheduleError;

#[cfg(test)]
mod tests {
    use super::*;

    /// A real project on disk, loaded the way boot and build load one — so
    /// these exercise the parser's actual view of the file rather than a
    /// hand-built module that could agree with a broken reader.
    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, CompiledProject) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("routes")).unwrap();
        std::fs::write(
            src.join("routes/index.tsx"),
            "export default function Home() { return <main>home</main>; }",
        )
        .unwrap();
        for (path, body) in files {
            let full = src.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let compiled = CompiledProject::load_from_dir(&src).expect("project loads");
        (dir, compiled)
    }

    fn declare(source: &str) -> Result<Option<JobsDecl>, Vec<String>> {
        let (_dir, compiled) = project(&[("jobs.ts", source)]);
        declaration(&compiled)
    }

    #[test]
    fn no_jobs_file_is_not_an_error() {
        let (_dir, compiled) = project(&[]);
        assert!(declaration(&compiled).expect("no file is fine").is_none());
    }

    #[test]
    fn a_scheduled_job_is_read() {
        let decl = declare(
            r#"import { job } from "albedo/jobs";
               export const nightly = job({ schedule: "0 3 * * *" }, async () => {});"#,
        )
        .expect("declares")
        .expect("some");
        assert_eq!(decl.jobs.len(), 1);
        let nightly = decl.get("nightly").expect("named");
        assert_eq!(nightly.schedule_source.as_deref(), Some("0 3 * * *"));
        assert!(nightly.schedule.is_some());
        assert_eq!(nightly.max_attempts, DEFAULT_ATTEMPTS);
    }

    #[test]
    fn an_enqueue_only_job_has_no_schedule() {
        let decl = declare(
            r#"import { job } from "albedo/jobs";
               export const deliver = job({ retries: 4 }, async () => {});"#,
        )
        .expect("declares")
        .expect("some");
        let deliver = decl.get("deliver").expect("named");
        assert!(deliver.schedule.is_none());
        assert_eq!(deliver.max_attempts, 5, "retries are attempts after the first");
        assert_eq!(decl.scheduled().count(), 0);
    }

    #[test]
    fn a_renamed_import_still_declares() {
        let decl = declare(
            r#"import { job as defineJob } from "albedo/jobs";
               export const nightly = defineJob({ schedule: "@daily" }, async () => {});"#,
        )
        .expect("declares")
        .expect("some");
        assert!(decl.get("nightly").is_some());
    }

    /// The falsifier for the import check: a local function named `job` must
    /// not have its results read as declarations.
    #[test]
    fn a_local_function_called_job_declares_nothing() {
        let result = declare(
            r#"const job = (opts: unknown, fn: unknown) => fn;
               export const nightly = job({ schedule: "@daily" }, async () => {});"#,
        );
        let problems = result.expect_err("a file with no declarations is refused");
        assert!(
            problems.iter().any(|p| p.contains("declares no jobs")),
            "expected the empty-file refusal, got {problems:?}"
        );
    }

    #[test]
    fn over_without_schedule_is_refused() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               export const digest = job({ over: "users" }, async () => {});"#,
        )
        .expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("without `schedule`")),
            "got {problems:?}"
        );
    }

    #[test]
    fn a_fan_out_over_users_is_read() {
        let decl = declare(
            r#"import { job } from "albedo/jobs";
               export const digest = job({ schedule: "@daily", over: "users" }, async () => {});"#,
        )
        .expect("declares")
        .expect("some");
        assert_eq!(decl.get("digest").expect("named").fan_out, Some(FanOut::Users));
    }

    #[test]
    fn a_misspelled_option_is_named_rather_than_ignored() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               export const nightly = job({ schedul: "@daily" }, async () => {});"#,
        )
        .expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("schedul")),
            "a typo'd key must be named, or the job silently never runs: {problems:?}"
        );
    }

    #[test]
    fn a_bad_schedule_names_the_job_and_the_string() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               export const nightly = job({ schedule: "0 3 * *" }, async () => {});"#,
        )
        .expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("nightly") && p.contains("0 3 * *")),
            "got {problems:?}"
        );
    }

    #[test]
    fn a_computed_options_object_is_refused() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               const opts = { schedule: "@daily" };
               export const nightly = job(opts, async () => {});"#,
        )
        .expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("object literal")),
            "got {problems:?}"
        );
    }

    #[test]
    fn a_missing_handler_is_refused() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               export const nightly = job({ schedule: "@daily" });"#,
        )
        .expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("handler")),
            "got {problems:?}"
        );
    }

    #[test]
    fn timeouts_parse_and_are_bounded() {
        assert_eq!(parse_timeout("500ms"), Ok(500));
        assert_eq!(parse_timeout("30s"), Ok(30_000));
        assert_eq!(parse_timeout("2m"), Ok(120_000));
        assert!(parse_timeout("0s").is_err());
        assert!(parse_timeout("60m").is_err(), "over the ceiling");
        assert!(parse_timeout("30").is_err(), "no unit");
        assert!(parse_timeout("30q").is_err(), "unknown unit");
    }

    #[test]
    fn retries_are_bounded() {
        let problems = declare(
            r#"import { job } from "albedo/jobs";
               export const nightly = job({ retries: 9999 }, async () => {});"#,
        )
        .expect_err("refused");
        assert!(problems.iter().any(|p| p.contains("retries")), "got {problems:?}");
    }

    #[test]
    fn two_jobs_with_one_name_are_refused() {
        // `export const` twice is a TypeScript error too, but the queue reason
        // is separate and worth its own message: the name is the row's identity.
        let (_dir, compiled) = project(&[(
            "jobs.ts",
            r#"import { job } from "albedo/jobs";
               export const a = job({ schedule: "@daily" }, async () => {});
               export const b = job({ schedule: "@hourly" }, async () => {});"#,
        )]);
        let decl = declaration(&compiled).expect("declares").expect("some");
        assert_eq!(decl.jobs.len(), 2, "distinct names both land");
        assert_eq!(decl.scheduled().count(), 2);
    }

    #[test]
    fn an_empty_jobs_file_is_refused() {
        let problems = declare("export const nothing = 1;").expect_err("refused");
        assert!(
            problems.iter().any(|p| p.contains("declares no jobs")),
            "got {problems:?}"
        );
    }
}
