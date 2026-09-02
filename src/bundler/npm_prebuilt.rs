//! Server npm bundles, written at build time and read at boot.
//!
//! # The defect this closes
//!
//! `CompiledProject::wrap` bundles every bare specifier out of `node_modules`
//! **in memory, every time it runs** — and boot runs it. Nothing was ever
//! written to disk, so `node_modules` had to be present wherever the app was
//! served, not merely wherever it was built.
//!
//! 📏 Measured 2026-09-02 on a five-dependency app (`date-fns`, `zod`, `clsx`,
//! `nanoid`, `slugify`): **58 MB / 6 526 files of `node_modules`** against
//! **409 KB of `.albedo/dist`**. A real dependency tree is several hundred
//! megabytes, and all of it shipped into every container image to re-derive
//! something the build already computed.
//!
//! This is the same shape as the other defects found that day — a fact the
//! compiler holds with no consumer — except the missing consumer is the
//! deployment story.
//!
//! # What is written
//!
//! The lowered, QuickJS-loadable artifacts themselves: per-file factory
//! registration scripts plus the alias artifact, exactly as
//! [`crate::bundler::npm::bundle_npm_dependency`] produced them. Nothing here
//! needs `node_modules` to interpret, which is the whole point.
//!
//! # Staleness
//!
//! 🔑 **The specifier set is the identity.** A prebuilt file is only usable if
//! it bundles precisely the specifiers this source tree asks for — no more, no
//! fewer. An app that gained an import since the build must not be served from
//! a file that predates it, and one that dropped an import should not carry the
//! dead package into its process. The check is therefore an exact set
//! comparison rather than a timestamp: timestamps say *when*, and the question
//! is *what*.
//!
//! ⚠️ It deliberately does **not** hash the artifact scripts. Their content is a
//! function of `node_modules`, which by design is absent at the moment this
//! matters; a hash we cannot recompute is a check we cannot run.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bundler::npm::NpmDependencyBundle;

/// Filename inside the build output directory.
pub const NPM_SERVER_BUNDLES_FILENAME: &str = "npm-server-bundles.json";

/// Schema version of the file. Bumped when the artifact shape changes; a file
/// written by a different version is ignored rather than misread.
pub const NPM_SERVER_BUNDLES_VERSION: &str = "1.0";

/// The on-disk form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrebuiltNpmBundles {
    /// [`NPM_SERVER_BUNDLES_VERSION`].
    pub version: String,
    /// Which engine the artifacts were lowered for. `quickjs` today; a file
    /// written for another engine is not loadable here.
    pub engine: String,
    /// The bundles, one per bare specifier, in deterministic order.
    pub bundles: Vec<NpmDependencyBundle>,
}

impl PrebuiltNpmBundles {
    /// Wrap freshly built bundles for writing.
    #[must_use]
    pub fn new(mut bundles: Vec<NpmDependencyBundle>) -> Self {
        bundles.sort_by(|left, right| left.specifier.cmp(&right.specifier));
        Self {
            version: NPM_SERVER_BUNDLES_VERSION.to_string(),
            engine: "quickjs".to_string(),
            bundles,
        }
    }

