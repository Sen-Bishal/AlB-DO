//! UPLOADS · 15.1 — the request-time half: a multipart body becomes files on
//! disk and fields an action can read.
//!
//! The compiler crate owns the declaration and the layout
//! ([`dom_render_compiler::upload`]); this owns the one thing that needs a
//! request body.
//!
//! ## Nothing is buffered
//!
//! `forms.rs` refuses `multipart/form-data` with a reason rather than a shrug:
//! *"decoding it would mean buffering a file into memory on a path whose entire
//! purpose is to be cheap."* That reason is still correct, so this is not that
//! path. A file part is read chunk by chunk straight into a temp file, hashed as
//! it goes, and the bytes are never all in memory at once — not in a `Vec`, not
//! in a `Bytes`, and above all not in the QuickJS arena.
//!
//! What an action body receives is the **id**, as the field's value. A text
//! field decodes to its text; a file field decodes to its id. The action ABI is
//! unchanged, there is no new JS type, and `append("photos", { file: form.photo })`
//! is the whole authoring story.
//!
//! ## Three bounds, and why each exists
//!
//! | bound | why it is not the others' job |
//! |---|---|
//! | per **file** part | the bucket's own `maxBytes` — the thing an author actually reasons about |
//! | per **text** part | [`crate::forms::MAX_FORM_BODY_BYTES`]; a caption is not a file and must not get a file's allowance |
//! | whole **body** | [`MAX_FILE_PARTS`] × the largest declared bucket — because a body may carry several parts, and without this a caller could send an unbounded stream of legal-sized files |
//!
//! Every one is enforced **while reading**, so an over-sized upload costs the
//! bytes seen before the limit and not the whole file. A cap checked after
//! `to_bytes` would have already paid for the attack it refuses.
//!
//! ## Why a temp file and a rename
//!
//! The id is the hash, so the final path is not known until the last byte is
//! read. Writing to `uploads/.tmp/<random>` and renaming on success means a
//! reader can never observe a partial file at a real id — a rename within one
//! filesystem is atomic, and the temp directory is deliberately *inside*
//! `uploads/` so it is on that filesystem by construction rather than by luck.

use bytes::Bytes;
use dom_render_compiler::forge::DataSubstrate;
use dom_render_compiler::upload::store::{self, StoredUpload};
use dom_render_compiler::upload::{sanitise_name, ReadRule, UploadRegistry};
use futures_util::Stream;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::warn;

/// How many file parts one submit may carry.
///
/// Eight covers a form with a handful of attachments and bounds the whole-body
/// read at `8 ×` the largest declared bucket. A number rather than a
/// declaration because no author has an opinion about it until they hit it, and
/// the failure is a clear refusal rather than a silent truncation.
pub const MAX_FILE_PARTS: usize = 8;

/// Temp directory for in-flight writes, under [`store::UPLOAD_DIR`].
const TMP_DIR: &str = ".tmp";

/// Why a multipart submit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    /// The app declared no `uploads` block, so it accepts no files at all.
    NoBucketsDeclared,
    /// A file part named a bucket nobody declared.
    UnknownBucket {
        /// The field name, which is the bucket name.
        bucket: String,
    },
    /// The part's media type is not in that bucket's `accept` list.
    TypeRefused {
        /// The bucket.
        bucket: String,
        /// What the part claimed to be.
        offered: String,
    },
    /// An anonymous upload into a bucket only its uploader may read.
    SignInRequired {
        /// The bucket.
        bucket: String,
    },
    /// A file part ran past its bucket's ceiling.
    FileTooLarge {
        /// The bucket.
        bucket: String,
        /// What the bucket allows.
        limit: u64,
    },
    /// A text part ran past the form-field cap.
    FieldTooLarge {
        /// The field.
        field: String,
    },
    /// More file parts than [`MAX_FILE_PARTS`].
    TooManyFiles,
    /// The whole body ran past its derived ceiling.
    BodyTooLarge,
    /// The body was not well-formed multipart.
    Malformed {
        /// What the parser said.
        reason: String,
    },
    /// The bytes could not be written.
    Io {
        /// What the filesystem said.
        reason: String,
    },
    /// A file part arrived before the submit showed a valid CSRF token, or the
    /// token it showed was wrong. See [`Genuine`].
    Forged,
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBucketsDeclared => write!(
                f,
                "this app declares no `uploads` block, so it accepts no files. Declare a bucket \
                 in albedo.config.ts and name a file input after it"
            ),
            Self::UnknownBucket { bucket } => write!(
                f,
                "no upload bucket called `{bucket}` is declared — a file input's `name` is the \
                 bucket it writes to"
            ),
            Self::TypeRefused { bucket, offered } => write!(
                f,
                "`{bucket}` does not accept `{offered}`"
            ),
            Self::SignInRequired { bucket } => write!(
                f,
                "sign in to upload to `{bucket}` — only the person who uploads a file there may \
                 read it back, and a visitor who is not signed in is nobody. A bucket open to \
                 anonymous uploads says `read: \"signed-in\"` or `read: \"public\"`"
            ),
            Self::FileTooLarge { bucket, limit } => {
                write!(f, "that file is over `{bucket}`'s {limit}-byte limit")
            }
            Self::FieldTooLarge { field } => write!(f, "the `{field}` field is too large"),
            Self::TooManyFiles => write!(f, "a submit may carry at most {MAX_FILE_PARTS} files"),
            Self::BodyTooLarge => write!(f, "that submit is too large"),
            Self::Malformed { reason } => write!(f, "the submit was not readable: {reason}"),
            Self::Io { .. } => write!(f, "the upload could not be stored"),
            Self::Forged => write!(
                f,
                "CSRF validation failed: files are stored only after the submit's token is \
                 checked, so the `x-albedo-csrf` header or the `_csrf` field must come before any \
                 file field (the forms albedo renders put it first)"
            ),
        }
    }
}

