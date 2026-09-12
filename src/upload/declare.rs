//! UPLOADS · 15.1 — the `uploads` block.
//!
//! ## Why an upload is declared at all
//!
//! The same reason a `source` is, and the reason is not symmetry — it is that
//! **the caps have to be knowable before the body is read.**
//!
//! `transforms::form`'s [`ACTION_ENDPOINT_PREFIX`] doc names the original
//! problem: the action id used to ride *inside* a bincode envelope, so the
//! server could not know what an action was until it had buffered the whole
//! body, *"which is why SHUTTER charges a flat `Write` before it knows what it
//! is charging for and why streaming uploads are foreclosed."*
//!
//! Half of that is already fixed — the action's **name** is in the request line
//! now, so the server can route and price from the request line alone. This
//! block is the other half: given the name, the server can look up *how many
//! bytes this action is allowed to accept and of what type* and refuse at the
//! header, before a single byte of the body is read.
//!
//! An undeclared bucket is therefore not a missing convenience. It is a request
//! with no bound, and it is refused.
//!
//! ## What owning this lets the compiler say
//!
//! `TODO.md` item 15's rule: *for each surface, does owning it let the compiler
//! say something nobody else can?* Uploads are table stakes and the honest
//! answer is "mostly no" — so this block is deliberately small, and everything
//! here is derived rather than maintained:
//!
//! - an `<input type="file">` inside a form whose action writes to a declared bucket can have its
//!   `accept` attribute **derived** from [`UploadDecl::accept`], so the browser's file picker and
//!   the server's refusal cannot disagree;
//! - a form that uploads to a bucket nobody declared fails at `albedo build`, which is the same
//!   check `sources`, `forge` and `auth` already get.
//!
//! That is the whole claim. There is no transform pipeline here, no image
//! resizing and no CDN — those are `15.9`, deferred on purpose.
//!
//! [`ACTION_ENDPOINT_PREFIX`]: crate::transforms::form::ACTION_ENDPOINT_PREFIX

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Default ceiling for a bucket that declares no `maxBytes`.
///
/// 8 MiB: comfortably a phone photo, nowhere near a video. A default that is
/// *too small* is a one-line fix by the author who hits it; a default that is
/// too large is a disk-filling hole nobody notices, so this errs small.
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Hard ceiling on what any bucket may declare.
///
/// Not a policy about files — a bound on what a **declaration** can ask the
/// process to accept, so a typo (`maxBytes: "500gb"`) is refused at lowering
/// rather than at 2 a.m. Raising it is a source edit, which is the correct
/// amount of friction for "this app accepts half-gigabyte uploads".
pub const MAX_DECLARABLE_BYTES: u64 = 512 * 1024 * 1024;

/// What a `uploads` block failed to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadSchemaError {
    /// The bucket name is not a URL path segment.
    InvalidName {
        /// The offending name.
        name: String,
    },
    /// `maxBytes` did not parse.
    InvalidSize {
        /// The bucket.
        bucket: String,
        /// What was written.
        value: String,
        /// Why it did not parse.
        reason: String,
    },
    /// `maxBytes` is over [`MAX_DECLARABLE_BYTES`].
    SizeTooLarge {
        /// The bucket.
        bucket: String,
        /// What was asked for.
        requested: u64,
    },
    /// An `accept` entry is not a media type.
    InvalidAccept {
        /// The bucket.
        bucket: String,
        /// The offending entry.
        value: String,
    },
    /// `accept: []` — an empty list accepts nothing, which is never what an
    /// author means and is silently a bucket that refuses every upload.
    EmptyAccept {
        /// The bucket.
        bucket: String,
    },
}

impl fmt::Display for UploadSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(
                f,
                "`{name}` is not a usable upload bucket name — lowercase letters, digits, `_` \
                 and `-` only, because the name appears in a URL path"
            ),
            Self::InvalidSize {
                bucket,
                value,
                reason,
            } => write!(f, "`{bucket}` declares maxBytes `{value}`: {reason}"),
            Self::SizeTooLarge { bucket, requested } => write!(
                f,
                "`{bucket}` asks for {requested} bytes, over the {MAX_DECLARABLE_BYTES}-byte \
                 ceiling a declaration may request"
            ),
            Self::InvalidAccept { bucket, value } => write!(
                f,
                "`{bucket}` accepts `{value}`, which is not a media type — write `image/png`, \
                 or `image/*` for a whole family"
            ),
            Self::EmptyAccept { bucket } => write!(
                f,
                "`{bucket}` declares `accept: []`, which accepts nothing. Omit `accept` to take \
                 any type, or name the types you want"
            ),
        }
    }
}

impl std::error::Error for UploadSchemaError {}

/// One declared bucket, as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadDecl {
    /// Media types this bucket takes. Absent means any.
    ///
    /// `image/*` is accepted as a family wildcard, because that is what an
    /// author writes in the HTML `accept` attribute and having the two spellings
    /// diverge would be a trap.
    #[serde(default)]
    pub accept: Option<Vec<String>>,
    /// Per-file ceiling, as `"5mb"` / `"900kb"` / a bare byte count.
    #[serde(default, rename = "maxBytes", alias = "max_bytes")]
    pub max_bytes: Option<String>,
}

