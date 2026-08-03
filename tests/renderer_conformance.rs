//! The renderer conformance gate.
//!
//! Renders every component in the corpus through **both** renderers and fails
//! the build when they disagree. See [`dom_render_compiler::conformance`] for
//! the verdict taxonomy and why its distinctions are drawn where they are.
//!
//! ## Reading a failure
//!
//! * `DIVERGE` — both renderers produced markup and it differs. The output
//!   locates the first differing byte and windows both sides around it. This is
//!   a real bug in one of the two renderers; which one is a judgement call, and
//!   the evaluator is *usually* the one to trust, because it is the model the
//!   opcodes are emitted from.
//! * `EVALUATOR-FAULT` — the evaluator errored without naming a construct it
//!   does not model. That is a defect, not a coverage boundary; see
//!   `classify_evaluator_error`.
//!
//! ## Adding to the corpus
//!
//! Drop a fixture directory in, or add a module to the scaffold, and it is
//! picked up automatically. Adding a case is a commitment, in the same sense
//! `jsx_expr_eval_matrix.rs` means it.
//!
//! ## Quarantine
//!
//! [`QUARANTINE`] names cases that are known to diverge, each with a reason.
//! It is **bidirectional**: a quarantined case that starts *passing* fails the
//! gate too. A one-way quarantine rots — entries outlive the bug and nobody
//! learns the divergence is gone. This one cannot.

use dom_render_compiler::conformance::{
    CaseId, ConformanceReport, Contract, Verdict, compare_entry,
};
use dom_render_compiler::runtime::eval::CompiledProject;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Fixture groups whose subdirectories each hold one `Component.tsx`.
const FIXTURE_GROUPS: &[&str] = &["hook_compile", "jsx_matrix", "render_quickjs"];

/// Props for cases whose component reads them. A component rendered from `{}`
/// when it needs input is not evidence about the renderers — it is evidence
/// about the harness, which is why these are supplied rather than skipped.
fn props_for(corpus: &str, entry: &str) -> Value {
    match (corpus, entry) {
        ("render_quickjs/list", _) => json!({ "items": ["alpha", "beta"] }),
        ("scaffold", "routes/room/[id].tsx") => json!({ "params": { "id": "lobby" } }),
        _ => json!({}),
    }
}

/// Cases the harness cannot legitimately set up, each with the reason.
///
/// Distinct from [`QUARANTINE`]: these are not known divergences, they are
/// cases where a comparison would mean nothing. They are counted and printed
/// so the corpus's real reach is visible rather than assumed.
const NOT_COMPARABLE: &[(&str, &str, &str)] = &[
    (
        "render_quickjs/form_in_list",
        "Component.tsx",
        "metadata-only fixture: `rows` is a free variable that resolves nowhere, \
         by design — the fixture exists to be read by the extractor, not rendered",
    ),
    (
        "scaffold",
        "routes/guestbook.tsx",
        "reads a FORGE collection; a faithful comparison needs a seeded \
         BroadcastRegistry (render_entry_quickjs_with_broadcast), which this \
         corpus does not build yet",
    ),
    (
        "scaffold",
        "routes/room/[id].tsx",
        "same unseeded-collection setup as guestbook.tsx. Worth recording what \
         the attempt exposed, though: with the topic unseeded BOTH renderers \
         resolve it to `null`, and then they part company — QuickJS throws on \
         `null.map(...)`, which is what JS says it must do, while the evaluator \
         renders through it. That is the empty-collection case a first-run app \
         is always in. Which side is right is a FORGE empty-state decision, not \
         a renderer bug, so it is named here rather than quietly normalized \
         away. See development-plan/CONFORMANCE.md.",
    ),
];

/// Known divergences, each with a reason. See the module header: entries here
/// fail the gate if they start passing.
const QUARANTINE: &[(&str, &str, Contract, &str)] = &[
    (
        "render_quickjs/form_errors",
        "Component.tsx",
        Contract::Structural,
        "P6 form error-span PLACEMENT. The pure-Rust renderer interleaves each \
         field's `data-albedo-error` span after that field, using a render-time \
         scope stack; the QuickJS shim is bottom-up — children are stringified \
         before the enclosing form runs — so it appends the whole set at the \
         form's end. Same spans, same ids, same form; different position. \
         Acknowledged in `quickjs_engine.rs` where the shim appends them. Only \
         observable when an error message is actually visible.",
    ),
    (
        "render_quickjs/form_errors",
        "Component.tsx",
        Contract::Reactive,
        "Same as the structural case above — the placement difference is not \
         mode-dependent.",
    ),
];

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