impl std::error::Error for UploadError {}

/// What a decoded multipart submit yielded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MultipartForm {
    /// Every field, text and file alike. A file field's value is its id.
    pub fields: BTreeMap<String, String>,
    /// The files that were stored, for the caller to record.
    pub uploads: Vec<StoredUpload>,
}

impl MultipartForm {
    /// A field's value.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Everything the decode needs that is not the body.
pub struct UploadContext<'a> {
    /// Declared buckets.
    pub registry: &'a UploadRegistry,
    /// The data directory — uploads land under `<data_dir>/uploads/`, beside
    /// the database that records them.
    pub data_dir: &'a Path,
    /// Who is uploading, when anyone is signed in.
    pub principal: Option<String>,
    /// Whether the submit has shown it is not forged.
    pub genuine: Genuine<'a>,
}

/// Whether a submit has shown it is not a forgery — asked **before a file part
/// is written**, not after the body.
///
/// 🔴 The action's CSRF check runs once the body is decoded, and decoding a
/// multipart body *is* streaming its files to disk. So a forged cross-site
/// submit was refused with every file it carried already stored and recorded.
/// The token is in the body too, which is why this is a question asked during
/// the stream rather than before it: the `x-albedo-csrf` header answers it up
/// front, and otherwise a `_csrf` field must arrive — and pass — before the
/// first file part. Browsers send fields in document order and both renderers
/// make the token a form's first child, so a real form always qualifies; a
/// file before the token is refused without a byte of it written.
pub enum Genuine<'a> {
    /// Already shown — a valid `x-albedo-csrf` header.
    Yes,
    /// Not yet. This checks a `_csrf` field's value.
    Pending(&'a (dyn Fn(&str) -> bool + Sync)),
}

