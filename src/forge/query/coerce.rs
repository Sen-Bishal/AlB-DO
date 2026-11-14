//! T-0033 · Q1a — argument coercion (Design 7). **Rust decides what reaches
//! SQL**: SQLite's type affinity would match `"042"` against an integer 42 if
//! it got that far, so a value that isn't exactly the column's type is
//! rejected here, before any statement exists.
//!
//! A rejected value from a **route param** (untrusted text) means "this term
//! can't match" — the AND group is empty, an OR branch drops — and nothing
//! touches SQL. A rejected value from **code** is a bug at the call site and
//! comes back as an error.

use serde_json::Value;

use crate::forge::declare::FieldType;
use crate::forge::value::SqlValue;

/// The largest integer a JS number holds exactly: 2^53 − 1.
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
/// Most elements an `in` array may carry.
pub const MAX_IN_ELEMENTS: usize = 1000;

/// Where a value came from (Design 4: by binding, not spelling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The component's actual `params` binding — untrusted text from the URL.
    RouteParam,
    /// The actual `user` binding.
    Principal,
    /// Anything else.
    Code,
}

/// The verdict on one value.
#[derive(Debug, Clone, PartialEq)]
pub enum Coerced {
    /// Bind this.
    Bind(SqlValue),
    /// A route-param value that isn't of the column's type: the term can't
    /// match, and no SQL runs for its AND group.
    NoMatch,
}

/// A value from code that doesn't fit its column.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value} is not a valid {ty} for `{column}`")]
pub struct CoerceError {
    pub column: String,
    pub ty: &'static str,
    pub value: String,
}

/// Coerce one scalar for an equality or range term on a column of type `ty`.
///
/// `null` binds as SQL `NULL` (equality lowers to `IS`, so it means `IS NULL`);
/// callers reject `null` in a range or an `in` array before getting here.
///
/// # Errors
/// [`CoerceError`] when a [`Provenance::Code`] or [`Provenance::Principal`]
/// value doesn't fit `ty`.
pub fn coerce(
    value: &Value,
    ty: FieldType,
    provenance: Provenance,
    column: &str,
) -> Result<Coerced, CoerceError> {
    if value.is_null() {
        return Ok(Coerced::Bind(SqlValue::Null));
    }
    let fitted = match ty {
        FieldType::Int | FieldType::Timestamp => int(value).map(SqlValue::Integer),
        FieldType::Real => real(value).map(SqlValue::Real),
        FieldType::Text => value.as_str().map(|s| SqlValue::Text(s.to_string())),
        FieldType::Bool => boolean(value, provenance).map(|b| SqlValue::Integer(i64::from(b))),
    };
    match (fitted, provenance) {
        (Some(v), _) => Ok(Coerced::Bind(v)),
        (None, Provenance::RouteParam) => Ok(Coerced::NoMatch),
        (None, _) => Err(CoerceError {
            column: column.to_string(),
            ty: super::descriptor::type_name(ty),
            value: value.to_string(),
        }),
    }
}

/// Coerce an `in` array: each element alone, under the same rule.
/// Route-param elements that don't fit are dropped; code elements error.
/// The result is the JSON text bound to `json_each(?)` — or `None` when no
/// element survives (no match).
///
/// # Errors
/// [`CoerceError`] for a code element that doesn't fit, a `null` element, or
/// more than [`MAX_IN_ELEMENTS`] elements.
pub fn coerce_in(
    values: &[Value],
    ty: FieldType,
    provenance: Provenance,
    column: &str,
) -> Result<Option<String>, CoerceError> {
    if values.len() > MAX_IN_ELEMENTS {
        return Err(CoerceError {
            column: column.to_string(),
            ty: "array of at most 1000 elements",
            value: format!("an array of {}", values.len()),
        });
    }
    let mut kept = Vec::with_capacity(values.len());
    for value in values {
        if value.is_null() {
            return Err(CoerceError {
                column: column.to_string(),
                ty: "non-null element",
                value: "null".to_string(),
            });
        }
        match coerce(value, ty, provenance, column)? {
            Coerced::Bind(SqlValue::Integer(n)) => kept.push(Value::from(n)),
            Coerced::Bind(SqlValue::Real(x)) => kept.push(Value::from(x)),
            Coerced::Bind(SqlValue::Text(s)) => kept.push(Value::from(s)),
            Coerced::Bind(_) | Coerced::NoMatch => {}
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    // i64s survive `json_each` exactly: serde_json writes integers as integers.
    Ok(Some(Value::Array(kept).to_string()))
}

/// int/timestamp: a JS number that is a safe integer, or a string matching
/// `^(0|-?[1-9][0-9]*)$` that fits i64. Strings aren't subject to JS precision,
/// so `"9007199254740993"` is accepted; `"042"`, `" 42"`, `"4a"`, `"42.0"`,
/// `"1e3"` are not; a JS number beyond 2^53 − 1 is not.
fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&i).then_some(i);
            }
            let f = n.as_f64()?;
            if f.fract() != 0.0 || f.abs() > MAX_SAFE_INTEGER as f64 {
                return None;
            }
            Some(f as i64)
        }
        Value::String(s) => {
            let digits = s.strip_prefix('-').unwrap_or(s);
            let canonical = s == "0"
                || (!digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && !digits.starts_with('0'));
            if !canonical || s == "-0" {
                return None;
            }
            s.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// real: a finite JS number, or a string matching
/// `^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$` that parses finite.
fn real(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => real_literal(s).then(|| s.parse::<f64>().ok()).flatten().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn real_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    // integer part: 0 | [1-9][0-9]*
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false,
    }
    // fraction
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    // exponent
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}

/// bool: `true`/`false`, plus the strings `"true"`/`"false"` from a route
/// param only.
fn boolean(value: &Value, provenance: Provenance) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) if provenance == Provenance::RouteParam => match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}
