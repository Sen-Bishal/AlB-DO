//! The Node built-in module table — what a bare `fs`/`node:crypto` specifier
//! means when it reaches the bundler, and what to say about it.
//!
//! ## The defect this exists to remove
//!
//! Every Node built-in that reached the resolver produced *"npm package 'crypto'
//! not found in node_modules (searched upward from …)"*, which sends a reader
//! hunting for a dependency they never had. `TODO.md` item 9.5 names that
//! sentence specifically. The built-ins are not missing packages; they are a
//! host runtime Albedo does not provide, and the error should say so.
//!
//! ## 🔑 Resolve first, diagnose second
//!
//! A bare built-in *name* is not proof that a built-in was meant. `events`,
//! `buffer`, `stream`, `path`, `punycode`, `process`, `url` and `assert` are all
//! real, installable npm packages — the browserify shim layer — and a Tier-C
//! island that has one in `node_modules` is importing that package, which works
//! in a browser because it is ordinary JavaScript. So this table is consulted
//! **only after `node_modules` resolution has failed** (see
//! [`crate::bundler::npm`]'s `resolve_bare_entry`), never before.
//!
//! 🪤 That is the [`crate::runtime::react_host`] `react-is` lesson applied in
//! advance: *a refusal must name a capability the host genuinely lacks, not a
//! specifier that looks host-shaped.* Refusing bare `events` outright would turn
//! builds that work today into build errors, and the thing it would be
//! "protecting" the browser from is a polyfill.
//!
//! The one specifier form that carries no ambiguity is the **`node:` prefix**:
//! an npm package name cannot contain a colon, so `node:fs` is a built-in and
//! nothing else. Those skip the `node_modules` probe entirely.
//!
//! ## What this is, and what it is not
//!
//! For **client-reachable code this is a diagnosis, not an enforcement
//! boundary** — the browser is the boundary, and it has no `fs` to give. The
//! value is that the fact the compiler already holds arrives as a build error
//! naming the built-in, instead of as a blank island in a user's browser. Said
//! plainly here so nobody later cites this file as the thing that keeps client
//! code away from the filesystem; what does that is that client code runs in a
//! browser.
//!
//! For the **server** the same specifier is a policy statement, and the policy
//! is [`FLOOR.md`]'s: yes to Node at build time, yes to built-ins implemented
//! natively in our own runtime, **no to a Node host in the request path**.
//!
//! ## The two kinds, and why the split is honest
//!
//! [`BuiltinKind::HostOnly`] and [`BuiltinKind::Shimmable`] carry different
//! sentences because they have different futures. 🔑 **FLOOR's alignment claim
//! is exactly this split:** the built-ins that are easy (`util`, `events`,
//! `path`, `url`, `buffer`) grant no authority, and the ones that grant
//! authority (`fs`, `net`, `http`, `child_process`) are precisely the ones that
//! must not exist here. The npm gap and the capability model are not in tension,
//! and this table is where that stops being a claim in a document.
//!
//! ⇒ The work list for `TODO.md` 9.4 (the shim table) is *derived* from this
//! file — it is the [`BuiltinKind::Shimmable`] rows — rather than maintained
//! beside it.
//!
//! [`FLOOR.md`]: ../../development-plan/FLOOR.md

/// What a Node built-in would take to provide, which decides what to say about
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinKind {
    /// Names a capability only a Node host can provide — the filesystem,
    /// sockets, processes, the VM's own internals. There is no client-side
    /// implementation and there will not be one.
    HostOnly,
    /// Pure computation. Implementable in the runtime with no new authority,
    /// and on `TODO.md` 9.4's list. Not built yet.
    Shimmable,
}

/// One Node built-in module.
#[derive(Debug, Clone, Copy)]
pub struct NodeBuiltin {
    /// The module name without the `node:` prefix (`fs`, `child_process`).
    pub name: &'static str,
    /// What providing it would cost.
    pub kind: BuiltinKind,
    /// `true` when Node only accepts the `node:` form. `node:test` is a
    /// built-in; bare `test` is whatever is in `node_modules`, and matching it
    /// here would misdiagnose an ordinary missing package.
    pub prefix_only: bool,
}

impl NodeBuiltin {
    const fn host(name: &'static str) -> Self {
        Self {
            name,
            kind: BuiltinKind::HostOnly,
            prefix_only: false,
        }
    }

