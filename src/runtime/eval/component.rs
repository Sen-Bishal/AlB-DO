use serde_json::Value;
use std::path::Path;

pub fn is_component_module(path: &Path) -> bool {
    // Phase P · post-P wire-through — skip ambient TypeScript
    // declaration files (`*.d.ts`, `*.d.tsx`). They carry
    // `declare function` shapes with no body that the SWC parse
    // path would reject as "missing function body", and they're
    // declaration-only anyway — no runtime content for the
    // renderer to walk.
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".d.ts") || name.ends_with(".d.tsx") {
        return false;
    }
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

/// FNV-1a-32: matches `stable_id_for_placeholder` in albedo-server's
/// `render::tier_b`. The compiler crate can't depend on the server crate,
/// so this is the source of truth for shell-stamped `data-albedo-id`s
/// emitted by the static evaluator. Both functions must produce the same
/// bytes for the same input — anchor IDs cross the WT boundary as u32s.
pub fn fnv1a_32(data: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for byte in data {
        hash ^= u32::from(*byte);
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
    // React JSX whitespace rules (paraphrased):
    //   * Pure-whitespace text becomes nothing.
    //   * If the text contains a newline, it represents source formatting:
    //     adjacent whitespace fully collapses (`\n  x \n` → `x`).
    //   * Without a newline, runs of whitespace collapse to a single space
    //     and leading/trailing whitespace is preserved as one space — this
    //     is what keeps `{n} items` from becoming `3items`.
    let inner = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if inner.is_empty() {
        return None;
    }
    if value.contains('\n') {
        return Some(inner);
    }
    let mut result = String::new();
    if value.starts_with(|c: char| c.is_whitespace()) {
        result.push(' ');
    }
    result.push_str(&inner);
    if value.ends_with(|c: char| c.is_whitespace()) {
        result.push(' ');
    }
    Some(result)
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
        Value::Number(number) => format_number_for_output(number),
        Value::String(string) => string.clone(),
        Value::Array(values) => values.iter().map(value_to_string).collect(),
        Value::Object(object) => {
            // Date objects (encoded as { __albedo_date__: ms }) print as the
            // ISO string, mirroring JS's `String(new Date())` shape closely
            // enough for templates that interpolate them directly. Anything
            // else falls through to JSON for visibility.
            if let Some(ms) = object
                .get("__albedo_date__")
                .and_then(|v| v.as_f64())
            {
                return format_date_iso(ms);
            }
            serde_json::to_string(object).unwrap_or_default()
        }
    }
}

/// Format a JSON number the way JS's `String(n)` does: integers without a
/// trailing `.0`, floats with the standard ECMAScript-ish representation.
/// `serde_json::Number::from_f64(42.0).to_string()` yields "42.0", which
/// silently drifts from JS semantics — fix it once at the print site.
pub fn format_number_for_output(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(f) = n.as_f64() {
        if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
            return format!("{}", f as i64);
        }
        return n.to_string();
    }
    n.to_string()
}

/// Encode a Date instance as a tagged JSON object so it survives through
/// the evaluator's `Value` substrate without needing a parallel type.
pub fn make_date_value(ms: f64) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "__albedo_date__".to_string(),
        serde_json::Number::from_f64(ms)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
    Value::Object(map)
}

pub fn date_value_ms(value: &Value) -> Option<f64> {
    value
        .as_object()
        .and_then(|m| m.get("__albedo_date__"))
        .and_then(|v| v.as_f64())
}

/// Coerce a runtime `Value` to an f64 the way JS's arithmetic operators
/// would. NaN-on-failure is left as 0.0 because the static evaluator
/// never surfaces NaN to HTML — Phase K's reactive path can take over.
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(true) => 1.0,
        Value::Bool(false) => 0.0,
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        Value::Array(_) | Value::Object(_) => 0.0,
    }
}

pub fn json_num(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn json_int(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

pub fn arg_num(args: &[Value], index: usize) -> f64 {
    args.get(index).map(to_number).unwrap_or(0.0)
}

fn format_date_iso(ms: f64) -> String {
    let total_ms = ms as i64;
    let mut secs = total_ms.div_euclid(1000);
    let mut millis = total_ms.rem_euclid(1000) as u32;
    if millis >= 1000 {
        secs += 1;
        millis -= 1000;
    }
    let (y, mo, d, h, mi, s) = epoch_seconds_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, millis
    )
}

fn epoch_seconds_to_ymd_hms(mut secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let day_secs: i64 = 86_400;
    let mut days = secs.div_euclid(day_secs);
    secs = secs.rem_euclid(day_secs);
    let hour = (secs / 3600) as u32;
    let minute = ((secs % 3600) / 60) as u32;
    let second = (secs % 60) as u32;

    // Civil-from-days (Howard Hinnant), works for the full proleptic Gregorian
    // range including pre-1970 negative inputs.
    days += 719_468;
    let era = if days >= 0 { days / 146_097 } else { (days - 146_096) / 146_097 };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hour, minute, second)
}

pub fn prop_name_to_string(name: &swc_ecma_ast::PropName) -> Option<String> {
    match name {
        swc_ecma_ast::PropName::Ident(ident) => Some(ident.sym.to_string()),
        swc_ecma_ast::PropName::Str(str_lit) => Some(str_lit.value.to_string()),
        swc_ecma_ast::PropName::Num(num) => Some(num.value.to_string()),
        _ => None,
    }
}
