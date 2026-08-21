//! Boundaries the build enforces.
//!
//! ## Why this exists
//!
//! Two invariants in this tree were written down and then never checked, and both
//! decayed in the same way — silently, because nothing could observe them:
//!
//! * **`unsafe_code`.** `[workspace.lints.rust]` declared `unsafe_code = "forbid"`
//!   while no crate carried `[lints] workspace = true`, so the strongest lint in the
//!   manifest applied to nothing at all. The tree meanwhile contained real `unsafe`
//!   in the arena allocator and the evaluator. The lint is now `deny` and every
//!   crate opts in, which makes the exceptions *expressible* — this fence is what
//!   keeps them *few*.
//! * **`RenderManifestV2` is "the compiler's only output the server consumes."**
//!   `crates/albedo-server` in fact reaches into thirteen compiler modules. Three of
//!   them are compile-time modules. That is not automatically wrong — some of it is
//!   deliberate shared vocabulary, the "one spelling, in Rust" rule the renderer
//!   conformance work established — but the difference between a considered
//!   exception and an accident has to live somewhere a reviewer cannot skip.
//!
//! ## The shape, and where it comes from
//!
//! Borrowed from the scriptc tree, which fences its two-compiler-worlds invariant in
//! its linter: a hard rule plus a short, named allowlist of the files permitted to
//! cross it, so an accidental crossing is a build error and a deliberate one is a
//! diff to a list someone reviews. The list is the artifact; the rule alone would
//! either be false or be turned off.
//!
//! A fence is only worth having if it fails. Each one below names what a violation
//! means, so the failure message tells you whether to fix the code or change the
//! fence — deliberately, on the record.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively. Build artifacts and vendored trees are
/// skipped: this walks *our* source, and `target/` alone would otherwise dwarf it.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["target", "node_modules", ".git", "graphify-out", "_claude-migration"];
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIP.contains(&name.as_ref()) {
                out.extend(rust_files(&path));
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
    out
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Repo-relative, forward-slashed — so the expectations below read the same on
/// Windows (where this project is developed) and on CI's Linux runners.
fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── Fence 1 · the audited `unsafe` exception list ────────────────────────────
//
// The compiler enforces one half: with `unsafe_code = "deny"` applied through
// `[lints] workspace = true`, no `unsafe` compiles without an explicit `allow`.
// This test enforces the other half — that no `allow` appears without review.
// Together they mean the list below IS the complete inventory of unsafe in this
// workspace, which is the property the manifest claimed and could not deliver.

/// Files permitted to relax `unsafe_code`, each with the reason it must state at
/// its own site. Adding an entry here is the review step; there is no other way to
/// introduce `unsafe` into this workspace.
const UNSAFE_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "src/runtime/arena.rs",
        "this module IS the allocator handed to QuickJS as a JSMallocFunctions table",
    ),
    (
        "src/runtime/eval/core.rs",
        "the render walk re-enters itself through QuickJS, so the active CompiledProject \
         travels as a thread-local raw pointer rather than a borrow",
    ),
    (
        "tests/adversarial_input.rs",
        "GlobalAlloc is an unsafe trait; the counting allocator is this suite's instrument \
         and governs the test binary only",
    ),
];

#[test]
fn unsafe_code_exceptions_are_exactly_the_audited_list() {
    let root = repo_root();
    let mut found: Vec<String> = Vec::new();
    for path in rust_files(&root) {
        let relative = rel(&path, &root);
        // This file names the exceptions in order to check them; counting its own
        // list as a violation would make the fence unwritable.
        if relative == "tests/module_fences.rs" {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if text.contains("allow(unsafe_code)") {
            found.push(relative);
        }
    }
    found.sort();

    let mut expected: Vec<String> = UNSAFE_EXCEPTIONS.iter().map(|(f, _)| (*f).to_owned()).collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "\nthe audited `unsafe` exception list drifted.\n\
         A file gained or lost `#![allow(unsafe_code)]`.\n\
         If new `unsafe` is genuinely necessary: state the reason at the site, then add the \
         file to UNSAFE_EXCEPTIONS in this test with that same reason.\n\
         If it is not necessary — and it usually is not outside an allocator or an FFI \
         boundary — write it in safe Rust instead.\n"
    );
}

#[test]
fn every_unsafe_exception_states_its_reason_at_the_site() {
    let root = repo_root();
    for (file, _) in UNSAFE_EXCEPTIONS {
        let text = fs::read_to_string(root.join(file))
            .unwrap_or_else(|e| panic!("audited exception {file} is unreadable: {e}"));
        assert!(
            text.contains("AUDITED EXCEPTION"),
            "{file} relaxes `unsafe_code` without the `AUDITED EXCEPTION` note explaining why. \
             The allowlist entry is the review; the note at the site is what the next reader gets."
        );
    }
}