    const fn shimmable(name: &'static str) -> Self {
        Self {
            name,
            kind: BuiltinKind::Shimmable,
            prefix_only: false,
        }
    }

    const fn host_prefixed(name: &'static str) -> Self {
        Self {
            name,
            kind: BuiltinKind::HostOnly,
            prefix_only: true,
        }
    }

    /// The sentence a build error carries for this built-in.
    #[must_use]
    pub fn refusal(&self, specifier: &str) -> String {
        match self.kind {
            BuiltinKind::HostOnly => format!(
                "'{specifier}' is a Node built-in, not an npm package — it names a capability \
                 only a Node host provides, and Albedo does not give application code a Node \
                 host. There is nothing to install, and this will not be shimmed."
            ),
            BuiltinKind::Shimmable => format!(
                "'{specifier}' is a Node built-in, not an npm package — it is pure computation \
                 and could be provided, but Albedo has no Node built-in shims yet. There is \
                 nothing to install."
            ),
        }
    }
}

/// Every Node built-in module, by name.
///
/// Sourced from Node's own `module.builtinModules` (Node 22, stable surface),
/// minus the `_`-prefixed legacy internals which no published package imports.
/// Subpaths (`fs/promises`, `path/posix`, `stream/web`, `util/types`,
/// `timers/promises`, `dns/promises`, `assert/strict`, `test/reporters`) are
/// **not** listed: [`lookup`] keys on the first segment, so they resolve to
/// their parent's row and cannot disagree with it.
pub const NODE_BUILTINS: &[NodeBuiltin] = &[
    // ── Host capabilities. FLOOR.md's "no request path" set. ──────────────
    NodeBuiltin::host("async_hooks"),
    NodeBuiltin::host("child_process"),
    NodeBuiltin::host("cluster"),
    NodeBuiltin::host("dgram"),
    NodeBuiltin::host("dns"),
    NodeBuiltin::host("domain"),
    NodeBuiltin::host("fs"),
    NodeBuiltin::host("http"),
    NodeBuiltin::host("http2"),
    NodeBuiltin::host("https"),
    NodeBuiltin::host("inspector"),
    NodeBuiltin::host("module"),
    NodeBuiltin::host("net"),
    NodeBuiltin::host("os"),
    NodeBuiltin::host("process"),
    NodeBuiltin::host("readline"),
    NodeBuiltin::host("repl"),
    NodeBuiltin::host("tls"),
    NodeBuiltin::host("trace_events"),
    NodeBuiltin::host("tty"),
    NodeBuiltin::host("v8"),
    NodeBuiltin::host("vm"),
    NodeBuiltin::host("wasi"),
    NodeBuiltin::host("worker_threads"),
    // `node:`-only. Bare `test`/`sea`/`sqlite`/`quic` are ordinary npm names.
    NodeBuiltin::host_prefixed("quic"),
    NodeBuiltin::host_prefixed("sea"),
    NodeBuiltin::host_prefixed("sqlite"),
    NodeBuiltin::host_prefixed("test"),
    // ── Pure computation. TODO.md 9.4's work list is this half. ───────────
    NodeBuiltin::shimmable("assert"),
    NodeBuiltin::shimmable("buffer"),
    NodeBuiltin::shimmable("console"),
    NodeBuiltin::shimmable("constants"),
    NodeBuiltin::shimmable("crypto"),
    NodeBuiltin::shimmable("diagnostics_channel"),
    NodeBuiltin::shimmable("events"),
    NodeBuiltin::shimmable("path"),
    NodeBuiltin::shimmable("perf_hooks"),
    NodeBuiltin::shimmable("punycode"),
    NodeBuiltin::shimmable("querystring"),
    NodeBuiltin::shimmable("stream"),
    NodeBuiltin::shimmable("string_decoder"),
    NodeBuiltin::shimmable("sys"),
    NodeBuiltin::shimmable("timers"),
    NodeBuiltin::shimmable("url"),
    NodeBuiltin::shimmable("util"),
    NodeBuiltin::shimmable("zlib"),
];

/// `true` when the specifier is written in the unambiguous `node:` form.
///
/// An npm package name cannot contain a colon, so this needs no `node_modules`
/// probe: the resolver can refuse it on sight.
#[must_use]
pub fn is_prefixed_specifier(specifier: &str) -> bool {
    specifier.trim().starts_with("node:")
}

