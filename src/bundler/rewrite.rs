use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RewriteAction {
    WrapModule {
        component_id: u64,
        source_module: String,
        wrapper_module: String,
    },
    LinkVendorChunk {
        component_id: u64,
        chunk_name: String,
    },
}

pub fn stable_wrapper_module_path(source_module: &str) -> String {
    let normalized = normalize_module_path(source_module);
    let hash = fnv1a_64_hex(normalized.as_bytes());
    let slug = normalized
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();

    format!("__albedo__/wrappers/{hash}_{slug}.mjs")
}

// `build_wrapper_module_source` and `build_wrapper_source_map` used to live
// here, alongside `wrapper_basename`, `stable_wrapper_source_map_url` and
// `escape_js_string` which existed only to serve them.
//
// They built the *contents* of `__albedo__/wrappers/*.mjs` and its sibling
// `.map`. Those files are no longer written — nothing ever loaded one, and each
// embedded the build machine's absolute source path into a shipped artifact.
// See the note in `bundler::emit::emit_bundle_artifacts_to_dir_internal`.
//
// `stable_wrapper_module_path` above stays: naming a component's module is how
// `RewriteAction::WrapModule` and the budget attribution in `budget/bundle.rs`
// group per-component bytes, and that never required a file to exist at the
// path. The Phase M.4 "Stage 2 source maps" deferral note went with the
// functions — it described per-line mappings for a trampoline the browser
// never received, so there is nothing left to defer.

fn normalize_module_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn fnv1a_64_hex(input: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_wrapper_module_path_is_deterministic() {
        let first = stable_wrapper_module_path("src/routes/home.tsx");
        let second = stable_wrapper_module_path("src/routes/home.tsx");
        assert_eq!(first, second);
        assert!(first.starts_with("__albedo__/wrappers/"));
    }

    /// The path is still a stable, machine-independent identity even though no
    /// file is written to it — backslash normalisation is what keeps a Windows
    /// build and a Linux build agreeing on the same component key.
    #[test]
    fn wrapper_module_path_is_separator_independent() {
        assert_eq!(
            stable_wrapper_module_path("src\\routes\\home.tsx"),
            stable_wrapper_module_path("src/routes/home.tsx")
        );
    }
}