// ── Fence 2 · what the server may reach into ─────────────────────────────────
//
// `crates/albedo-server` depends on `dom-render-compiler`, so Cargo permits it the
// whole public surface. The intended contract is narrower: the server consumes the
// compiler's OUTPUT (the manifest, the IR, the runtime) plus a small, named set of
// shared vocabulary. It does not run the compiler — the `albedo` binary does, and
// that binary is deliberately outside this fence.
//
// The distinction that matters: `parser`, `scanner`, `analysis`, `incremental`,
// `graph`, `budget`, `doctor`, and `dev` are compile-time machinery. A request path
// that reaches one of them has moved compilation into the request, which is the one
// architectural mistake this whole system is organized against.

/// Compiler modules `crates/albedo-server/src/**` may name, and why.
const SERVER_MAY_REACH: &[(&str, &str)] = &[
    ("ir", "the opcode wire and action envelopes — the interface itself"),
    ("manifest", "RenderManifestV2, the compiler's declared output"),
    ("runtime", "the evaluator and engine the server drives per request"),
    ("forge", "the data substrate; the server is its only caller in production"),
    ("auth", "session and principal types shared with the request path"),
    ("aperture", "the outbound HTTP client the server stages suspensions through"),
    ("shutter", "the streaming/suspension machinery"),
    ("hydration", "tier and hydration types travelling on the manifest"),
    ("types", "shared identifier newtypes"),
    ("dev_contract", "the dev-server contract type, shared by construction"),
    // Compile-time modules, admitted only as SHARED VOCABULARY — the same "one
    // spelling, in Rust" rule that removed the JS shim's private copies of markup
    // rules. Each is a constant or an id-allocation function that both sides must
    // compute identically; a second spelling is a silent divergence, not a
    // refactor. These are the entries to watch: if one starts pulling behaviour
    // rather than vocabulary, it belongs in a shared module instead of here.
    ("transforms", "form vocabulary only — field names, CSRF placeholders, id allocation"),
    ("bundler", "the client asset surface the server serves (emit, client_npm)"),
    ("graph", "test-only, inside #[cfg(test)] modules in handlers/streaming.rs"),
];

#[test]
fn the_server_reaches_only_into_declared_compiler_modules() {
    let root = repo_root();
    let server_src = root.join("crates/albedo-server/src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files(&server_src) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (line_no, line) in text.lines().enumerate() {
            let mut rest = line;
            while let Some(idx) = rest.find("dom_render_compiler::") {
                rest = &rest[idx + "dom_render_compiler::".len()..];
                let module: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if module.is_empty() {
                    continue;
                }
                if !SERVER_MAY_REACH.iter().any(|(m, _)| *m == module) {
                    violations.push(format!(
                        "{}:{} reaches dom_render_compiler::{module}",
                        rel(&path, &root),
                        line_no + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "\nthe server reached into a compiler module outside the declared set:\n  {}\n\n\
         The server consumes the compiler's output; it does not run the compiler. If the \
         reach is genuine shared vocabulary (a constant or an id both sides must compute \
         identically), add the module to SERVER_MAY_REACH with that justification. If it is \
         behaviour, it belongs behind the manifest — or in a module both sides can share \
         without the request path importing compile-time machinery.\n",
        violations.join("\n  ")
    );
}

#[test]
fn the_compiler_never_names_the_server() {
    let root = repo_root();
    let mut violations: Vec<String> = Vec::new();
    for path in rust_files(&root.join("src")) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        // `src/bin/albedo.rs` compiles under `albedo-server` (a cross-crate `[[bin]]`
        // path entry), so it is the server's binary living in the compiler's tree —
        // the one file here allowed to name it.
        if rel(&path, &root) == "src/bin/albedo.rs" {
            continue;
        }
        // Comments are exempt on purpose. The compiler documents its counterparts by
        // name — `transforms::form`'s constants say which `albedo_server` module
        // re-exports them — and those cross-references are the shared-vocabulary rule
        // being honest about itself. What must not exist is a code path.
        for (line_no, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with('*') {
                continue;
            }
            if code.contains("albedo_server::") {
                violations.push(format!("{}:{}", rel(&path, &root), line_no + 1));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "\nthe compiler crate named `albedo_server`: {violations:?}\n\
         The dependency runs one way. A cycle here would make the manifest boundary \
         unenforceable in both directions at once.\n"
    );
}