/// Decode a `multipart/form-data` body, streaming file parts to disk.
///
/// # Errors
/// [`UploadError`] for any of the three bounds, an undeclared bucket, a refused
/// media type, a malformed body, or a filesystem failure.
pub async fn decode_multipart<S>(
    stream: S,
    boundary: &str,
    context: &UploadContext<'_>,
) -> Result<MultipartForm, UploadError>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    if context.registry.is_empty() {
        // Refused before a byte is read. An app with no `uploads` block has no
        // bound to enforce, and "no declaration" is not the same as "no limit".
        return Err(UploadError::NoBucketsDeclared);
    }

    let body_ceiling = context
        .registry
        .ceiling()
        .saturating_mul(MAX_FILE_PARTS as u64)
        .saturating_add(crate::forms::MAX_FORM_BODY_BYTES as u64);

    let mut multipart = multer::Multipart::new(stream, boundary.to_string());
    let mut form = MultipartForm::default();
    let mut files_seen = 0usize;
    let mut body_seen = 0u64;
    let mut genuine = matches!(context.genuine, Genuine::Yes);

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|err| UploadError::Malformed {
            reason: err.to_string(),
        })?
    {
        let Some(name) = field.name().map(str::to_string) else {
            // A part with no name cannot be addressed by anything downstream.
            // Draining it rather than refusing keeps a browser quirk from
            // becoming a failed submit.
            while field.chunk().await.ok().flatten().is_some() {}
            continue;
        };

        // `file_name()` present is what makes a part a file — the same rule a
        // browser follows. A text input never carries one, and a file input
        // always does, even when the chosen file is empty.
        let file_name = field.file_name().map(str::to_string);
        let content_type = field.content_type().map(ToString::to_string);

        if let Some(file_name) = file_name {
            // First, before what the part is aiming at: a submit that is not
            // genuine is refused as forged whatever bucket it named.
            if !genuine {
                return Err(UploadError::Forged);
            }
            files_seen += 1;
            if files_seen > MAX_FILE_PARTS {
                return Err(UploadError::TooManyFiles);
            }

            // The bucket is decided **before** the first chunk is read, so the
            // limit that governs this part is in hand when the reading starts.
            let bucket = context.registry.bucket(&name).ok_or_else(|| {
                UploadError::UnknownBucket {
                    bucket: name.clone(),
                }
            })?;

            // An owner-only bucket keeps a file for whoever uploaded it, and an
            // anonymous visitor is nobody: the file would be stored, and then
            // readable by no one, forever. Refused before a byte is read.
            if bucket.read == ReadRule::Owner && context.principal.is_none() {
                return Err(UploadError::SignInRequired {
                    bucket: bucket.name.clone(),
                });
            }

            let offered = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
            if !bucket.accepts(&offered) {
                return Err(UploadError::TypeRefused {
                    bucket: bucket.name.clone(),
                    offered,
                });
            }

            let stored = write_part(
                &mut field,
                bucket.max_bytes,
                &bucket.name,
                &offered,
                &sanitise_name(&file_name),
                context,
                &mut body_seen,
                body_ceiling,
            )
            .await?;

            // The field's value is the id. This is the whole ABI.
            form.fields.insert(name, stored.id.clone());
            form.uploads.push(stored);
            continue;
        }

        // A text part. Bounded by the form-field cap, not the file cap.
        let mut text = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(|err| UploadError::Malformed {
            reason: err.to_string(),
        })? {
            body_seen = body_seen.saturating_add(chunk.len() as u64);
            if body_seen > body_ceiling {
                return Err(UploadError::BodyTooLarge);
            }
            if text.len() + chunk.len() > crate::forms::MAX_FORM_BODY_BYTES {
                return Err(UploadError::FieldTooLarge { field: name });
            }
            text.extend_from_slice(&chunk);
        }
        // Lossy for the same reason `FormBody::decode` is lossy: a browser will
        // not send invalid UTF-8 in a text field, so a body that does is not a
        // browser and does not deserve a distinct error to probe.
        let value = String::from_utf8_lossy(&text).into_owned();
        if name == crate::render::csrf::CSRF_FIELD_NAME {
            if let Genuine::Pending(check) = context.genuine {
                // A wrong token is refused now, as the action would refuse it:
                // nothing after it can make the submit genuine.
                if !check(&value) {
                    return Err(UploadError::Forged);
                }
                genuine = true;
            }
        }
        form.fields.insert(name, value);
    }

    Ok(form)
}

/// Stream one file part to disk, hashing as it goes.
#[allow(clippy::too_many_arguments)]
async fn write_part(
    field: &mut multer::Field<'_>,
    limit: u64,
    bucket: &str,
    content_type: &str,
    original_name: &str,
    context: &UploadContext<'_>,
    body_seen: &mut u64,
    body_ceiling: u64,
) -> Result<StoredUpload, UploadError> {
    let upload_root = context.data_dir.join(store::UPLOAD_DIR);
    let tmp_dir = upload_root.join(TMP_DIR);
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|err| UploadError::Io {
            reason: err.to_string(),
        })?;

    let tmp_path = tmp_dir.join(format!("{:032x}", rand::random::<u128>()));
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|err| UploadError::Io {
            reason: err.to_string(),
        })?;

    let mut hasher = Sha256::new();
    let mut byte_size = 0u64;

    // The read loop. Every bound is checked *inside* it, so an over-sized
    // upload costs the bytes seen before the limit rather than the whole file.
    let outcome = loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break Ok(()),
            Err(err) => {
                break Err(UploadError::Malformed {
                    reason: err.to_string(),
                })
            }
        };

        byte_size = byte_size.saturating_add(chunk.len() as u64);
        *body_seen = body_seen.saturating_add(chunk.len() as u64);
        if byte_size > limit {
            break Err(UploadError::FileTooLarge {
                bucket: bucket.to_string(),
                limit,
            });
        }
        if *body_seen > body_ceiling {
            break Err(UploadError::BodyTooLarge);
        }

        hasher.update(&chunk);
        if let Err(err) = file.write_all(&chunk).await {
            break Err(UploadError::Io {
                reason: err.to_string(),
            });
        }
    };

    if let Err(err) = outcome {
        discard(&tmp_path).await;
        return Err(err);
    }
    if let Err(err) = file.flush().await {
        discard(&tmp_path).await;
        return Err(UploadError::Io {
            reason: err.to_string(),
        });
    }
    drop(file);

    let id = format!("{:x}", hasher.finalize());
    let final_path = store::path_for(context.data_dir, &id).map_err(|err| UploadError::Io {
        reason: err.to_string(),
    })?;

    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| UploadError::Io {
                reason: err.to_string(),
            })?;
    }

    // Content-addressed, so an existing file at this path has identical bytes
    // by construction. Renaming over it is a no-op with extra steps, and on
    // Windows it can fail outright if a reader holds the target open — so the
    // dedup case discards the temp file instead.
    if tokio::fs::metadata(&final_path).await.is_ok() {
        discard(&tmp_path).await;
    } else if let Err(err) = tokio::fs::rename(&tmp_path, &final_path).await {
        // A concurrent upload of the same bytes can win this race. That is a
        // success, not a failure: the file at the destination is the file we
        // were about to write.
        if tokio::fs::metadata(&final_path).await.is_err() {
            discard(&tmp_path).await;
            return Err(UploadError::Io {
                reason: err.to_string(),
            });
        }
        discard(&tmp_path).await;
    }

    Ok(StoredUpload {
        id,
        bucket: bucket.to_string(),
        content_type: content_type.to_string(),
        byte_size,
        original_name: original_name.to_string(),
        principal: context.principal.clone(),
    })
}

