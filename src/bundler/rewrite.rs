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

pub fn build_wrapper_module_source(source_module: &str) -> String {
    let normalized = normalize_module_path(source_module);
    let escaped = escape_js_string(&normalized);

    format!(
        "import * as target from \"{escaped}\";\nconst resolved = target.default ?? target.render ?? target;\nexport default resolved;\nexport * from \"{escaped}\";\n//# sourceMappingURL={map_url}\n",
        map_url = stable_wrapper_source_map_url(source_module)
    )
}

/// Phase M.4 — emit-time wrapper JS source map. Stage 1 produces a
/// minimal but v3-spec-compliant map that points browser DevTools at
/// the original `.tsx` file without claiming per-line mappings. Real
/// mappings (Stage 2) require wiring SWC's mapping collector through
/// the transpile path; until then, this stub lets DevTools surface
/// the original source name in the call stack and "open in editor"
/// flows, which is the practically useful 80%.
pub fn build_wrapper_source_map(source_module: &str) -> String {
    let normalized = normalize_module_path(source_module);
    // Manual JSON write so we don't need to bring in `serde_json`
    // for a 4-field struct. Both fields are server-controlled
    // identifiers; escaping is single-character only.
    let escaped = escape_js_string(&normalized);
    format!(
        "{{\"version\":3,\"file\":\"{wrapper_basename}\",\"sources\":[\"{escaped}\"],\"sourcesContent\":[null],\"names\":[],\"mappings\":\"\"}}",
        wrapper_basename = wrapper_basename(source_module)
    )
}

/// Filename portion of the wrapper module, used as the `file` field
/// inside the source map.
fn wrapper_basename(source_module: &str) -> String {
    let path = stable_wrapper_module_path(source_module);
    path.rsplit('/').next().unwrap_or(&path).to_string()
}

/// `sourceMappingURL` value the wrapper JS emits. Relative path so
/// the map file lives next to the JS in the bundle output tree.
fn stable_wrapper_source_map_url(source_module: &str) -> String {
    format!("{}.map", wrapper_basename(source_module))
}

fn normalize_module_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn escape_js_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

    #[test]
    fn test_build_wrapper_module_source_contains_exports() {
        let source = build_wrapper_module_source("src/routes/home.tsx");
        assert!(source.contains("import * as target"));
        assert!(source.contains("export default resolved;"));
        assert!(source.contains("export * from"));
    }

    #[test]
    fn test_build_wrapper_module_source_carries_sourcemap_url() {
        // Phase M.4 · every emitted wrapper points at a peer .map
        // file for browser DevTools.
        let source = build_wrapper_module_source("src/routes/home.tsx");
        assert!(source.contains("//# sourceMappingURL="));
        assert!(source.contains(".mjs.map"));
    }

    #[test]
    fn test_build_wrapper_source_map_is_v3_compliant_stub() {
        // Stage 1 stub: valid v3 JSON pointing browser DevTools at
        // the original .tsx; empty mappings until Stage 2 wires SWC.
        let map = build_wrapper_source_map("src/routes/home.tsx");
        assert!(map.contains("\"version\":3"));
        assert!(map.contains("\"sources\":[\"src/routes/home.tsx\"]"));
        assert!(map.contains("\"mappings\":\"\""));
    }

    #[test]
    fn test_build_wrapper_source_map_normalises_backslashes() {
        let map = build_wrapper_source_map("src\\routes\\home.tsx");
        assert!(map.contains("\"sources\":[\"src/routes/home.tsx\"]"));
    }
}