struct Corpus {
    name: String,
    dir: PathBuf,
    entries: Vec<String>,
}

fn fixture_corpora() -> Vec<Corpus> {
    let root = repo_root().join("tests").join("fixtures");
    let mut out = Vec::new();
    for group in FIXTURE_GROUPS {
        let Ok(read) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        let mut cases: Vec<PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("Component.tsx").is_file())
            .collect();
        cases.sort();
        for case in cases {
            let leaf = case.file_name().unwrap().to_string_lossy().to_string();
            out.push(Corpus {
                name: format!("{group}/{leaf}"),
                dir: case,
                entries: vec!["Component.tsx".to_string()],
            });
        }
    }
    out
}

/// Every module the compiled project knows about, so a corpus widens by
/// someone adding a file rather than by someone remembering to list it.
fn whole_project_corpus(name: &str, dir: &Path) -> Option<Corpus> {
    let project = CompiledProject::load_from_dir(dir).ok()?;
    let mut entries: Vec<String> = project.project().modules().keys().cloned().collect();
    entries.sort();
    Some(Corpus {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        entries,
    })
}

/// The in-repo corpora — what the gate runs on every build.
fn corpora() -> Vec<Corpus> {
    let mut out = fixture_corpora();
    if let Some(scaffold) = whole_project_corpus("scaffold", &repo_root().join("scaffold/src")) {
        out.push(scaffold);
    }
    out
}

fn quarantine_reason(corpus: &str, entry: &str, contract: Contract) -> Option<&'static str> {
    QUARANTINE
        .iter()
        .find(|(c, e, k, _)| *c == corpus && *e == entry && *k == contract)
        .map(|(_, _, _, reason)| *reason)
}

fn not_comparable_reason(corpus: &str, entry: &str) -> Option<&'static str> {
    NOT_COMPARABLE
        .iter()
        .find(|(c, e, _)| *c == corpus && *e == entry)
        .map(|(_, _, reason)| *reason)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

struct Run {
    report: ConformanceReport,
    /// Quarantined cases that passed — the stale half of the bidirectional rule.
    stale_quarantine: Vec<String>,
    quarantined: Vec<String>,
}

fn run_corpora(corpora: &[Corpus]) -> Run {
    let mut run = Run {
        report: ConformanceReport::default(),
        stale_quarantine: Vec::new(),
        quarantined: Vec::new(),
    };

    for corpus in corpora {
        let project = match CompiledProject::load_from_dir(&corpus.dir) {
            Ok(project) => project,
            Err(err) => {
                for entry in &corpus.entries {
                    for contract in [Contract::Structural, Contract::Reactive] {
                        run.report.push(
                            CaseId {
                                corpus: corpus.name.clone(),
                                entry: entry.clone(),
                                contract,
                            },
                            Verdict::NotComparable {
                                reason: format!("corpus failed to compile: {err:#}"),
                            },
                        );
                    }
                }
                continue;
            }
        };

        for entry in &corpus.entries {
            let props = props_for(&corpus.name, entry);
            for contract in [Contract::Structural, Contract::Reactive] {
                let id = CaseId {
                    corpus: corpus.name.clone(),
                    entry: entry.clone(),
                    contract,
                };

                if let Some(reason) = not_comparable_reason(&corpus.name, entry) {
                    run.report.push(
                        id,
                        Verdict::NotComparable {
                            reason: reason.to_string(),
                        },
                    );
                    continue;
                }

                let verdict = compare_entry(&project, entry, &props, contract);

                match quarantine_reason(&corpus.name, entry, contract) {
                    Some(reason) if verdict.is_failure() => {
                        run.quarantined.push(format!("{id}\n      {reason}"));
                        run.report.push(
                            id,
                            Verdict::NotComparable {
                                reason: format!("quarantined: {reason}"),
                            },
                        );
                    }
                    Some(reason) => {
                        run.stale_quarantine.push(format!(
                            "{id}\n      now reports `{}`, but is still quarantined as:\n      {reason}",
                            verdict.label()
                        ));
                        run.report.push(id, verdict);
                    }
                    None => run.report.push(id, verdict),
                }
            }
        }
    }

    run
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// The gate. Both renderers, every component in the corpus, both contracts.
#[test]
fn the_two_renderers_agree_across_the_corpus() {
    let corpora = corpora();
    assert!(
        corpora.len() > 10,
        "the corpus collapsed to {} entries — discovery is broken, and a gate \
         that checks almost nothing passes quietly",
        corpora.len()
    );

    let run = run_corpora(&corpora);
    eprintln!("\n{}", run.report.summary());

    // Print every case that was not compared. A count alone lets these drift
    // upward unnoticed, and the corpus quietly stops covering what it claims to.
    let unread: Vec<&_> = run
        .report
        .outcomes
        .iter()
        .filter(|o| {
            matches!(
                o.verdict,
                Verdict::QuickJsFaulted { .. } | Verdict::NotComparable { .. }
            )
        })
        .collect();
    if !unread.is_empty() {
        eprintln!("NOT COMPARED ({})", unread.len());
        for outcome in unread {
            let reason = match &outcome.verdict {
                Verdict::QuickJsFaulted { reason } | Verdict::NotComparable { reason } => reason,
                _ => unreachable!(),
            };
            eprintln!("  [{}] {}\n      {reason}", outcome.verdict.label(), outcome.id);
        }
        eprintln!();
    }

    if !run.quarantined.is_empty() {
        eprintln!("QUARANTINED ({})", run.quarantined.len());
        for entry in &run.quarantined {
            eprintln!("  - {entry}\n");
        }
    }

    let failures = run.report.failures();
    let mut problems: Vec<String> = failures.iter().map(|f| f.describe()).collect();

    // A quarantine that outlived its bug is itself a failure: it is a claim
    // about the code that is no longer true, and left alone it will be used to
    // excuse the next real divergence at the same site.
    for stale in &run.stale_quarantine {
        problems.push(format!(
            "STALE QUARANTINE — this case passes now; delete its QUARANTINE entry.\n  {stale}"
        ));
    }

    assert!(
        problems.is_empty(),
        "renderer conformance: {} problem(s)\n\n{}\n",
        problems.len(),
        problems.join("\n\n")
    );
}

/// At least one case must reach byte-for-byte identity.
///
/// Without this the gate could be satisfied by normalizing everything into
/// agreement. `Identical` is the standard; `Equivalent` is the concession.
#[test]
fn the_corpus_contains_byte_identical_agreement() {
    let run = run_corpora(&corpora());
    let identical = run
        .report
        .outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Identical))
        .count();
    assert!(
        identical > 20,
        "only {identical} cases are byte-identical — if the corpus only ever \
         agrees *after* normalization, the normalizations have stopped being \
         concessions and become the contract"
    );
}

