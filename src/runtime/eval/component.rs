use serde_json::Value;
use std::path::Path;

pub fn is_component_module(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("jsx" | "tsx" | "js" | "ts")
    )
}

pub fn fnv1a_hash(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub fn normalize_specifier(path: impl AsRef<Path>) -> String {
    let mut parts = Vec::new();
    for component in path.as_ref().components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            std::path::Component::Normal(segment) => {
                parts.push(segment.to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    normalize_slashes(&parts.join("/"))
}

pub fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

pub fn import_candidates(base: &str) -> Vec<String> {
    let mut out = Vec::new();
    if std::path::Path::new(base).extension().is_some() {
        out.push(base.to_string());
    } else {
        for ext in ["jsx", "tsx", "js", "ts"] {
            out.push(format!("{base}.{ext}"));
        }
        for ext in ["jsx", "tsx", "js", "ts"] {
            out.push(format!("{base}/index.{ext}"));
        }
    }
    out
}

pub fn normalize_jsx_text(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn is_component_tag(tag: &str) -> bool {
    tag.chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
}

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn escape_attr(value: &str) -> String {
    escape_html(value).replace('"', "&quot;")
}

pub fn render_attrs(attrs: &[(String, Value)]) -> String {
    let mut out = Vec::new();
    for (name, value) in attrs {
        if name.starts_with("on") {
            continue;
        }
        let attr_name = if name == "className" { "class" } else { name };
        match value {
            Value::Null => {}
            Value::Bool(false) => {}
            Value::Bool(true) => out.push(attr_name.to_string()),
            _ => {
                let text = value_to_string(value);
                if !text.is_empty() {
                    out.push(format!("{attr_name}=\"{}\"", escape_attr(&text)));
                }
            }
        }
    }
    out.join(" ")
}

pub fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

pub fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub fn classnames_collect(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::String(s) if !s.is_empty() => {
            out.push(s.clone());
        }
        Value::Array(arr) => {
            for item in arr {
                classnames_collect(item, out);
            }
        }
        Value::Object(map) => {
            for (key, flag) in map {
                if is_truthy(flag) {
                    out.push(key.clone());
                }
            }
        }
        _ => {}
    }
}

pub fn is_classnames_source(source: &str) -> bool {
    matches!(source, "classnames" | "clsx")
        || source.ends_with("/classnames")
        || source.ends_with("/clsx")
}

pub fn lit_to_value(lit: &swc_ecma_ast::Lit) -> Value {
    match lit {
        swc_ecma_ast::Lit::Str(str_lit) => Value::String(str_lit.value.to_string()),
        swc_ecma_ast::Lit::Bool(bool_lit) => Value::Bool(bool_lit.value),
        swc_ecma_ast::Lit::Num(num) => serde_json::Number::from_f64(num.value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        swc_ecma_ast::Lit::Null(_) => Value::Null,
        _ => Value::Null,
    }
}

pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        Value::Array(values) => values.iter().map(value_to_string).collect(),
        Value::Object(object) => serde_json::to_string(object).unwrap_or_default(),
    }
}

pub fn prop_name_to_string(name: &swc_ecma_ast::PropName) -> Option<String> {
    match name {
        swc_ecma_ast::PropName::Ident(ident) => Some(ident.sym.to_string()),
        swc_ecma_ast::PropName::Str(str_lit) => Some(str_lit.value.to_string()),
        swc_ecma_ast::PropName::Num(num) => Some(num.value.to_string()),
        _ => None,
    }
}