    /// The specifiers this file covers, sorted.
    #[must_use]
    pub fn specifiers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .bundles
            .iter()
            .map(|bundle| bundle.specifier.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Serialize for [`NPM_SERVER_BUNDLES_FILENAME`].
    ///
    /// # Errors
    /// If the bundles cannot be encoded as JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Read the file from a build output directory.
    ///
    /// Returns `None` when it is absent, unreadable, malformed, or written by a
    /// version or engine this build does not understand. **Every one of those is
    /// a fall-back to re-bundling, never an error**: the prebuilt file is an
    /// optimisation, and a build that can reach `node_modules` can always
    /// produce the same thing again. The one case that must not be silent is
    /// *usable file, wrong contents*, which [`Self::covers`] answers separately.
    #[must_use]
    pub fn load(dist_dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(dist_dir.join(NPM_SERVER_BUNDLES_FILENAME)).ok()?;
        let parsed: Self = serde_json::from_str(&raw).ok()?;
        (parsed.version == NPM_SERVER_BUNDLES_VERSION && parsed.engine == "quickjs")
            .then_some(parsed)
    }

    /// Whether this file bundles exactly `wanted` — the specifiers the source
    /// tree asks for right now.
    ///
    /// 🔑 Exact, in both directions. A missing specifier means a component
    /// would render nothing; an extra one means the process carries a package
    /// the app no longer imports. Neither is something to serve through.
    #[must_use]
    pub fn covers(&self, wanted: &[String]) -> bool {
        let mut wanted: Vec<String> = wanted.to_vec();
        wanted.sort();
        wanted.dedup();
        self.specifiers() == wanted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundler::npm::{NpmArtifact, NpmDependencyBundle};

    fn bundle(specifier: &str) -> NpmDependencyBundle {
        NpmDependencyBundle {
            specifier: specifier.to_string(),
            package_name: specifier.to_string(),
            package_version: "1.0.0".to_string(),
            entry_key: format!("npm:{specifier}@1.0.0/index.js"),
            artifacts: vec![NpmArtifact {
                key: format!("npm:{specifier}@1.0.0/index.js"),
                script: "globalThis.__albedo_define_record('x', function(){});".to_string(),
                source_hash: 42,
            }],
        }
    }

    /// The artifacts must survive the round trip intact — they are the thing
    /// that replaces `node_modules`, so a lossy encoding is a broken runtime.
    #[test]
    fn bundles_round_trip_through_the_file_form() {
        let written = PrebuiltNpmBundles::new(vec![bundle("zod"), bundle("clsx")]);
        let json = written.to_json().expect("serializes");
        let read: PrebuiltNpmBundles = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(read.specifiers(), vec!["clsx".to_string(), "zod".to_string()]);
        let zod = read
            .bundles
            .iter()
            .find(|b| b.specifier == "zod")
            .expect("zod survives");
        assert_eq!(zod.artifacts.len(), 1);
        assert_eq!(zod.artifacts[0].source_hash, 42);
        assert!(zod.artifacts[0].script.contains("__albedo_define_record"));
    }

    /// Deterministic order, so the file does not churn between builds that
    /// bundled the same thing.
    #[test]
    fn the_written_order_is_deterministic() {
        let one = PrebuiltNpmBundles::new(vec![bundle("zod"), bundle("clsx")]);
        let two = PrebuiltNpmBundles::new(vec![bundle("clsx"), bundle("zod")]);
        assert_eq!(one.to_json().unwrap(), two.to_json().unwrap());
    }

    #[test]
    fn coverage_is_an_exact_set_match_in_both_directions() {
        let file = PrebuiltNpmBundles::new(vec![bundle("zod"), bundle("clsx")]);

        assert!(file.covers(&["clsx".to_string(), "zod".to_string()]));
        // Order and duplicates in the request are immaterial.
        assert!(file.covers(&["zod".to_string(), "clsx".to_string(), "zod".to_string()]));

        // 🔑 A specifier the app gained since the build: serving through this
        // renders the component using it as nothing.
        assert!(!file.covers(&["clsx".to_string(), "zod".to_string(), "nanoid".to_string()]));
        // A specifier the app dropped: the process would carry a dead package.
        assert!(!file.covers(&["zod".to_string()]));
        assert!(!file.covers(&[]));
    }

    #[test]
    fn an_absent_or_unreadable_file_is_a_fallback_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(PrebuiltNpmBundles::load(dir.path()).is_none());

        std::fs::write(dir.path().join(NPM_SERVER_BUNDLES_FILENAME), "{ not json")
            .expect("write garbage");
        assert!(PrebuiltNpmBundles::load(dir.path()).is_none());
    }

    /// A file from a future schema, or lowered for another engine, is ignored
    /// rather than half-understood.
    #[test]
    fn a_foreign_version_or_engine_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NPM_SERVER_BUNDLES_FILENAME);

        let mut future = PrebuiltNpmBundles::new(vec![bundle("zod")]);
        future.version = "2.0".to_string();
        std::fs::write(&path, future.to_json().unwrap()).expect("write");
        assert!(PrebuiltNpmBundles::load(dir.path()).is_none());

        let mut other_engine = PrebuiltNpmBundles::new(vec![bundle("zod")]);
        other_engine.engine = "v8".to_string();
        std::fs::write(&path, other_engine.to_json().unwrap()).expect("write");
        assert!(PrebuiltNpmBundles::load(dir.path()).is_none());
    }

    #[test]
    fn a_well_formed_file_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = PrebuiltNpmBundles::new(vec![bundle("zod")]);
        std::fs::write(
            dir.path().join(NPM_SERVER_BUNDLES_FILENAME),
            written.to_json().unwrap(),
        )
        .expect("write");

        let read = PrebuiltNpmBundles::load(dir.path()).expect("loads");
        assert!(read.covers(&["zod".to_string()]));
    }
}