/// A validated bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBucket {
    /// The declared name; also the path segment.
    pub name: String,
    /// Media types, lowercased. Empty means any.
    pub accept: Vec<String>,
    /// Per-file ceiling in bytes.
    pub max_bytes: u64,
}

impl ResolvedBucket {
    /// Whether this bucket takes `content_type`.
    ///
    /// Compared against the type **the part declared**, which is client-supplied
    /// and therefore a claim rather than a fact. That is deliberate and is why
    /// this is not the only defence: the stored bytes are served back with the
    /// type recorded here and with `X-Content-Type-Options: nosniff`, so a file
    /// that lied about being a PNG is served as a PNG that does not render
    /// rather than as script a browser will run.
    #[must_use]
    pub fn accepts(&self, content_type: &str) -> bool {
        if self.accept.is_empty() {
            return true;
        }
        // Drop any `; charset=…` before comparing — a browser may send one and
        // it is not part of the type.
        let offered = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        self.accept.iter().any(|allowed| {
            allowed
                .strip_suffix("/*")
                .map_or(allowed == &offered, |family| {
                    offered
                        .split('/')
                        .next()
                        .is_some_and(|offered_family| offered_family == family)
                })
        })
    }
}

/// The lowered `uploads` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadRegistry {
    /// Buckets, in declaration order.
    pub buckets: Vec<ResolvedBucket>,
}

impl UploadRegistry {
    /// Look a bucket up by name.
    #[must_use]
    pub fn bucket(&self, name: &str) -> Option<&ResolvedBucket> {
        self.buckets.iter().find(|bucket| bucket.name == name)
    }

    /// Whether this app declared any bucket at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// The largest ceiling any bucket declared.
    ///
    /// **The bound the request path applies before it knows which bucket a part
    /// is for**, since a multipart body names its fields inside itself. Reading
    /// past this is refused whatever the parts turn out to be, so an app whose
    /// biggest bucket is 1 MiB cannot be made to read 500 MiB by a body that
    /// claims to be for a bucket that does not exist.
    #[must_use]
    pub fn ceiling(&self) -> u64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.max_bytes)
            .max()
            .unwrap_or(0)
    }

    /// Lower a declared block.
    ///
    /// # Errors
    /// [`UploadSchemaError`] for any bucket that will not validate.
    pub fn from_declarations(
        declarations: &BTreeMap<String, UploadDecl>,
    ) -> Result<Self, UploadSchemaError> {
        let mut buckets = Vec::with_capacity(declarations.len());
        for (name, decl) in declarations {
            if !is_valid_bucket_name(name) {
                return Err(UploadSchemaError::InvalidName { name: name.clone() });
            }

            let max_bytes = match &decl.max_bytes {
                None => DEFAULT_MAX_BYTES,
                Some(raw) => {
                    let parsed =
                        parse_size(raw).map_err(|reason| UploadSchemaError::InvalidSize {
                            bucket: name.clone(),
                            value: raw.clone(),
                            reason,
                        })?;
                    if parsed > MAX_DECLARABLE_BYTES {
                        return Err(UploadSchemaError::SizeTooLarge {
                            bucket: name.clone(),
                            requested: parsed,
                        });
                    }
                    parsed
                }
            };

            let accept = match &decl.accept {
                None => Vec::new(),
                Some(list) if list.is_empty() => {
                    return Err(UploadSchemaError::EmptyAccept {
                        bucket: name.clone(),
                    })
                }
                Some(list) => {
                    let mut accept = Vec::with_capacity(list.len());
                    for entry in list {
                        let entry = entry.trim().to_ascii_lowercase();
                        if !is_media_type(&entry) {
                            return Err(UploadSchemaError::InvalidAccept {
                                bucket: name.clone(),
                                value: entry,
                            });
                        }
                        accept.push(entry);
                    }
                    accept
                }
            };

            buckets.push(ResolvedBucket {
                name: name.clone(),
                accept,
                max_bytes,
            });
        }
        Ok(Self { buckets })
    }
}

/// A bucket name has to survive being a URL path segment and a column value.
///
/// The same alphabet [`crate::auth::is_valid_provider_name`] uses, and for the
/// same reason — these two are the only user-chosen strings that appear in a
/// framework route.
#[must_use]
pub fn is_valid_bucket_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `type/subtype`, or `type/*`.
fn is_media_type(value: &str) -> bool {
    let Some((family, subtype)) = value.split_once('/') else {
        return false;
    };
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.' | b'_'))
    };
    token(family) && (subtype == "*" || token(subtype))
}

