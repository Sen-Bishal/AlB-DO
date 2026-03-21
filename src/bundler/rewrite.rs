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
        "import * as target from \"{escaped}\";\nconst resolved = target.default ?? target.render ?? target;\nexport default resolved;\nexport * from \"{escaped}\";\n"
    )
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
}