/// Remove a temp file, reporting rather than propagating — the caller is
/// already returning the error that matters, and a leaked temp file must not
/// replace it.
async fn discard(path: &PathBuf) {
    if let Err(err) = tokio::fs::remove_file(path).await {
        warn!(target: "albedo.uploads", %err, "could not remove a temp upload");
    }
}

/// Record every stored file. Best-effort per row, so one failure does not lose
/// the rest — the bytes are already on disk either way.
pub async fn record_all(db: &dyn DataSubstrate, form: &MultipartForm, now_ms: i64) {
    for upload in &form.uploads {
        if let Err(err) = store::record(db, upload, now_ms).await {
            warn!(
                target: "albedo.uploads",
                %err,
                upload = %upload.id,
                "stored the bytes but could not record the row"
            );
        }
    }
}

/// Path prefix stored bytes are served back from.
pub const SERVE_PREFIX: &str = "/_albedo/uploads/";

/// The upload id in `GET /_albedo/uploads/{id}`, or `None`.
///
/// Exactly one segment, and it must already be a well-formed id — so nothing
/// that could traverse a directory reaches [`store::path_for`], which refuses
/// it a second time anyway. Two checks rather than one because this is the only
/// route where a request-supplied string becomes a filesystem path.
#[must_use]
pub fn serve_id(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(SERVE_PREFIX)?;
    (!rest.contains('/') && dom_render_compiler::upload::is_upload_id(rest)).then_some(rest)
}