/// Cases whose opcode frame addresses an id the QuickJS markup lacks.
///
/// **Empty, and that is a result rather than an oversight.** The reactive
/// anchor wrappers do shift ids in hook-compile mode, and the obvious inference
/// — that a frame therefore names a wrapper the served markup has no node for —
/// is wrong. Conditional and list bindings do not travel in the opcode frame at
/// all; they travel in the `ReactivePayload`, and `build_reactive_payload`
/// ships the payload together with the pure-Rust HTML it was rendered from
/// (`renderer_runtime::build_reactive_blocks` fills the placeholder with
/// `payload.html`). Frame and markup are never crossed on that path.
///
/// The inference was written here as a quarantine entry first, and this test's
/// stale-quarantine half rejected it. Leaving the list empty and the check
/// running is the point: it costs nothing and it is the tripwire for the day
/// something does start crossing them.
const UNADDRESSABLE: &[(&str, &str, &str)] = &[];

/// Every id an opcode frame names must exist in the markup that will be served.
///
/// The other gate compares markup to markup. This one asks the question that
/// actually breaks pages: a Tier-B component's frame is emitted by the
/// evaluator at build time and its markup is rendered by QuickJS at request
/// time, so "the two renders look alike" is not the same claim as "the frame
/// can find its nodes". A single missing id costs the **whole frame** — every
/// handler on the page — and logs nothing.
#[test]
fn every_id_a_frame_names_exists_in_the_served_markup() {
    let mut broken: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for corpus in corpora() {
        let Ok(project) = CompiledProject::load_from_dir(&corpus.dir) else {
            continue;
        };
        for entry in &corpus.entries {
            if not_comparable_reason(&corpus.name, entry).is_some() {
                continue;
            }
            let props = props_for(&corpus.name, entry);
            let Ok(missing) =
                dom_render_compiler::conformance::frame_addressability(&project, entry, &props)
            else {
                continue;
            };
            checked += 1;

            let known = UNADDRESSABLE
                .iter()
                .find(|(c, e, _)| *c == corpus.name && *e == *entry)
                .map(|(_, _, reason)| *reason);

            match (missing.is_empty(), known) {
                (false, None) => broken.push(format!(
                    "{}::{entry}\n      frame names {missing:?}, which the QuickJS \
                     markup does not contain — this frame would be dropped whole",
                    corpus.name
                )),
                (true, Some(reason)) => stale.push(format!(
                    "{}::{entry}\n      addressable now; delete its UNADDRESSABLE \
                     entry, which claims:\n      {reason}",
                    corpus.name
                )),
                _ => {}
            }
        }
    }

    assert!(checked > 10, "only {checked} cases were checked");
    let mut problems = broken;
    problems.extend(stale);
    assert!(
        problems.is_empty(),
        "frame addressability: {} problem(s)\n\n{}\n",
        problems.len(),
        problems.join("\n\n")
    );
}

