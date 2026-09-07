//! **One rule for what a module path looks like inside an artifact.**
//!
//! A component's `file_path` is a host path while the compiler is running — it
//! has to be, the compiler opens it. The moment that string is *serialised* it
//! stops being a path and becomes an **identity**, and an identity carrying
//! `B:\beta-two\` is an identity that only resolves on the machine that built
//! it. `render-manifest.v2.json` and `precompiled-runtime-modules.json` both
//! key their components on it, so an artifact built here cannot be served
//! anywhere else — which is the whole of what blocks `albedo ship --binary`.
//!
//! 🪤 The rule was already written three times before this module existed:
//! `ManifestBuilder::component_entry_for_project` (resolve → strip root →
//! forward slashes), a bare `replace('\\', "/")` in the route-param check, and
//! the evaluator's own entry lookup. Three spellings of one fact is how the
//! fourth one comes to disagree, so this is the only one now and the others
//! call it.
//!
//! The shape is deliberately **project-relative with forward slashes**:
//! forward slashes because a manifest built on Windows is served on Linux and
//! `Path` on Linux does not treat `\` as a separator, and relative because the
//! artifact must not name a directory the serving box has never heard of.

use std::path::{Component as PathComponent, Path, PathBuf};

/// The canonical, machine-independent spelling of a component's module path.
///
/// Returns `None` when the path cannot be expressed relative to `root` —
/// which the caller must treat as a build failure rather than a fallback to
/// the absolute string. An artifact that silently keeps a host path is the
/// exact failure this module exists to prevent, and it is invisible until the
/// artifact is opened on another machine.
pub fn portable_module_path(file_path: &str, root: Option<&Path>) -> Option<String> {
    if file_path.is_empty() {
        return Some(String::new());
    }

    let normalized = file_path.replace('\\', "/");
    let path = Path::new(normalized.as_str());

    let relative = if path.is_absolute() {
        // `strip_prefix` is a component-wise comparison, so it is not fooled by
        // a trailing slash or a differing separator on either side — but it
        // *is* literal about `.` and `..`, hence the lexical tidy below.
        let root = root?;
        let root = normalize_lexically(root);
        match path.strip_prefix(root.as_path()) {
            Ok(stripped) => stripped.to_path_buf(),
            Err(_) => strip_prefix_ignoring_case(path, root.as_path())?,
        }
    } else {
        path.to_path_buf()
    };

    let cleaned = normalize_lexically(relative.as_path());
    if cleaned.components().any(|c| matches!(c, PathComponent::ParentDir)) {
        // A `..` that survives normalisation means the file genuinely sits
        // outside the project. There is no relative spelling that a serving
        // box could resolve, so this is a refusal, not a best effort.
        return None;
    }

    let text = cleaned.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        return None;
    }
    Some(text)
}

/// Resolve a canonical module path back to a real file under `root`.
///
/// The inverse of [`portable_module_path`], and the only supported way to turn
/// a manifest identity back into something openable. Callers that joined the
/// raw string onto a directory used to work by accident whenever the string
/// was absolute; that accident is what made artifacts unportable.
pub fn resolve_portable_module_path(module_path: &str, root: &Path) -> PathBuf {
    let normalized = module_path.replace('\\', "/");
    let path = Path::new(normalized.as_str());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Strip `root` from `path` comparing components case-insensitively.
///
/// 🪤 Only on Windows, and only as a second attempt after the exact strip
/// fails. `Path::strip_prefix` compares a `Prefix::Disk` case-insensitively
/// but ordinary directory components byte-wise, so a project root recorded as
/// `C:\Dev\App` against a walk that produced `C:\dev\app` — the same directory
/// as far as the filesystem is concerned — does not strip. Without this the
/// build refuses a perfectly valid Windows project.
///
/// On Unix this is a no-op by construction: `A.tsx` and `a.tsx` are two
/// different files there, and folding case would collapse two components into
/// one manifest identity, which is a corruption rather than a convenience.
#[cfg(windows)]
fn strip_prefix_ignoring_case(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut path_components = path.components();
    for root_component in root.components() {
        let candidate = path_components.next()?;
        let same = candidate == root_component
            || candidate
                .as_os_str()
                .to_str()
                .zip(root_component.as_os_str().to_str())
                .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right));
        if !same {
            return None;
        }
    }
    Some(path_components.as_path().to_path_buf())
}

#[cfg(not(windows))]
fn strip_prefix_ignoring_case(_path: &Path, _root: &Path) -> Option<PathBuf> {
    None
}