/// Serve a stored file.
///
/// ## Why the headers are what they are
///
/// - **`Content-Type` from the row, never sniffed.** The type recorded is the one the bucket's
///   `accept` list approved; trusting the bytes instead would let a file that passed as
///   `image/png` be served as whatever a sniffer decided it looked like.
/// - **`X-Content-Type-Options: nosniff`.** The other half of that. A part's declared type is a
///   *claim*, so a file that lied still gets served as the type we accepted — a PNG that does not
///   render, rather than script a browser will run.
/// - **`Content-Disposition: attachment` for anything we did not positively decide is safe to
///   render inline.** The bytes are attacker-supplied and same-origin, which is the shape that
///   turns an upload store into stored XSS.
/// - **`private, no-cache`, with the hash as the `ETag`.** The bytes never change, but who may
///   read them can. A shared cache or CDN holding a copy would answer for the app, and neither
///   the read rule below nor `src/middleware.ts` would see the request. So every read reaches
///   the server, and a browser that already has the file gets a `304` instead of the bytes. An
///   app whose uploads are public can set its own `cache-control` from middleware.
///
/// ## Who may read it
///
/// `reader` is the signed-in principal, if any, and the bucket's declared
/// [`ReadRule`](dom_render_compiler::upload::ReadRule) decides — the uploader
/// only, unless the bucket says `"signed-in"` or `"public"`
/// ([`store::readable_by`]). A refused read is answered **exactly** as a file
/// that was never stored: same status, same body, same headers. Anything else
/// would turn the route into a test for whether a known file was ever uploaded.
/// The check runs before the `304` too, or revalidation would be a way round it.
pub async fn serve(
    db: &dyn DataSubstrate,
    registry: &UploadRegistry,
    data_dir: &Path,
    id: &str,
    reader: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{header as h, StatusCode};

    let not_found = || {
        axum::response::IntoResponse::into_response((
            StatusCode::NOT_FOUND,
            [(h::CACHE_CONTROL, "no-store")],
            "no such upload",
        ))
    };
    let header = |name: h::HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
    let Ok(records) = store::records(db, id).await else {
        return not_found();
    };
    let Some(record) = store::readable_by(&records, registry, reader).cloned() else {
        return not_found();
    };

    let etag = format!("\"{id}\"");
    if header(h::IF_NONE_MATCH).is_some_and(|value| etag_matches(value, &etag)) {
        return axum::response::IntoResponse::into_response((
            StatusCode::NOT_MODIFIED,
            [
                (h::ETAG, etag),
                (h::CACHE_CONTROL, UPLOAD_CACHE_CONTROL.to_string()),
            ],
        ));
    }
    let Ok(path) = store::path_for(data_dir, id) else {
        return not_found();
    };
    let opened = match tokio::fs::File::open(&path).await {
        Ok(file) => file.metadata().await.map(|meta| (file, meta.len())),
        Err(err) => Err(err),
    };
    let Ok((mut file, total)) = opened else {
        // A row with no file is a real inconsistency — a half-finished delete, a
        // restored database over an unrestored disk — and it is worth saying so
        // in the log rather than only answering 404.
        warn!(target: "albedo.uploads", upload = %id, "a recorded upload has no file on disk");
        return not_found();
    };

    let disposition = if renders_inline_safely(&record.content_type) {
        "inline".to_string()
    } else {
        // The name is already sanitised of quotes and control characters by
        // `sanitise_name` at upload time, which is what makes this
        // interpolation safe.
        format!("attachment; filename=\"{}\"", record.original_name)
    };

    // A range is honoured only for this exact file: `If-Range` naming another
    // entity means the client's partial copy is of something else, so it gets
    // the whole file instead. The id is the content hash, so a strong match is
    // exact.
    let range_applies = header(h::IF_RANGE).is_none_or(|value| value.trim() == etag);
    let wanted = if range_applies {
        header(h::RANGE).map_or(ByteRange::Whole, |value| parse_range(value, total))
    } else {
        ByteRange::Whole
    };
    let (status, start, length, content_range) = match wanted {
        ByteRange::Whole => (StatusCode::OK, 0, total, None),
        ByteRange::Part { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{total}")),
        ),
        ByteRange::Unsatisfiable => {
            return axum::response::IntoResponse::into_response((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    (h::CONTENT_RANGE, format!("bytes */{total}")),
                    (h::CACHE_CONTROL, UPLOAD_CACHE_CONTROL.to_string()),
                    (h::ETAG, etag),
                ],
            ));
        }
    };
    if start > 0 {
        use tokio::io::AsyncSeekExt;
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return not_found();
        }
    }

    let mut response = axum::response::Response::new(axum::body::Body::from_stream(
        file_chunks(file, length),
    ));
    *response.status_mut() = status;
    let response_headers = response.headers_mut();
    let mut set = |name: h::HeaderName, value: String| {
        if let Ok(value) = h::HeaderValue::from_str(&value) {
            response_headers.insert(name, value);
        }
    };
    set(h::CONTENT_TYPE, record.content_type.clone());
    set(h::CONTENT_LENGTH, length.to_string());
    set(h::ACCEPT_RANGES, "bytes".to_string());
    set(h::CACHE_CONTROL, UPLOAD_CACHE_CONTROL.to_string());
    set(h::ETAG, etag);
    set(h::CONTENT_DISPOSITION, disposition);
    set(h::HeaderName::from_static("x-content-type-options"), "nosniff".to_string());
    if let Some(content_range) = content_range {
        set(h::CONTENT_RANGE, content_range);
    }
    response
}

/// How much of a file a read gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    /// No usable `Range` — the whole file, `200`.
    Whole,
    /// Bytes `start..=end`, `206`.
    Part {
        /// First byte.
        start: u64,
        /// Last byte, inclusive.
        end: u64,
    },
    /// A range that starts past the end — `416`.
    Unsatisfiable,
}

/// Read a `Range` header against a file of `total` bytes.
///
/// One range, in any of the three forms (`a-b`, `a-`, `-n`) — what a media
/// element asks for. A header this does not understand, or one asking for
/// several ranges, is ignored and the whole file served: RFC 9110 lets a server
/// do that, and it is always a correct answer, where a guess at a multipart
/// byte-range response would not be.
fn parse_range(header: &str, total: u64) -> ByteRange {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return ByteRange::Whole;
    };
    if spec.contains(',') {
        return ByteRange::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return ByteRange::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    let parse = |text: &str| text.parse::<u64>().ok();
    match (first.is_empty(), last.is_empty()) {
        // `-n`: the last n bytes.
        (true, false) => match parse(last) {
            Some(0) => ByteRange::Unsatisfiable,
            Some(_) if total == 0 => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Part {
                start: total.saturating_sub(n),
                end: total - 1,
            },
            None => ByteRange::Whole,
        },
        // `a-`: from a to the end.
        (false, true) => match parse(first) {
            Some(start) if start >= total => ByteRange::Unsatisfiable,
            Some(start) => ByteRange::Part {
                start,
                end: total - 1,
            },
            None => ByteRange::Whole,
        },
        // `a-b`: an explicit span, clamped to the file.
        (false, false) => match (parse(first), parse(last)) {
            (Some(start), Some(end)) if start <= end => {
                if start >= total {
                    ByteRange::Unsatisfiable
                } else {
                    ByteRange::Part {
                        start,
                        end: end.min(total - 1),
                    }
                }
            }
            _ => ByteRange::Whole,
        },
        (true, true) => ByteRange::Whole,
    }
}

/// Bytes read from disk as the response is sent.
const READ_CHUNK: u64 = 64 * 1024;