/// Point the harness at real applications outside the repo.
///
/// The in-repo corpus is the gate, and it has to stay hermetic — CI cannot
/// depend on a path on one laptop. But the in-repo corpus is also *written by
/// us for us*, and it shows: it produces almost no declines, because every
/// fixture in it was authored against the evaluator's abilities. The evaluator's
/// coverage frontier only has anything to say when it meets code that was not.
///
/// So the widening is opt-in and re-runnable, the same shape
/// `npm_coverage_probe.rs` settled on and for the same reason:
///
/// ```text
/// ALBEDO_CONFORMANCE_CORPUS="C:\Development\ALKMY\forge-lab\src;A:\halation\src" \
///   cargo test --test renderer_conformance -- --ignored --nocapture external
/// ```
///
/// It asserts **no threshold**, deliberately. A ratchet on a corpus you choose
/// yourself only teaches you to choose an easier corpus.
#[test]
#[ignore = "needs ALBEDO_CONFORMANCE_CORPUS; run explicitly"]
fn external_corpora_report_the_coverage_frontier() {
    let Ok(raw) = std::env::var("ALBEDO_CONFORMANCE_CORPUS") else {
        eprintln!(
            "ALBEDO_CONFORMANCE_CORPUS is unset — nothing to do.\n\
             Set it to one or more source roots, separated by ';'."
        );
        return;
    };

    let mut corpora = Vec::new();
    for raw_dir in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let dir = PathBuf::from(raw_dir);
        if !dir.is_dir() {
            eprintln!("  skipped (not a directory): {}", dir.display());
            continue;
        }
        let name = dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| raw_dir.to_string());
        match whole_project_corpus(&name, &dir) {
            Some(corpus) => corpora.push(corpus),
            None => eprintln!("  skipped (did not compile): {}", dir.display()),
        }
    }

    assert!(
        !corpora.is_empty(),
        "ALBEDO_CONFORMANCE_CORPUS was set but no entry yielded a compilable project"
    );

    let run = run_corpora(&corpora);
    eprintln!("\n{}", run.report.summary());

    for outcome in run.report.failures() {
        eprintln!("{}\n", outcome.describe());
    }
}

/// Every construct named in the decline vocabulary must still classify as a
/// decline.
///
/// `classify_evaluator_error` matches on message text, so a reworded error
/// would silently start classifying a coverage boundary as a defect — or, far
/// worse, a defect as a coverage boundary. This renders sources that provoke
/// each refusal and asserts the classification survives the round trip.
#[test]
fn each_declared_refusal_still_classifies_as_a_decline() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "for_loop",
            "statement: for",
            "export default function C(){ for (let i=0;i<1;i++) {} return <p>x</p>; }",
        ),
        (
            "try_catch",
            "statement: try/catch",
            "export default function C(){ try { } catch (e) { } return <p>x</p>; }",
        ),
        (
            "while_loop",
            "statement: while",
            "export default function C(){ while (false) {} return <p>x</p>; }",
        ),
        (
            "switch_stmt",
            "statement: switch",
            "export default function C(){ switch (1) { default: break; } return <p>x</p>; }",
        ),
    ];

    let tmp = std::env::temp_dir().join("albedo-conformance-declines");
    for (name, expected, source) in cases {
        let dir = tmp.join(name);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("Component.tsx"), source).expect("write fixture");

        let project = CompiledProject::load_from_dir(&dir).expect("fixture compiles");
        let verdict = compare_entry(
            &project,
            "Component.tsx",
            &json!({}),
            Contract::Structural,
        );

        match verdict {
            Verdict::EvaluatorDeclined { construct } => assert_eq!(
                &construct, expected,
                "`{name}` declined, but was bucketed as `{construct}` rather than \
                 `{expected}` — the coverage frontier's labels have drifted from \
                 the evaluator's messages"
            ),
            other => panic!(
                "`{name}` must be a DECLINE — the evaluator does not model this \
                 construct and says so. Got `{}`. If the evaluator learned this \
                 construct, that is good news and this case should move to the \
                 corpus; if the message was reworded, `classify_evaluator_error` \
                 is now mislabelling real defects.",
                other.label()
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
