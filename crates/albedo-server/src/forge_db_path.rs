//! Where `forge.db` lives.
//!
//! The database was opened as the bare relative name `forge.db`, which resolves
//! against the **process working directory**. That is fine exactly once — when
//! someone runs `albedo serve` from inside their project — and wrong every
//! other time it matters:
//!
//! * `albedo serve <dir>` from anywhere else opens (or *creates*) a second,
//!   empty database beside the shell, and the app comes up with no data;
//! * a container started with a different `WORKDIR` writes into the image's
//!   own filesystem, so **every restart loses every row** unless the volume
//!   happens to be mounted at exactly the right place;
//! * two apps served from one shell share one file.
//!
//! 🔑 The failure is *silent in the worst direction*: `open_local` **creates**
//! a missing database, so pointing at the wrong place looks like a healthy boot
//! serving an empty app, not like an error. That is why the default is derived
//! from the artifacts the server is already reading rather than from wherever
//! the operator's shell happened to be.

use std::path::{Path, PathBuf};

/// The file name used when nothing else determines it.
pub const DEFAULT_FORGE_DB_FILENAME: &str = "forge.db";

/// The environment variable a container sets to point at its mounted volume.
pub const FORGE_DB_ENV: &str = "ALBEDO_FORGE_DB";

/// Decide the database path from configuration, environment and artifact layout.
///
/// Precedence, highest first:
///
/// 1. `configured` — an explicit `forge.db_path` in the config file. The
///    operator said it; nothing should second-guess it.
/// 2. `ALBEDO_FORGE_DB` — the container seam. An env var is what a Dockerfile,
///    a systemd unit or a PaaS can set without rewriting the app's config.
/// 3. `<project>/forge.db`, where the project is derived from `artifacts_dir`.
///    This is the case that used to be CWD-relative.
///
/// A relative path from (1) or (2) is resolved against the project root too,
/// not the CWD — the same reason: the shell's location is not a property of
/// the deployment.
#[must_use]
pub fn resolve_forge_db_path(
    configured: Option<&str>,
    env: Option<&str>,
    artifacts_dir: &Path,
) -> PathBuf {
    let root = project_root_for_artifacts(artifacts_dir);

    let chosen = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| env.map(str::trim).filter(|value| !value.is_empty()));

    match chosen {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        }
        None => root.join(DEFAULT_FORGE_DB_FILENAME),
    }
}

/// Read [`FORGE_DB_ENV`] from the real environment.
///
/// Split out so [`resolve_forge_db_path`] stays pure and testable — reading the
/// process environment inside it would make the precedence rules untestable
/// without mutating global state, which is exactly the kind of test that
/// passes alone and fails in a suite.
#[must_use]
pub fn forge_db_env() -> Option<String> {
    std::env::var(FORGE_DB_ENV).ok()
}

/// The project directory a `<project>/.albedo/dist` artifacts directory belongs to.
///
/// Two components up, because the build fixes that layout. A shallower path
/// falls back to the artifacts directory itself rather than panicking on
/// `parent()`.
fn project_root_for_artifacts(artifacts_dir: &Path) -> PathBuf {
    artifacts_dir
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| artifacts_dir.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifacts() -> PathBuf {
        PathBuf::from("/srv/app/.albedo/dist")
    }

    /// The defect: the bare name resolved against the shell, so serving a
    /// project from outside it opened an empty database beside the shell and
    /// booted cleanly with no data.
    #[test]
    fn the_default_sits_beside_the_project_not_the_shell() {
        assert_eq!(
            resolve_forge_db_path(None, None, &artifacts()),
            PathBuf::from("/srv/app").join("forge.db")
        );
    }

    /// The container seam: a mounted volume is named by an absolute path and
    /// must be used exactly as given.
    #[test]
    fn an_absolute_env_path_points_at_a_mounted_volume_verbatim() {
        assert_eq!(
            resolve_forge_db_path(None, Some("/data/forge.db"), &artifacts()),
            PathBuf::from("/data/forge.db")
        );
    }

    /// Config beats environment: the file is the more specific statement, and
    /// an inherited env var should never quietly redirect a declared path.
    #[test]
    fn an_explicit_config_path_outranks_the_environment() {
        assert_eq!(
            resolve_forge_db_path(Some("/data/from-config.db"), Some("/data/from-env.db"), &artifacts()),
            PathBuf::from("/data/from-config.db")
        );
    }

    /// A relative override is still not CWD-relative — that is the whole bug.
    #[test]
    fn a_relative_override_resolves_against_the_project_not_the_cwd() {
        assert_eq!(
            resolve_forge_db_path(Some("data/forge.db"), None, &artifacts()),
            PathBuf::from("/srv/app").join("data/forge.db")
        );
    }

    /// An empty or whitespace-only value is a value nobody meant to set —
    /// an unset env var in a shell script expands to exactly this, and taking
    /// it literally would open a database named "".
    #[test]
    fn a_blank_override_falls_through_instead_of_naming_an_empty_file() {
        assert_eq!(
            resolve_forge_db_path(Some("   "), Some(""), &artifacts()),
            PathBuf::from("/srv/app").join("forge.db")
        );
    }
}