/// The built-in a specifier names, if any.
///
/// Keys on the first path segment after an optional `node:` prefix, so
/// `node:fs/promises` and `fs/promises` both land on the `fs` row. A bare name
/// marked [`NodeBuiltin::prefix_only`] is *not* matched — that form is an npm
/// package name, and reporting it as a built-in would misdiagnose an ordinary
/// missing dependency.
#[must_use]
pub fn lookup(specifier: &str) -> Option<&'static NodeBuiltin> {
    let raw = specifier.trim();
    let (bare, prefixed) = match raw.strip_prefix("node:") {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    let head = bare.split('/').next().unwrap_or(bare);
    NODE_BUILTINS
        .iter()
        .find(|builtin| builtin.name == head && (prefixed || !builtin.prefix_only))
}

/// The build-error sentence for a specifier that names a Node built-in.
///
/// `None` for anything else, which is the caller's signal to report whatever it
/// was going to report — a genuinely missing package, usually.
#[must_use]
pub fn refusal(specifier: &str) -> Option<String> {
    lookup(specifier).map(|builtin| builtin.refusal(specifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_names_each_builtin_once() {
        let mut seen = std::collections::BTreeSet::new();
        for builtin in NODE_BUILTINS {
            assert!(
                seen.insert(builtin.name),
                "'{}' is listed twice",
                builtin.name
            );
        }
    }

    #[test]
    fn floors_authority_set_is_host_only_and_9_4s_easy_set_is_not() {
        // 🔑 FLOOR.md's alignment claim, as a test: the built-ins that grant
        // authority are exactly the ones that must never exist here, and the
        // ones 9.4 schedules first grant none. If a future edit moves `fs` into
        // the shimmable half, this fails before the shim does.
        for name in ["fs", "net", "http", "child_process"] {
            assert_eq!(
                lookup(name).map(|builtin| builtin.kind),
                Some(BuiltinKind::HostOnly),
                "{name} grants authority and cannot be shimmable"
            );
        }
        for name in ["util", "events", "path", "url", "buffer", "crypto"] {
            assert_eq!(
                lookup(name).map(|builtin| builtin.kind),
                Some(BuiltinKind::Shimmable),
                "{name} is 9.4's work list and grants no authority"
            );
        }
    }

    #[test]
    fn a_subpath_resolves_to_its_parents_row() {
        assert_eq!(lookup("fs/promises").map(|b| b.name), Some("fs"));
        assert_eq!(lookup("node:fs/promises").map(|b| b.name), Some("fs"));
        assert_eq!(lookup("path/posix").map(|b| b.name), Some("path"));
        assert_eq!(lookup("util/types").map(|b| b.name), Some("util"));
        assert_eq!(lookup("stream/web").map(|b| b.name), Some("stream"));
    }

    #[test]
    fn a_prefix_only_builtin_is_not_claimed_in_its_bare_form() {
        // `node:test` is Node's test runner; `test` is whatever npm published
        // under that name. Claiming the bare form would tell someone with a
        // missing dependency that it was a built-in.
        assert!(lookup("node:test").is_some());
        assert!(lookup("test").is_none());
        assert!(lookup("sqlite").is_none());
        assert!(lookup("node:sqlite").is_some());
    }

    #[test]
    fn an_ordinary_package_is_not_a_builtin() {
        for specifier in ["lucide-react", "@radix-ui/react-slot", "date-fns", "clsx"] {
            assert!(lookup(specifier).is_none(), "{specifier} is not a built-in");
        }
        // 🪤 The browserify shim layer publishes real packages under built-in
        // names. This table must never be consulted before resolution, and the
        // lookup itself stays name-only — the resolve-first rule lives at the
        // call site in `bundler::npm`.
        assert!(lookup("events").is_some(), "the NAME is a built-in name");
    }

    #[test]
    fn the_two_kinds_say_different_things_about_the_future() {
        let host = refusal("node:child_process").expect("built-in");
        assert!(host.contains("will not be shimmed"));
        assert!(host.contains("node:child_process"), "name the specifier as written");

        let shimmable = refusal("path").expect("built-in");
        assert!(shimmable.contains("no Node built-in shims yet"));
        assert!(!shimmable.contains("will not be shimmed"));
    }

    #[test]
    fn only_the_prefixed_form_is_unambiguous() {
        assert!(is_prefixed_specifier("node:fs"));
        assert!(!is_prefixed_specifier("fs"));
        assert!(!is_prefixed_specifier("node-fetch"));
    }
}
