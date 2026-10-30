//! The QuickJS realm one test file runs in.
//!
//! It is a [`QuickJsEngine`] — the same prelude, the same `h`, the same module
//! compiler as the realm that renders a page — with three things added:
//!
//! * **the test API** (`prelude.js`), bound to `import … from "albedo/test"`;
//! * **lazy project modules.** A test imports `../components/Card`; the first
//!   import compiles that file from disk with the server's own module compiler
//!   and evaluates it here, then its imports, and so on. Only what the test
//!   reaches is loaded, in the order it reaches it;
//! * **a synchronous host bridge** — `__albedo_test_host(op, json)` — through
//!   which `render()`, `visit()` and the rest reach a [`TestHost`]: the build's
//!   renderers and the app's router, in the server crate.
//!
//! Module keys are paths **relative to the project**, forward-slashed
//! (`src/components/Card.tsx`). They are what a component reference carries to
//! the host, and relative keys read the same on every machine.

use crate::runtime::engine::RuntimeEngine;
use crate::runtime::quickjs_engine::{
    compile_module_script_for_quickjs_with_spec, QuickJsEngine, MODULE_MISSING_MARKER,
};
use rquickjs::promise::MaybePromise;
use rquickjs::{Ctx, Exception, Function};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

const PRELUDE: &str = include_str!("prelude.js");

/// Extensions a project import may omit, in the order the server probes them.
const MODULE_EXTENSIONS: [&str; 5] = ["tsx", "ts", "jsx", "js", "md"];

/// What a test's calls into the app land on.
///
/// `op` names the call and `payload` is its JSON argument; the answer is JSON
/// too. An `Err` becomes an `Error` thrown in the test, with the message as
/// written — so a refusal reads as the server's own sentence.
pub trait TestHost {
    /// Answer one call from a test.
    ///
    /// # Errors
    /// Whatever the app refused or failed with, as the test should read it.
    fn call(&mut self, op: &str, payload: serde_json::Value) -> Result<serde_json::Value, String>;
}

thread_local! {
    /// The host of the test now running on this thread. Set for the length of
    /// one [`TestRealm::run`] and cleared on the way out, so a call that
    /// arrives with no test running is an error rather than a stale answer.
    static HOST: RefCell<Option<Rc<RefCell<dyn TestHost>>>> = RefCell::new(None);
}

struct HostGuard;

impl HostGuard {
    fn install(host: Rc<RefCell<dyn TestHost>>) -> Self {
        HOST.with(|slot| *slot.borrow_mut() = Some(host));
        Self
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        HOST.with(|slot| *slot.borrow_mut() = None);
    }
}

/// One test a file registered.
#[derive(Debug, Clone, Deserialize)]
pub struct TestCase {
    /// `describe` names and the test's own, joined with ` › `.
    pub name: String,
    /// Not run: `test.skip`, `test.todo`, or another test is `.only`.
    pub skip: bool,
    /// Registered with `test.todo` — a skip that is a reminder.
    #[serde(default)]
    pub todo: bool,
}

/// How one test ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    /// Every assertion held.
    Passed,
    /// The reason, as the reporter should print it.
    Failed(String),
}

#[derive(Deserialize)]
struct RunEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// A realm for test files. One per file: a file's module state, its registered
/// tests and anything it did to globals stay with it.
pub struct TestRealm {
    engine: QuickJsEngine,
    project_dir: PathBuf,
}

impl TestRealm {
    /// A fresh realm for a project, with the build's npm bundles installed —
    /// `(key, script, source_hash)`, the same artifacts the server installs in
    /// its engines — so a component that imports a package loads here as it
    /// does there.
    ///
    /// # Errors
    /// The engine failed to start, a bundle failed to install, or the test
    /// prelude failed — each named.
    pub fn new(project_dir: &Path, npm: &[(String, String, u64)]) -> Result<Self, String> {
        let mut engine = QuickJsEngine::new();
        for (key, script, source_hash) in npm {
            engine
                .load_precompiled_module(key, script, *source_hash)
                .map_err(|err| format!("npm bundle '{key}' did not load into the test realm: {err}"))?;
        }
        let root = project_dir.to_path_buf();
        engine
            .with_ctx(|ctx| -> Result<(), String> {
                install_host_bridge(&ctx, root)
                    .map_err(|err| format!("the test host bridge did not install: {err}"))?;
                ctx.eval::<(), _>(PRELUDE)
                    .map_err(|err| format!("the test prelude failed: {}", describe(&ctx, &err)))
            })
            .map_err(|err| format!("the test realm did not start: {err}"))??;
        Ok(Self {
            engine,
            project_dir: project_dir.to_path_buf(),
        })
    }

