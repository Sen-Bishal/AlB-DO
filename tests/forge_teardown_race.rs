//! The libSQL teardown crash — `0xc0000005 STATUS_ACCESS_VIOLATION` — and the
//! measurements that say what actually causes it.
//!
//! # ✅ RESOLVED 14-15 Nov 2026 (T-0040) — read this before the history below
//!
//! A stack captured in-process (a vectored exception handler printing a
//! backtrace) put the fault in `sqlite3_close_v2` ← `local::Connection::drop`
//! inside `drop_glue<LibsqlConnection>`. libsql 0.6.0 closes every local
//! connection **twice**: `LibsqlConnection::drop` disconnects, then its
//! `local::Connection` field drops and disconnects again. The second close
//! reads the freed `sqlite3` struct, which faults only once the Windows heap
//! has released that page. Co-liveness mattered only because a bigger heap
//! releases more pages. The fix is libsql 0.10, which nulls the handle after
//! closing (an in-repo wrapper bridged it for a day; see `forge::libsql`).
//!
//! Measured on the dev rig, 15 Nov: libsql 0.6.0 `probe_i` 13/20 crashed;
//! libsql 0.10.0 `probe_i` 0/40, `probe_a` 0/20, `thirty_simultaneous_teardowns`
//! 0/20. `probe_i` is now a normal test; the rest stay ignored as the record of
//! the investigation. Every-ordering cases: `tests/forge_connection_ownership.rs`.
//!
//! Everything below is the September investigation, kept as written. Its
//! "must stay ignored" rule predates CI's `--no-fail-fast`.
//!
//! # 🛑 EVERY PROBE HERE IS `#[ignore]`d, AND MUST STAY THAT WAY
//!
//! They reproduce a **process abort**, at up to 70%. A crash does not fail one
//! test: it kills the binary, so `cargo test` stops there, every binary after it
//! never runs, and the truncated log ends in an unbroken run of `ok` that reads
//! as a pass. An un-ignored probe would not report the bug — it would hide the
//! rest of the suite behind it.
//!
//! Run them deliberately:
//!
//! ```text
//! cargo test --test forge_teardown_race -- --ignored --test-threads=1
//! ```
//!
//! and expect failures. That is what they are for.
//!
//! # What is known, and what was wrong
//!
//! Measured 2026-09-02 — one machine, one binary, 30 substrates per run, 30–40
//! runs per row:
//!
//! | shape | co-live | drop thread | crashes |
//! |---|---|---|---|
//! | `probe_j_interleaved_inline` | 1 | async worker, inline | **0 / 40** |
//! | `probe_e_single_threaded_one_at_a_time` | 1 | `current_thread` runtime | **0 / 40** |
//! | `probe_f_multithread_one_worker` | 1 | 1-worker runtime | **0 / 30** |
//! | `probe_g_drop_on_blocking_thread` | 1 | `spawn_blocking` | **0 / 30** |
//! | `probe_h_drop_on_dedicated_os_thread` | 1 | fresh OS thread | **0 / 40** |
//! | `probe_c_never_dropped` | 30 | never dropped | **0 / 30** |
//! | `thirty_simultaneous_teardowns` | 30 | concurrent tasks | 2–5 / 40 |
//! | `probe_a_sequential_drops` | 30 | async worker, inline | 14–20 / 40 |
//! | `probe_i_batch_open_then_drop_all_on_one_os_thread` | 30 | one fresh OS thread | **28 / 40** |
//!
//! 🔑 **The variable is co-liveness — not concurrency, and not the thread.** The
//! crash needs many substrates open *at the same time*, and then a teardown.
//! Thirty dropped strictly one at a time, on a single dedicated OS thread, with
//! nothing overlapping anywhere, is the **worst** row in the table. Interleaving
//! so only one is ever alive is clean in every configuration tried.
//!
//! 🔴 **This refutes the recorded diagnosis** in
//! `development-plan/ACCESS_VIOLATION.md`, which concluded the hazard was two
//! teardowns *overlapping*. Its two arms — "30 cycles one after another" (0/60)
//! and "the same 30 all in flight at once" (13/60) — differ in **both** overlap
//! and co-liveness, and it credited overlap. The rows above hold overlap at zero
//! and still crash.
//!
//! 🪤 **Two fixes were built on that reading and both were refuted by their own
//! measurement**: a process-wide teardown mutex (9/30 — no effect) and a
//! dedicated teardown thread with every handle shipped to it (20/40). Neither is
//! in the tree, because an unproven mechanism left in place is indistinguishable
//! from a fix to the next reader.
//!
//! ⚠️ **Still unexplained**, and it needs a stack rather than another count: why
//! co-liveness matters at all. No debugger is installed on the dev rig; enabling
//! WER local dumps or installing `cdb`/`procdump` is the next step.
//!
//! ✅ **One real fix did come out of this**, in `forge::libsql`: `Drop` deleted
//! the ephemeral directory *before* closing the connections, so on Windows the
//! delete hit a locked file and silently leaked. 31 510 leaked `forge-*`
//! directories had accumulated on the dev rig. Closing first makes it succeed —
//! measured delta **0**.

use dom_render_compiler::forge::libsql::LibSqlSubstrate;
use dom_render_compiler::forge::substrate::DataSubstrate;

/// Substrates per run. 30 is where the signal is strong; 1 is clean everywhere.
const WIDTH: usize = 30;