/// `length` bytes of `file`, from where it is positioned, in chunks of
/// [`READ_CHUNK`].
///
/// 🔴 This was `tokio::fs::read` — the whole file into memory on every request,
/// so a 500 MiB video cost 500 MiB per viewer. A read now holds one chunk.
fn file_chunks(
    file: tokio::fs::File,
    length: u64,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
    futures_util::stream::unfold((file, length), |(mut file, remaining)| async move {
        use tokio::io::AsyncReadExt;
        if remaining == 0 {
            return None;
        }
        let mut buffer = vec![0u8; usize::try_from(remaining.min(READ_CHUNK)).unwrap_or(0)];
        match file.read(&mut buffer).await {
            // Shorter than its record: stop rather than promise bytes that are
            // not there. The client sees a short body against its
            // `Content-Length`, which is the truthful failure.
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                let left = remaining - read as u64;
                Some((Ok(Bytes::from(buffer)), (file, left)))
            }
            Err(err) => Some((Err(err), (file, 0))),
        }
    })
}

/// Browsers may keep a copy, but must ask before using it; shared caches keep none.
const UPLOAD_CACHE_CONTROL: &str = "private, no-cache";

/// Whether an `If-None-Match` header names this entity tag. Weak and strong
/// forms compare equal: the bytes behind an upload id never change.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    if_none_match
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
}

/// Types we are willing to render inline in the app's own origin.
///
/// An **allowlist**, and a short one. The question is not "is this type
/// dangerous" — that list is unbounded and grows with every browser release —
/// but "do we positively know this renders as data". `image/svg+xml` is
/// deliberately **absent**: an SVG is a document that can carry script, and
/// serving one inline from the app's origin is stored XSS with extra steps.
fn renders_inline_safely(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "application/pdf"
            | "video/mp4"
            | "video/webm"
            | "audio/mpeg"
            | "audio/ogg"
            | "text/plain"
    )
}