    /// Load a test file and return the tests it registered, in order.
    ///
    /// # Errors
    /// The file could not be read, compiled or evaluated — a syntax error, an
    /// import that does not resolve, a top-level throw — with the reason.
    pub fn load_file(&mut self, file: &Path) -> Result<Vec<TestCase>, String> {
        let key = module_key(&self.project_dir, file)?;
        let source = std::fs::read_to_string(file)
            .map_err(|err| format!("could not read {}: {err}", file.display()))?;
        let script = compile_module_script_for_quickjs_with_spec(&key, &source, None)
            .map_err(|err| err.to_string())?;
        self.engine
            .with_ctx(|ctx| -> Result<String, String> {
                ctx.eval::<(), _>("__albedo_test_reset();")
                    .map_err(|err| describe(&ctx, &err))?;
                ctx.eval::<(), _>(script.as_str())
                    .map_err(|err| describe(&ctx, &err))?;
                let list: Function = ctx
                    .globals()
                    .get("__albedo_test_list")
                    .map_err(|err| describe(&ctx, &err))?;
                list.call::<(), String>(()).map_err(|err| describe(&ctx, &err))
            })
            .map_err(|err| err.to_string())?
            .and_then(|json| {
                serde_json::from_str(&json)
                    .map_err(|err| format!("the test list was unreadable: {err}"))
            })
    }

    /// Run the `index`-th test of the loaded file against `host`, stopping it
    /// once `timeout` has passed.
    pub fn run(
        &mut self,
        index: usize,
        host: Rc<RefCell<dyn TestHost>>,
        timeout: Duration,
    ) -> TestOutcome {
        let _guard = HostGuard::install(host);
        self.engine.set_deadline(Some(Instant::now() + timeout));
        let settled = self.engine.with_ctx(|ctx| -> Result<String, String> {
            let run: Function = ctx
                .globals()
                .get("__albedo_test_run")
                .map_err(|err| describe(&ctx, &err))?;
            let index = i32::try_from(index).unwrap_or(i32::MAX);
            match run.call::<(i32,), MaybePromise>((index,)) {
                Ok(pending) => match pending.finish::<String>() {
                    Ok(json) => Ok(json),
                    Err(rquickjs::Error::WouldBlock) => Err(
                        "the test awaited something that never settles — a timer, or a promise \
                         nothing resolves. albedo test has no event loop: visit(), render() and \
                         signIn() answer before they return, so a test never needs to wait."
                            .to_string(),
                    ),
                    Err(err) => Err(describe(&ctx, &err)),
                },
                Err(err) => Err(describe(&ctx, &err)),
            }
        });
        self.engine.set_deadline(None);
        if self.engine.take_interrupted() {
            return TestOutcome::Failed(format!(
                "timed out after {} s — the test was stopped",
                timeout.as_secs_f64()
            ));
        }
        match settled {
            Ok(Ok(json)) => match serde_json::from_str::<RunEnvelope>(&json) {
                Ok(RunEnvelope { ok: true, .. }) => TestOutcome::Passed,
                Ok(RunEnvelope { error, .. }) => {
                    TestOutcome::Failed(error.unwrap_or_else(|| "failed".to_string()))
                }
                Err(err) => TestOutcome::Failed(format!("the test result was unreadable: {err}")),
            },
            Ok(Err(reason)) => TestOutcome::Failed(reason),
            Err(err) => TestOutcome::Failed(err.to_string()),
        }
    }
}