/// Held by every non-ignored test that opens ephemeral substrates.
/// [`an_ephemeral_substrate_deletes_its_directory_on_drop`] counts `forge-*`
/// directories in the shared temp dir, so a test opening thirty of them on a
/// parallel thread made it read eleven "leaks" from eight drops (14 Nov 2026,
/// once `probe_i` stopped being ignored).
static TEMP_DIR_COUNTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir_turn() -> std::sync::MutexGuard<'static, ()> {
    // A test that panicked holding the lock poisoned it; the lock guards no
    // data, so the next test may still take its turn.
    TEMP_DIR_COUNTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn multi_thread(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("runtime")
}

async fn open_one() -> LibSqlSubstrate {
    let substrate = LibSqlSubstrate::open_ephemeral()
        .await
        .expect("open ephemeral substrate");
    substrate
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
        .await
        .expect("create table");
    substrate
}

// ── co-live = 30 · these crashed before T-0040 ────────────────────────────

/// 30 held open, then dropped **one at a time on one fresh OS thread**. No
/// overlap anywhere, and the highest crash rate in the table — the row that
/// refutes "overlapping teardowns".
///
/// **The regression test for T-0040, so it is not ignored.** On libsql 0.6.0
/// it crashed 13/20 on Windows; on 0.10.0, 0/40. A crash shows as this binary exiting `0xc0000005`,
/// which fails the run now that CI passes `--no-fail-fast` (the other binaries still
/// run) and Windows gates. Linux and macOS never faulted on the double close,
/// so there it only proves the teardown works.
#[test]
fn probe_i_batch_open_then_drop_all_on_one_os_thread() {
    let _turn = temp_dir_turn();
    multi_thread(8).block_on(async {
        let mut live = Vec::new();
        for _ in 0..WIDTH {
            live.push(open_one().await);
        }
        std::thread::spawn(move || drop(live))
            .join()
            .expect("teardown thread");
    });
}

/// 30 held open, then dropped sequentially inline on the async thread.
#[ignore = "reproduces a process abort (14-20/40) — run deliberately with --ignored"]
#[test]
fn probe_a_sequential_drops() {
    multi_thread(8).block_on(async {
        let mut live = Vec::new();
        for _ in 0..WIDTH {
            live.push(open_one().await);
        }
        for substrate in live {
            drop(substrate);
        }
    });
}

/// 30 held open, then all dropped concurrently — the shape the old diagnosis
/// believed was the cause. It crashes *less* than the sequential rows.
#[ignore = "reproduces a process abort (2-5/40) — run deliberately with --ignored"]
#[test]
fn thirty_simultaneous_teardowns() {
    multi_thread(8).block_on(async {
        let mut live = Vec::new();
        for _ in 0..WIDTH {
            live.push(open_one().await);
        }
        let mut handles = Vec::new();
        for substrate in live {
            handles.push(tokio::spawn(async move { drop(substrate) }));
        }
        for handle in handles {
            handle.await.expect("teardown task");
        }
    });
}

// ── controls · these are clean ────────────────────────────────────────────

/// 30 held open and **never dropped**. Clean — so teardown is required.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_c_never_dropped() {
    multi_thread(8).block_on(async {
        let mut live = Vec::new();
        for _ in 0..WIDTH {
            live.push(open_one().await);
        }
        std::mem::forget(live);
    });
}

/// Interleaved open/drop, inline on an 8-worker runtime. Same thread and same
/// runtime as the crashing rows; only co-liveness differs. Clean.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_j_interleaved_inline() {
    multi_thread(8).block_on(async {
        for _ in 0..WIDTH {
            drop(open_one().await);
        }
    });
}

/// Interleaved, `current_thread` runtime — no concurrency anywhere.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_e_single_threaded_one_at_a_time() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            for _ in 0..WIDTH {
                drop(open_one().await);
            }
        });
}

/// Interleaved, `multi_thread` runtime with a single worker.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_f_multithread_one_worker() {
    multi_thread(1).block_on(async {
        for _ in 0..WIDTH {
            drop(open_one().await);
        }
    });
}

/// Interleaved, each teardown on a blocking-pool thread.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_g_drop_on_blocking_thread() {
    multi_thread(8).block_on(async {
        for _ in 0..WIDTH {
            let substrate = open_one().await;
            tokio::task::spawn_blocking(move || drop(substrate))
                .await
                .expect("blocking teardown");
        }
    });
}

/// Interleaved, each teardown on its own fresh OS thread.
#[ignore = "part of the crash matrix; only meaningful beside the rows above"]
#[test]
fn probe_h_drop_on_dedicated_os_thread() {
    multi_thread(8).block_on(async {
        for _ in 0..WIDTH {
            let substrate = open_one().await;
            std::thread::spawn(move || drop(substrate))
                .join()
                .expect("teardown thread");
        }
    });
}

// ── the fix that IS in the tree ───────────────────────────────────────────

/// ✅ An ephemeral substrate deletes its directory on drop, because `Drop` now
/// closes every handle **before** deleting.
///
/// Not ignored: it asserts the fix, and its shape — one substrate live at a
/// time — is clean in every measurement above.
#[test]
fn an_ephemeral_substrate_deletes_its_directory_on_drop() {
    let _turn = temp_dir_turn();
    let before = leaked_dirs();
    multi_thread(2).block_on(async {
        for _ in 0..8 {
            drop(open_one().await);
        }
    });
    let after = leaked_dirs();
    assert!(
        after <= before,
        "dropping 8 ephemeral substrates leaked {} directories — `Drop` must close \
         the connections before deleting the directory, or Windows keeps the \
         database file locked and the delete silently fails",
        after.saturating_sub(before)
    );
}

/// Count `forge-*` directories in the temp dir.
fn leaked_dirs() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("forge-"))
        .count()
}