/// Drop `.` components and collapse `a/b/..` pairs without touching the disk.
///
/// `std::fs::canonicalize` would do more, but it requires the file to exist and
/// it produces a `\\?\` verbatim prefix on Windows that then fails to strip
/// against a plain root — a real failure mode, not a hypothetical one.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            PathComponent::CurDir => {}
            PathComponent::ParentDir => {
                // Only collapse against a real directory name; popping a root
                // or an existing `..` would change what the path means.
                let pops_a_name = out
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, PathComponent::Normal(_)));
                if pops_a_name {
                    out.pop();
                } else {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for: what the build actually wrote into
    /// `.albedo/dist` was `B:\beta-two\test-app/src/components\App.jsx` — a
    /// drive letter *and* both separators in one string.
    #[test]
    fn an_absolute_host_path_becomes_project_relative() {
        let root = PathBuf::from("B:/beta-two/test-app");
        assert_eq!(
            portable_module_path("B:\\beta-two\\test-app/src/components\\App.jsx", Some(&root)),
            Some("src/components/App.jsx".to_string())
        );
    }

    /// Forward slashes are not cosmetic: `Path` on Linux does not treat `\` as
    /// a separator, so a Windows-built manifest served on Linux would look up
    /// one file literally named `src\components\App.jsx`.
    #[test]
    fn every_separator_in_the_output_is_a_forward_slash() {
        let root = PathBuf::from("B:/app");
        let out = portable_module_path("B:\\app\\src\\deep\\Nested.tsx", Some(&root)).unwrap();
        assert!(!out.contains('\\'), "backslash survived: {out}");
        assert_eq!(out, "src/deep/Nested.tsx");
    }

    /// An already-relative path is already portable; it only needs its
    /// separators normalised, and must not be re-rooted.
    #[test]
    fn a_relative_path_is_left_relative() {
        let root = PathBuf::from("B:/app");
        assert_eq!(
            portable_module_path("src\\components\\Button.jsx", Some(&root)),
            Some("src/components/Button.jsx".to_string())
        );
    }

    /// `./` noise from a scanner walk must not reach the artifact, or two
    /// spellings of one component become two identities and the manifest
    /// stops joining to the precompiled modules.
    #[test]
    fn dot_segments_are_removed_so_one_file_has_one_identity() {
        let root = PathBuf::from("B:/app");
        assert_eq!(
            portable_module_path("B:/app/./src/components/../components/App.jsx", Some(&root)),
            Some("src/components/App.jsx".to_string())
        );
    }

    /// A file outside the project has no relative spelling a serving box could
    /// resolve. Refusing is the point: the alternative is an artifact that
    /// looks fine here and cannot be served anywhere else.
    #[test]
    fn a_path_outside_the_project_root_is_refused_rather_than_kept_absolute() {
        let root = PathBuf::from("B:/app");
        assert_eq!(
            portable_module_path("B:/elsewhere/Vendor.jsx", Some(&root)),
            None
        );
    }

    /// Without a root there is nothing to strip against, so an absolute path
    /// cannot be made portable — and must not be passed through.
    #[test]
    fn an_absolute_path_without_a_root_is_refused() {
        assert_eq!(portable_module_path("B:/app/src/App.jsx", None), None);
    }

    /// Round-tripping is the property the runtime depends on: whatever the
    /// build wrote, the server must be able to open under its own root.
    #[test]
    fn a_canonical_path_resolves_back_to_a_real_file_under_any_root() {
        let built_on = PathBuf::from("B:/beta-two/test-app");
        let canonical =
            portable_module_path("B:\\beta-two\\test-app\\src\\App.jsx", Some(&built_on)).unwrap();

        let served_on = PathBuf::from("/srv/app");
        assert_eq!(
            resolve_portable_module_path(&canonical, &served_on),
            PathBuf::from("/srv/app").join("src/App.jsx")
        );
    }

    /// 🪤 `Path::strip_prefix` is a **case-sensitive** component comparison on
    /// every platform, but Windows paths are case-insensitive. A root spelled
    /// `c:\dev\app` against a walk that yielded `C:\dev\app` therefore fails
    /// to strip and falls through to the absolute path — the silent failure
    /// this whole module exists to prevent, reintroduced by a drive letter.
    ///
    /// Deliberately Windows-only: on Linux `A.tsx` and `a.tsx` are two files,
    /// and folding case there would join two components into one identity.
    /// 🪤 Varying only the drive letter proves nothing — Rust compares a
    /// Windows `Prefix::Disk` case-insensitively already. The hole is in the
    /// **directory components**, which are compared byte-wise: a root recorded
    /// as `Dev\App` against a walk that yielded `dev\app` does not strip.
    #[cfg(windows)]
    #[test]
    fn a_root_differing_in_directory_case_still_strips_on_windows() {
        let root = PathBuf::from(r"C:\Dev\App");
        assert_eq!(
            portable_module_path(r"C:\dev\app\src\App.tsx", Some(&root)),
            Some("src/App.tsx".to_string())
        );
    }
}