/// `src/components/Card.tsx` for `<project>/src/components/Card.tsx`.
fn module_key(project_dir: &Path, file: &Path) -> Result<String, String> {
    let relative = file.strip_prefix(project_dir).map_err(|_| {
        format!(
            "{} is outside the project at {}",
            file.display(),
            project_dir.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn describe(ctx: &Ctx<'_>, err: &rquickjs::Error) -> String {
    plain_words(describe_raw(ctx, err))
}

/// The engine's internal markers, said in the words a test author searches for.
fn plain_words(message: String) -> String {
    let Some(at) = message.find(MODULE_MISSING_MARKER) else {
        return message;
    };
    let missing = message[at + MODULE_MISSING_MARKER.len()..]
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if missing.contains('/') || missing.contains('\\') {
        format!(
            "cannot find module '{missing}' — no {} file there. Imports are relative \
             (\"./Card\", \"../components/Card\") and resolve against the project.",
            MODULE_EXTENSIONS.join(" / ")
        )
    } else {
        format!(
            "cannot find module '{missing}'. A package must be installed (`npm i {missing}`) \
             and imported by something the build compiles; albedo test runs the bundles the \
             build made."
        )
    }
}

fn describe_raw(ctx: &Ctx<'_>, err: &rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(exception) = caught.as_exception() {
            let message = exception.message().unwrap_or_default();
            let name = exception
                .get::<_, String>("name")
                .ok()
                .filter(|name| name != "Error")
                .map(|name| format!("{name}: "))
                .unwrap_or_default();
            return format!("{name}{message}");
        }
        if let Ok(text) = caught.get::<rquickjs::Coerced<String>>() {
            return format!("threw {}", text.0);
        }
    }
    err.to_string()
}

fn install_host_bridge(ctx: &Ctx<'_>, project_dir: PathBuf) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let host = Function::new(ctx.clone(), |op: String, payload: String| -> String {
        let envelope = match HOST.with(|slot| slot.borrow().clone()) {
            None => Err("no test is running — albedo/test's functions work inside test()".to_string()),
            Some(host) => match serde_json::from_str::<serde_json::Value>(&payload) {
                Ok(payload) => host.borrow_mut().call(&op, payload),
                Err(err) => Err(format!("the test's call to '{op}' was unreadable: {err}")),
            },
        };
        match envelope {
            Ok(value) => serde_json::json!({ "ok": true, "value": value }).to_string(),
            Err(error) => serde_json::json!({ "ok": false, "error": error }).to_string(),
        }
    })?;
    globals.set("__albedo_test_host", host)?;

    // Modules mid-load, so an import cycle is a named error rather than a
    // stack overflow.
    let loading: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    let load = Function::new(ctx.clone(), move |ctx: Ctx<'_>, base: String| -> rquickjs::Result<String> {
        let Some(key) = find_module(&project_dir, &base) else {
            return Ok(String::new());
        };
        if !loading.borrow_mut().insert(key.clone()) {
            return Err(Exception::throw_message(
                &ctx,
                &format!("{key} imports itself through a cycle; albedo test loads modules in import order and cannot finish a cycle"),
            ));
        }
        let outcome = (|| {
            let path = project_dir.join(&key);
            let source = crate::markdown::read_module_source(&path).map_err(|err| {
                Exception::throw_message(&ctx, &format!("could not read {key}: {err}"))
            })?;
            let script = compile_module_script_for_quickjs_with_spec(&key, &source, None)
                .map_err(|err| Exception::throw_message(&ctx, &err.to_string()))?;
            ctx.eval::<(), _>(script.as_str())
        })();
        loading.borrow_mut().remove(&key);
        outcome.map(|()| key)
    })?;
    globals.set("__albedo_test_load", load)?;
    Ok(())
}

/// The project-relative key of the file an import resolves to, probing the
/// extensions the server's resolver probes — or `None`.
fn find_module(project_dir: &Path, base: &str) -> Option<String> {
    let base = base.trim_start_matches("./");
    let has_extension = Path::new(base)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| MODULE_EXTENSIONS.contains(&ext));
    let mut candidates = Vec::new();
    if has_extension {
        candidates.push(base.to_string());
    }
    for ext in MODULE_EXTENSIONS {
        candidates.push(format!("{base}.{ext}"));
    }
    for ext in MODULE_EXTENSIONS {
        candidates.push(format!("{base}/index.{ext}"));
    }
    candidates
        .into_iter()
        .find(|candidate| project_dir.join(candidate).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_resolve_against_the_project_with_the_servers_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/components/card")).unwrap();
        std::fs::write(dir.path().join("src/components/Hero.tsx"), "x").unwrap();
        std::fs::write(dir.path().join("src/components/card/index.ts"), "x").unwrap();

        assert_eq!(
            find_module(dir.path(), "src/components/Hero").as_deref(),
            Some("src/components/Hero.tsx")
        );
        assert_eq!(
            find_module(dir.path(), "src/components/card").as_deref(),
            Some("src/components/card/index.ts")
        );
        assert_eq!(find_module(dir.path(), "src/components/Missing"), None);
    }

    #[test]
    fn a_missing_import_is_named_without_the_engines_marker() {
        let project = plain_words(format!("Error: {MODULE_MISSING_MARKER}src/components/Gone"));
        assert!(project.starts_with("cannot find module 'src/components/Gone'"), "{project}");
        assert!(!project.contains(MODULE_MISSING_MARKER));

        let package = plain_words(format!("{MODULE_MISSING_MARKER}left-pad"));
        assert!(package.contains("npm i left-pad"), "{package}");
    }
}