/// Re-encode a decoded multipart form as `application/x-www-form-urlencoded`.
///
/// What makes the pre-stage a *normaliser* rather than a fork: after this, every
/// downstream step — CSRF, the return path, the action gate, the handler — is
/// the same code that runs for a body that was urlencoded to begin with. A file
/// field's value is its id, which is 64 hex characters, so the re-encoded body
/// is small however large the files were.
#[must_use]
pub fn to_urlencoded(form: &MultipartForm) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in &form.fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// Turn an upload refusal into a response.
///
/// `413` for the three size bounds, because that is the status a browser and a
/// client library both already understand to mean "smaller". `400` for the
/// rest — a bucket that does not exist or a type that is not accepted is a
/// malformed request against *this* app, not a server fault. `500` only for a
/// filesystem failure, which is the one case that is genuinely ours.
#[must_use]
pub fn refusal(error: &UploadError) -> axum::response::Response {
    use axum::http::StatusCode;
    let status = match error {
        UploadError::FileTooLarge { .. }
        | UploadError::BodyTooLarge
        | UploadError::FieldTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        UploadError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        UploadError::SignInRequired { .. } => StatusCode::UNAUTHORIZED,
        // What the action's own CSRF refusal answers.
        UploadError::Forged => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    if let UploadError::Io { reason } = error {
        // The only variant whose text is ours rather than the caller's, and the
        // only one that could carry a path. Logged, never sent.
        warn!(target: "albedo.uploads", %reason, "an upload could not be stored");
    }
    axum::response::IntoResponse::into_response((
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        error.to_string(),
    ))
}

/// The boundary from a `Content-Type`, or `None` if this is not multipart.
#[must_use]
pub fn boundary_of(content_type: Option<&str>) -> Option<String> {
    multer::parse_boundary(content_type?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::upload::UploadDecl;

    /// The three forms a media element sends, clamped to the file; anything
    /// else served whole, which is always a correct answer.
    #[test]
    fn a_range_header_is_read_as_one_span_or_ignored() {
        let part = |start, end| ByteRange::Part { start, end };
        assert_eq!(parse_range("bytes=0-3", 10), part(0, 3));
        assert_eq!(parse_range("bytes=4-", 10), part(4, 9));
        assert_eq!(parse_range("bytes=-3", 10), part(7, 9));
        assert_eq!(parse_range("bytes=-30", 10), part(0, 9), "a suffix past the start is the whole file");
        assert_eq!(parse_range("bytes=8-200", 10), part(8, 9), "the end is clamped");
        assert_eq!(parse_range("bytes=10-", 10), ByteRange::Unsatisfiable);
        assert_eq!(parse_range("bytes=12-20", 10), ByteRange::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), ByteRange::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), ByteRange::Unsatisfiable);
        for ignored in ["bytes=0-1,4-5", "items=0-3", "bytes=5-2", "bytes=x-y", "bytes=-", "0-3"] {
            assert_eq!(parse_range(ignored, 10), ByteRange::Whole, "{ignored}");
        }
    }

    fn registry(json: serde_json::Value) -> UploadRegistry {
        UploadRegistry::from_declarations(
            &serde_json::from_value::<BTreeMap<String, UploadDecl>>(json).expect("parses"),
        )
        .expect("lowers")
    }

    /// Build a multipart body by hand, so the test is over real wire bytes and
    /// not over a mock of the parser.
    fn body(boundary: &str, parts: &[(&str, Option<&str>, &str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, file_name, content_type, bytes) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match file_name {
                Some(file_name) => out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; \
                         filename=\"{file_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    fn stream(bytes: Vec<u8>) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
    {
        // Deliberately chunked small, so the streaming path is actually
        // exercised rather than handed one buffer that hides every boundary
        // bug in the read loop.
        futures_util::stream::iter(
            bytes
                .chunks(7)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )
    }

    async fn decode(
        registry: &UploadRegistry,
        dir: &Path,
        parts: &[(&str, Option<&str>, &str, &[u8])],
    ) -> Result<MultipartForm, UploadError> {
        let boundary = "albedotestboundary";
        decode_multipart(
            stream(body(boundary, parts)),
            boundary,
            &UploadContext {
                registry,
                data_dir: dir,
                principal: Some("u_test".to_string()),
                genuine: Genuine::Yes,
            },
        )
        .await
    }

    /// A6 · a file before the token is refused with nothing written, a wrong
    /// token is refused, and the token first — the order real forms send — is
    /// stored.
    #[tokio::test]
    async fn no_file_is_written_before_the_submit_shows_a_valid_token() {
        let registry = registry(serde_json::json!({ "avatars": {} }));
        let boundary = "albedotestboundary";
        let check = |token: &str| token == "good";
        let attempt = |dir: std::path::PathBuf, parts: Vec<(&'static str, Option<&'static str>, &'static str, &'static [u8])>| {
            let registry = &registry;
            async move {
                decode_multipart(
                    stream(body(boundary, &parts)),
                    boundary,
                    &UploadContext {
                        registry,
                        data_dir: &dir,
                        principal: Some("u_test".to_string()),
                        genuine: Genuine::Pending(&check),
                    },
                )
                .await
            }
        };
        let file = ("avatars", Some("a.bin"), "application/octet-stream", &b"forged bytes"[..]);
        let stored = |dir: &Path| {
            store::path_for(dir, &format!("{:x}", Sha256::digest(b"forged bytes")))
                .unwrap()
                .exists()
        };

        let temp = tempfile::tempdir().unwrap();
        let before = attempt(temp.path().to_path_buf(), vec![file, ("_csrf", None, "", b"good")]).await;
        assert_eq!(before.unwrap_err(), UploadError::Forged);
        assert!(!stored(temp.path()), "a file before the token reached disk");

        let wrong = attempt(temp.path().to_path_buf(), vec![("_csrf", None, "", b"bad"), file]).await;
        assert_eq!(wrong.unwrap_err(), UploadError::Forged);
        assert!(!stored(temp.path()));

        let first = attempt(temp.path().to_path_buf(), vec![("_csrf", None, "", b"good"), file]).await;
        assert!(first.is_ok(), "{first:?}");
        assert!(stored(temp.path()));
    }

    #[tokio::test]
    async fn a_file_field_decodes_to_its_id_and_the_bytes_land_on_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "avatars": { "accept": ["image/png"] } }));

        let form = decode(
            &registry,
            temp.path(),
            &[
                ("caption", None, "", b"a photo of a cat"),
                ("avatars", Some("cat.png"), "image/png", b"PNG-BYTES"),
            ],
        )
        .await
        .expect("decodes");

        assert_eq!(form.get("caption"), Some("a photo of a cat"));

        // The id is the SHA-256 of the contents, computed while streaming.
        let expected = format!("{:x}", Sha256::digest(b"PNG-BYTES"));
        assert_eq!(form.get("avatars"), Some(expected.as_str()));

        let path = store::path_for(temp.path(), &expected).expect("a real id");
        assert_eq!(
            tokio::fs::read(&path).await.expect("the file exists"),
            b"PNG-BYTES",
            "the bytes on disk must be the bytes that were sent"
        );

        let stored = &form.uploads[0];
        assert_eq!(stored.byte_size, 9);
        assert_eq!(stored.original_name, "cat.png");
        assert_eq!(stored.bucket, "avatars");
        assert_eq!(stored.principal.as_deref(), Some("u_test"));
    }

    /// The same bytes twice is one file. This is what makes the id a
    /// trustworthy `ETag` on the serve path.
    #[tokio::test]
    async fn the_same_bytes_twice_are_one_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "avatars": {} }));
        let parts: &[(&str, Option<&str>, &str, &[u8])] =
            &[("avatars", Some("a.bin"), "application/octet-stream", b"same")];

        let first = decode(&registry, temp.path(), parts).await.expect("decodes");
        let second = decode(&registry, temp.path(), parts).await.expect("decodes");
        assert_eq!(first.get("avatars"), second.get("avatars"));

        let mut count = 0;
        let mut dirs = tokio::fs::read_dir(temp.path().join(store::UPLOAD_DIR))
            .await
            .expect("upload dir");
        while let Ok(Some(entry)) = dirs.next_entry().await {
            if entry.file_name() == TMP_DIR {
                continue;
            }
            let mut files = tokio::fs::read_dir(entry.path()).await.expect("fan-out dir");
            while let Ok(Some(_)) = files.next_entry().await {
                count += 1;
            }
        }
        assert_eq!(count, 1, "content-addressed means one file, not two");
    }

    /// A bucket nobody declared has no bound, so it is refused **before** the
    /// part is read rather than after.
    #[tokio::test]
    async fn a_file_for_an_undeclared_bucket_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "avatars": {} }));
        let error = decode(
            &registry,
            temp.path(),
            &[("trophies", Some("x.bin"), "application/octet-stream", b"x")],
        )
        .await
        .expect_err("refused");
        assert_eq!(
            error,
            UploadError::UnknownBucket {
                bucket: "trophies".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_type_outside_the_accept_list_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "avatars": { "accept": ["image/*"] } }));
        let error = decode(
            &registry,
            temp.path(),
            &[("avatars", Some("evil.html"), "text/html", b"<script>")],
        )
        .await
        .expect_err("refused");
        assert!(matches!(error, UploadError::TypeRefused { .. }));
    }

    /// The bound that matters: it must fire *while reading*, and it must not
    /// leave the partial file behind.
    #[tokio::test]
    async fn an_oversized_file_is_refused_and_leaves_nothing_on_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "tiny": { "maxBytes": "16" } }));
        let error = decode(
            &registry,
            temp.path(),
            &[("tiny", Some("big.bin"), "application/octet-stream", &[b'x'; 4096])],
        )
        .await
        .expect_err("refused");
        assert!(matches!(error, UploadError::FileTooLarge { limit: 16, .. }));

        // Only the temp directory may remain, and it must be empty.
        let mut dirs = tokio::fs::read_dir(temp.path().join(store::UPLOAD_DIR))
            .await
            .expect("upload dir");
        while let Ok(Some(entry)) = dirs.next_entry().await {
            let mut inner = tokio::fs::read_dir(entry.path()).await.expect("read");
            let leftover = inner.next_entry().await.expect("read").is_some();
            assert!(
                !leftover,
                "a refused upload left bytes at {}",
                entry.path().display()
            );
        }
    }

    /// An app with no `uploads` block has nothing to enforce a bound with, and
    /// "undeclared" must not mean "unlimited".
    #[tokio::test]
    async fn an_app_with_no_declaration_accepts_no_files_at_all() {
        let temp = tempfile::tempdir().expect("tempdir");
        let error = decode(
            &UploadRegistry::default(),
            temp.path(),
            &[("anything", Some("x.bin"), "application/octet-stream", b"x")],
        )
        .await
        .expect_err("refused");
        assert_eq!(error, UploadError::NoBucketsDeclared);
    }

    #[tokio::test]
    async fn more_files_than_the_cap_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "files": {} }));
        let mut parts: Vec<(&str, Option<&str>, &str, &[u8])> = Vec::new();
        let names: Vec<String> = (0..=MAX_FILE_PARTS).map(|i| format!("f{i}.bin")).collect();
        let bodies: Vec<Vec<u8>> = (0..=MAX_FILE_PARTS).map(|i| vec![b'a' + i as u8]).collect();
        for i in 0..=MAX_FILE_PARTS {
            parts.push((
                "files",
                Some(names[i].as_str()),
                "application/octet-stream",
                bodies[i].as_slice(),
            ));
        }
        let error = decode(&registry, temp.path(), &parts)
            .await
            .expect_err("refused");
        assert_eq!(error, UploadError::TooManyFiles);
    }

    /// A caption is not a file and must not inherit a file's allowance.
    #[tokio::test]
    async fn a_text_field_is_bounded_by_the_form_cap_not_the_file_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(serde_json::json!({ "big": { "maxBytes": "10mb" } }));
        let oversized = vec![b'x'; crate::forms::MAX_FORM_BODY_BYTES + 1];
        let error = decode(
            &registry,
            temp.path(),
            &[("caption", None, "", oversized.as_slice())],
        )
        .await
        .expect_err("refused");
        assert_eq!(
            error,
            UploadError::FieldTooLarge {
                field: "caption".to_string()
            }
        );
    }

    #[test]
    fn a_boundary_is_read_from_the_content_type_or_is_absent() {
        assert_eq!(
            boundary_of(Some("multipart/form-data; boundary=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(boundary_of(Some("application/x-www-form-urlencoded")), None);
        assert_eq!(boundary_of(None), None);
    }
}