/// Parse `"5mb"` / `"900kb"` / `"1048576"` into bytes.
///
/// Binary units (`kb` = 1024), matching how every other size in this codebase is
/// spoken about and how a person picking "5mb" actually reasons. Stated in the
/// error rather than left to be discovered.
fn parse_size(raw: &str) -> Result<u64, String> {
    let text = raw.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err("it is empty".to_string());
    }
    let (digits, multiplier) = if let Some(rest) = text.strip_suffix("gb") {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = text.strip_suffix("mb") {
        (rest, 1024 * 1024)
    } else if let Some(rest) = text.strip_suffix("kb") {
        (rest, 1024)
    } else if let Some(rest) = text.strip_suffix('b') {
        (rest, 1)
    } else {
        (text.as_str(), 1)
    };

    let digits = digits.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "`{raw}` is not a size — write a byte count, or a number with `kb`, `mb` or `gb` \
             (binary units, so 1kb is 1024 bytes)"
        ));
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| format!("`{raw}` does not fit in a byte count, or is zero"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: serde_json::Value) -> BTreeMap<String, UploadDecl> {
        serde_json::from_value(json).expect("parses")
    }

    #[test]
    fn a_bucket_with_no_options_gets_the_default_ceiling_and_takes_any_type() {
        let registry =
            UploadRegistry::from_declarations(&block(serde_json::json!({ "attachments": {} })))
                .expect("lowers");
        let bucket = registry.bucket("attachments").expect("declared");
        assert_eq!(bucket.max_bytes, DEFAULT_MAX_BYTES);
        assert!(bucket.accepts("application/pdf"));
        assert!(bucket.accepts("image/png"));
    }

    #[test]
    fn sizes_parse_in_binary_units() {
        for (written, expected) in [
            ("1024", 1024_u64),
            ("1b", 1),
            ("1kb", 1024),
            ("5mb", 5 * 1024 * 1024),
            ("1gb", 1024 * 1024 * 1024),
            ("  2MB  ", 2 * 1024 * 1024),
        ] {
            assert_eq!(parse_size(written), Ok(expected), "{written}");
        }
    }

    #[test]
    fn a_size_that_is_not_one_is_refused_with_the_spelling_that_works() {
        for written in ["", "five", "5 megabytes", "-1", "0", "1.5mb", "mb"] {
            assert!(parse_size(written).is_err(), "`{written}` must not parse");
        }
    }

    /// A typo in a unit is the realistic way this goes wrong, and the result is
    /// a process that will happily read half a gigabyte into a temp file.
    #[test]
    fn a_declaration_cannot_ask_for_more_than_the_ceiling() {
        let error = UploadRegistry::from_declarations(&block(serde_json::json!({
            "video": { "maxBytes": "500gb" }
        })))
        .expect_err("refused");
        assert!(matches!(error, UploadSchemaError::SizeTooLarge { .. }));
    }

    #[test]
    fn a_family_wildcard_matches_the_family_and_nothing_else() {
        let registry = UploadRegistry::from_declarations(&block(serde_json::json!({
            "avatars": { "accept": ["image/*"] }
        })))
        .expect("lowers");
        let bucket = registry.bucket("avatars").expect("declared");
        assert!(bucket.accepts("image/png"));
        assert!(bucket.accepts("image/svg+xml"));
        assert!(!bucket.accepts("text/html"));
        assert!(!bucket.accepts("imagex/png"), "the family must match whole");
    }

    /// A browser may append one, and it is not part of the type.
    #[test]
    fn a_charset_parameter_does_not_defeat_the_accept_list() {
        let registry = UploadRegistry::from_declarations(&block(serde_json::json!({
            "notes": { "accept": ["text/plain"] }
        })))
        .expect("lowers");
        assert!(registry
            .bucket("notes")
            .unwrap()
            .accepts("text/plain; charset=utf-8"));
    }

    /// `accept: []` reads like "no restriction" and means "refuse everything".
    /// It is refused at lowering rather than becoming a bucket that silently
    /// rejects every upload an author tries.
    #[test]
    fn an_empty_accept_list_is_refused_rather_than_silently_refusing_everything() {
        let error = UploadRegistry::from_declarations(&block(serde_json::json!({
            "nothing": { "accept": [] }
        })))
        .expect_err("refused");
        assert!(matches!(error, UploadSchemaError::EmptyAccept { .. }));
    }

    #[test]
    fn a_bucket_name_has_to_be_a_path_segment() {
        for name in ["", "Avatars", "my bucket", "a/b", "../etc"] {
            assert!(!is_valid_bucket_name(name), "`{name}` must not be valid");
        }
        for name in ["avatars", "user_files", "logo-2x", "a1"] {
            assert!(is_valid_bucket_name(name), "`{name}` must be valid");
        }
    }

    /// The bound the request path applies before it can know which bucket a
    /// part belongs to — a multipart body names its fields inside itself.
    #[test]
    fn the_ceiling_is_the_largest_declared_bucket() {
        let registry = UploadRegistry::from_declarations(&block(serde_json::json!({
            "small": { "maxBytes": "1kb" },
            "large": { "maxBytes": "1mb" }
        })))
        .expect("lowers");
        assert_eq!(registry.ceiling(), 1024 * 1024);
    }

    /// An app that declared nothing has a ceiling of zero, so the request path
    /// refuses a multipart body outright rather than defaulting to some
    /// generous number nobody chose.
    #[test]
    fn an_app_with_no_buckets_has_a_ceiling_of_zero() {
        assert_eq!(UploadRegistry::default().ceiling(), 0);
        assert!(UploadRegistry::default().is_empty());
    }
}
