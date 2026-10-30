//! `albedo test` — the parts that belong to the compiler: which files are
//! tests, and the realm a test file runs in.
//!
//! ## A test file is not part of the app
//!
//! `card.test.tsx` sits next to `card.tsx` and uses JSX, so every walker that
//! collects an app's modules would otherwise take it as a component: the route
//! table would grow `/guestbook.test`, the tier report would list it, and its
//! `albedo/test` import would be bundled as an npm package. [`is_test_module`] is
//! the one predicate the scanner, the module loader and route discovery share,
//! so the three cannot disagree about which files are tests.
//!
//! The server half — booting the app against a throwaway database and answering
//! a test's requests — lives in `albedo-server` (`test_runner`), because that is
//! where the server is.

pub mod realm;

use std::path::Path;

pub use realm::{TestCase, TestHost, TestOutcome, TestRealm};

/// The module a test file imports its API from.
pub const TEST_MODULE: &str = "albedo/test";

/// The infixes that mark a file as a test, as in `card.test.tsx`.
pub const TEST_INFIXES: [&str; 2] = [".test.", ".spec."];

/// Is this file a test — `*.test.{ts,tsx,js,jsx}` or `*.spec.{…}`?
///
/// Decided on the file name alone, so it reads the same in every walker and
/// costs no I/O. Case-insensitive, because Windows and macOS file systems are,
/// and a test that is a test on one machine must not become a route on another.
#[must_use]
pub fn is_test_module(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    TEST_INFIXES.iter().any(|infix| {
        lower.rfind(infix).is_some_and(|at| {
            matches!(&lower[at + infix.len()..], "ts" | "tsx" | "js" | "jsx")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_files_are_recognised_by_name() {
        for name in [
            "card.test.tsx",
            "card.test.ts",
            "card.spec.jsx",
            "lib/format.test.js",
            "Card.Test.TSX",
        ] {
            assert!(is_test_module(Path::new(name)), "{name} should be a test");
        }
        for name in [
            "card.tsx",
            "contest.tsx",
            "latest.ts",
            "test.tsx",
            "card.test.css",
            "card.test.d.ts",
            "testing/card.tsx",
        ] {
            assert!(!is_test_module(Path::new(name)), "{name} should not be a test");
        }
    }
}
