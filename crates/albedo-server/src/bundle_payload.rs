//! One file that is both the runtime and the app.
//!
//! `albedo ship --binary` writes a copy of the running `albedo` executable
//! with the project appended to it. The result is a single file: copy it to a
//! box, run it, and the site is up — no source tree, no `node_modules`, no
//! `.albedo/dist` beside it.
//!
//! # Why appended rather than compiled in
//!
//! `include_str!` embeds at **compile** time, so embedding a *user's* app that
//! way would mean recompiling `albedo` on the user's machine — a Rust
//! toolchain, a linker, and minutes of build for every deploy. Appending needs
//! none of that: the shipped file is the same prebuilt runtime with a payload
//! stapled to the end, produced in milliseconds on any machine.
//!
//! Both PE (Windows) and ELF (Linux) locate their sections by offsets recorded
//! in the header, so bytes after the last section are ignored by the loader.
//! The executable still runs; the trailer is simply data neither format looks
//! at. This is how self-extracting archives have always worked.
//!
//! # Layout
//!
//! ```text
//! [ the albedo executable, byte for byte ]
//! [ file blob ][ file blob ]…            ← contents, back to back
//! [ index ]                               ← JSON: path, offset, len
//! [ trailer: MAGIC | index_offset | index_len ]   ← fixed 24 bytes, at EOF
//! ```
//!
//! The trailer is last because that is the only position findable without
//! parsing the executable format: seek 24 bytes back from the end and look.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// Marks a payload. Deliberately not a substring of anything the compiler
/// emits, so a scan of an ordinary binary cannot produce a false positive.
pub const PAYLOAD_MAGIC: &[u8; 8] = b"ALBEDOv1";

/// `MAGIC` + `index_offset: u64` + `index_len: u64`, little-endian.
pub const TRAILER_LEN: u64 = 8 + 8 + 8;

/// Where one file lives inside the shipped binary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PayloadEntry {
    /// Project-relative path, forward slashes. The same spelling rule as
    /// `module_path` in the manifest, and for the same reason: this string is
    /// read on a machine that is not the one that wrote it.
    pub path: String,
    /// Absolute offset in the shipped file.
    pub offset: u64,
    /// Length in bytes.
    pub len: u64,
}

/// The index of everything appended.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PayloadIndex {
    /// The build this payload came from, for cache keying and for saying out
    /// loud which build is running.
    pub build_id: String,
    pub entries: Vec<PayloadEntry>,
}

impl PayloadIndex {
    /// Total bytes of file content, excluding the index and trailer.
    #[must_use]
    pub fn content_len(&self) -> u64 {
        self.entries.iter().map(|entry| entry.len).sum()
    }
}

/// Reject a path that would escape the directory it is extracted into.
///
/// 🔴 **This is the zip-slip check and it is not optional.** An entry named
/// `../../.ssh/authorized_keys` extracted with a naive `root.join(path)`
/// writes outside the root. The payload is produced by `albedo ship` here and
/// now, but the file it produces travels, and a shipped binary is exactly the
/// kind of artifact someone else hands you. Validated on **write and read**,
/// because only the read side runs on the machine that would be harmed.
///
/// # Errors
/// The offending path, when it is absolute, escapes upward, or carries a
/// Windows drive prefix.
pub fn validate_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("a payload entry has an empty path".to_string());
    }
    if path.contains('\\') {
        return Err(format!(
            "payload entry '{path}' contains a backslash; entries are stored with forward slashes \
             so they resolve on any platform"
        ));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(format!("payload entry '{path}' is an absolute path"));
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "payload entry '{path}' escapes the extraction directory with `..`"
                ))
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("payload entry '{path}' is rooted or has a drive prefix"))
            }
        }
    }
    Ok(())
}

/// Join `path` onto `root`, refusing anything that would land outside it.
///
/// # Errors
/// The offending path. See [`validate_entry_path`].
pub fn safe_join(root: &Path, path: &str) -> Result<PathBuf, String> {
    validate_entry_path(path)?;
    Ok(root.join(path))
}

/// Write `host` followed by every file in `files`, then the index and trailer.
///
/// `files` is `(project-relative path, absolute source path)`.
///
/// # Errors
/// An IO failure, or a path that fails [`validate_entry_path`].
pub fn write_payload<W: Write + Seek>(
    out: &mut W,
    host: &mut dyn Read,
    build_id: &str,
    files: &[(String, PathBuf)],
) -> Result<PayloadIndex, String> {
    let host_len = std::io::copy(host, out)
        .map_err(|err| format!("failed to copy the albedo runtime into the output: {err}"))?;

    let mut offset = host_len;
    let mut entries = Vec::with_capacity(files.len());

    for (path, source) in files {
        validate_entry_path(path)?;
        let bytes = std::fs::read(source)
            .map_err(|err| format!("failed to read '{}': {err}", source.display()))?;
        out.write_all(&bytes)
            .map_err(|err| format!("failed to append '{path}': {err}"))?;
        let len = bytes.len() as u64;
        entries.push(PayloadEntry {
            path: path.clone(),
            offset,
            len,
        });
        offset += len;
    }

    let index = PayloadIndex {
        build_id: build_id.to_string(),
        entries,
    };
    let index_bytes = serde_json::to_vec(&index)
        .map_err(|err| format!("failed to serialise the payload index: {err}"))?;
    let index_offset = offset;

    out.write_all(&index_bytes)
        .map_err(|err| format!("failed to write the payload index: {err}"))?;
    out.write_all(PAYLOAD_MAGIC)
        .map_err(|err| format!("failed to write the payload trailer: {err}"))?;
    out.write_all(&index_offset.to_le_bytes())
        .map_err(|err| format!("failed to write the payload trailer: {err}"))?;
    out.write_all(&(index_bytes.len() as u64).to_le_bytes())
        .map_err(|err| format!("failed to write the payload trailer: {err}"))?;

    Ok(index)
}

/// Read the index from a file that may or may not carry a payload.
///
/// `Ok(None)` means "this is an ordinary executable" — the overwhelmingly
/// common case, and not an error: the same binary has to keep working as a
/// plain CLI.
///
/// # Errors
/// A payload that is present but unreadable. That *is* an error rather than a
/// silent `None`: a truncated shipped binary would otherwise fall back to
/// looking for a source tree that was never copied, and report a missing
/// project instead of a damaged file.
pub fn read_index<R: Read + Seek>(file: &mut R) -> Result<Option<PayloadIndex>, String> {
    let total = file
        .seek(SeekFrom::End(0))
        .map_err(|err| format!("failed to measure the executable: {err}"))?;
    if total < TRAILER_LEN {
        return Ok(None);
    }

    file.seek(SeekFrom::End(-(TRAILER_LEN as i64)))
        .map_err(|err| format!("failed to seek to the payload trailer: {err}"))?;
    let mut trailer = [0u8; TRAILER_LEN as usize];
    file.read_exact(&mut trailer)
        .map_err(|err| format!("failed to read the payload trailer: {err}"))?;

    if &trailer[..8] != PAYLOAD_MAGIC {
        return Ok(None);
    }

    let index_offset = u64::from_le_bytes(
        trailer[8..16]
            .try_into()
            .map_err(|_| "malformed payload trailer".to_string())?,
    );
    let index_len = u64::from_le_bytes(
        trailer[16..24]
            .try_into()
            .map_err(|_| "malformed payload trailer".to_string())?,
    );

    // A trailer that points outside the file is a truncated or corrupted
    // download. Say so rather than seeking into nothing.
    if index_offset
        .checked_add(index_len)
        .is_none_or(|end| end + TRAILER_LEN > total)
    {
        return Err(format!(
            "this binary carries an ALBEDO payload whose index runs past the end of the file \
             (offset {index_offset}, length {index_len}, file {total}). The file is truncated or \
             corrupted — re-copy it."
        ));
    }

    file.seek(SeekFrom::Start(index_offset))
        .map_err(|err| format!("failed to seek to the payload index: {err}"))?;
    let mut raw = vec![0u8; index_len as usize];
    file.read_exact(&mut raw)
        .map_err(|err| format!("failed to read the payload index: {err}"))?;

    let index: PayloadIndex = serde_json::from_slice(&raw)
        .map_err(|err| format!("the payload index is not readable: {err}"))?;
    Ok(Some(index))
}

/// Should this project-relative path travel inside the shipped binary?
///
/// The rule is a denylist over the project directory rather than an allowlist
/// of known directories, because an app's own files — a `fonts/` directory, a
/// `data.json` a component imports — are not enumerable in advance, and a file
/// silently left out reappears as a route that renders nothing.
///
/// 🔑 **`forge.db` is deliberately excluded, and that is a correctness rule
/// rather than a size one.** Shipping it would bake the development database
/// into the artifact: every deploy would ship the developer's rows, two
/// deployments of the same binary would start from identical state, and — the
/// worst of it — the file inside the binary is not writable, so the running app
/// would need a copy anyway. The database belongs beside the executable, which
/// is what [`crate::forge_db_path`] already arranges.
#[must_use]
pub fn should_ship(relative_path: &str) -> bool {
    // Compare on whole path segments: a substring test would drop a legitimate
    // `src/components/target-picker.tsx` for containing "target".
    let segments: Vec<&str> = relative_path.split('/').collect();

    const EXCLUDED_DIRS: &[&str] = &[
        // Taken out of the runtime by `npm_prebuilt` — 58 MB the app no longer
        // reads. Shipping it would undo that whole result.
        "node_modules",
        ".git",
        "target",
        // The incremental build cache: an input to the build, never read while
        // serving.
        ".albedo-cache",
    ];

    if segments
        .iter()
        .any(|segment| EXCLUDED_DIRS.contains(segment))
    {
        return false;
    }

    // 🪤 A shipped binary run inside its own project leaves its unpacked tree
    // beside itself, named for the build. Shipping again would embed a whole
    // copy of the previous build — the app twice, growing every release, and
    // carrying a stale `.albedo/dist` into the new payload. Matched by prefix
    // because the build id is part of the name.
    if segments
        .iter()
        .any(|segment| segment.starts_with(".albedo-app-"))
    {
        return false;
    }

    let Some(name) = segments.last() else {
        return false;
    };

    // The database and its WAL/SHM sidecars. `starts_with` rather than an
    // equality test because libsql writes `forge.db-wal` and `forge.db-shm`
    // beside it, and a shipped WAL is a torn half-transaction.
    if name.starts_with("forge.db") {
        return false;
    }
    // A previously shipped binary sitting in the project would otherwise be
    // appended to the next one, doubling the size every time.
    if name.ends_with(".albedo-bin") || name.ends_with(".albedo-bin.exe") {
        return false;
    }
    // The staging name `ship` writes before its final rename. An interrupted
    // ship leaves one behind, and it is a whole runtime-plus-app.
    if name.ends_with(".albedo-bin.partial") || name.ends_with(".partial") {
        return false;
    }
    // 🪤 The linux/amd64 runtime `ship --target docker` stages in the project
    // (13.4c). Same class as the two above and found the same way — the image's
    // builder stage does `COPY . .`, so without this the ~25 MB runtime lands
    // in the build context AND is then embedded in the payload the runtime
    // itself carries. A whole albedo inside every shipped app.
    //
    // It cannot be handled by `.dockerignore` instead: that would remove it
    // from the context entirely, and the builder's `COPY albedo-linux-amd64`
    // needs it there.
    if *name == "albedo-linux-amd64" {
        return false;
    }

    true
}

/// Walk `project_dir` and list everything [`should_ship`] admits.
///
/// # Errors
/// An IO failure, or a filename that is not valid UTF-8 — the payload index is
/// JSON, so a name that cannot be spelled in it has to be reported rather than
/// silently dropped.
pub fn collect_project_files(project_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut collected = Vec::new();
    let mut stack = vec![project_dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("failed to read '{}': {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("failed to read a directory entry: {err}"))?;
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(project_dir) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !should_ship(&relative) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                collected.push((relative, path));
            }
        }
    }

    // Deterministic order so two ships of one tree are byte-identical — a
    // build that differs run to run cannot be diffed or checksummed.
    collected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(collected)
}

/// Write every entry out under `root`.
///
/// # Errors
/// An IO failure, or an entry that fails [`validate_entry_path`].
pub fn extract_to<R: Read + Seek>(
    file: &mut R,
    index: &PayloadIndex,
    root: &Path,
) -> Result<(), String> {
    for entry in &index.entries {
        let destination = safe_join(root, &entry.path)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create '{}': {err}", parent.display()))?;
        }
        let bytes = read_entry(file, entry)?;
        std::fs::write(&destination, &bytes)
            .map_err(|err| format!("failed to write '{}': {err}", destination.display()))?;
    }
    Ok(())
}

/// The directory a shipped binary unpacks its project into.
///
/// Beside the executable rather than in a temp directory, for three reasons:
/// a temp directory is swept out from under a long-running server; an operator
/// who wants to know what is running should be able to look at it; and the
/// database belongs beside the executable, so keeping the project there too
/// means one directory holds the whole deployment.
///
/// Keyed by `build_id`, so shipping a new binary over an old one unpacks
/// afresh instead of serving a mixture of two builds — the failure that would
/// otherwise be a stale route with no explanation.
#[must_use]
pub fn unpack_dir_for(exe: &Path, build_id: &str) -> PathBuf {
    let parent = exe.parent().unwrap_or_else(|| Path::new("."));
    // The id comes from the manifest, but it lands in a path, so anything that
    // is not plainly a filename is replaced rather than trusted.
    let safe: String = build_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(64)
        .collect();
    parent.join(format!(".albedo-app-{safe}"))
}

/// Unpack the payload if it is not already on disk, and return the project root.
///
/// Extraction is skipped when the directory already carries this build's
/// marker, so a restart costs a single `exists` check rather than rewriting
/// the whole tree.
///
/// # Errors
/// An IO failure, or an entry that fails [`validate_entry_path`].
pub fn prepare_unpacked_project<R: Read + Seek>(
    file: &mut R,
    index: &PayloadIndex,
    exe: &Path,
) -> Result<PathBuf, String> {
    let root = unpack_dir_for(exe, &index.build_id);
    let marker = root.join(".albedo-unpacked");

    // 🪤 The marker is written **last**, so an extraction interrupted halfway
    // leaves no marker and the next run redoes it. A marker written first would
    // make a half-unpacked tree look complete for the life of the deployment.
    if marker.is_file() {
        return Ok(root);
    }

    std::fs::create_dir_all(&root)
        .map_err(|err| format!("failed to create '{}': {err}", root.display()))?;
    extract_to(file, index, &root)?;
    std::fs::write(&marker, index.build_id.as_bytes())
        .map_err(|err| format!("failed to mark '{}' as unpacked: {err}", root.display()))?;

    Ok(root)
}

/// Read one entry's bytes.
///
/// # Errors
/// An IO failure, or an entry whose extent runs past the end of the file.
pub fn read_entry<R: Read + Seek>(file: &mut R, entry: &PayloadEntry) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(entry.offset))
        .map_err(|err| format!("failed to seek to '{}': {err}", entry.path))?;
    let mut bytes = vec![0u8; entry.len as usize];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("failed to read '{}' from the payload: {err}", entry.path))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const HOST: &[u8] = b"\x7fELF-pretend-this-is-a-real-executable";

    fn ship(files: &[(&str, &[u8])]) -> (Vec<u8>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut on_disk = Vec::new();
        for (path, bytes) in files {
            let source = dir.path().join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = source.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(&source, bytes).expect("write source");
            on_disk.push(((*path).to_string(), source));
        }

        let mut out = Cursor::new(Vec::new());
        write_payload(&mut out, &mut Cursor::new(HOST), "build-1", &on_disk).expect("ship");
        (out.into_inner(), dir)
    }

    /// The plain CLI must keep working. A binary nobody shipped has no payload,
    /// and that is a value, not an error.
    #[test]
    fn an_ordinary_executable_reports_no_payload() {
        let mut file = Cursor::new(HOST.to_vec());
        assert_eq!(read_index(&mut file), Ok(None));
    }

    /// Even one shorter than the trailer — an empty or stub file must not make
    /// the reader seek to a negative offset.
    #[test]
    fn a_file_shorter_than_the_trailer_reports_no_payload() {
        let mut file = Cursor::new(b"ab".to_vec());
        assert_eq!(read_index(&mut file), Ok(None));
    }

    /// 🔑 The property the whole design rests on: the runtime's own bytes are
    /// untouched and still first, so the shipped file is still an executable
    /// the OS will load.
    #[test]
    fn the_runtime_bytes_are_preserved_byte_for_byte_at_the_front() {
        let (shipped, _dir) = ship(&[("src/App.tsx", b"export default () => null")]);
        assert_eq!(&shipped[..HOST.len()], HOST);
        assert!(shipped.len() > HOST.len(), "a payload was appended");
    }

    /// Every file comes back exactly as it went in.
    #[test]
    fn every_file_round_trips_byte_for_byte() {
        let files: &[(&str, &[u8])] = &[
            ("src/App.tsx", b"export default () => <p>hi</p>"),
            (".albedo/dist/render-manifest.v2.json", b"{\"version\":2}"),
            // A binary asset, to prove nothing assumes UTF-8.
            ("public/logo.png", &[0x89, b'P', b'N', b'G', 0x00, 0xFF, 0x1A]),
        ];
        let (shipped, _dir) = ship(files);

        let mut cursor = Cursor::new(shipped);
        let index = read_index(&mut cursor).expect("readable").expect("present");
        assert_eq!(index.build_id, "build-1");
        assert_eq!(index.entries.len(), files.len());

        for (path, expected) in files {
            let entry = index
                .entries
                .iter()
                .find(|entry| entry.path == *path)
                .unwrap_or_else(|| panic!("{path} is in the index"));
            assert_eq!(
                read_entry(&mut cursor, entry).expect("read"),
                *expected,
                "{path} did not round trip"
            );
        }
    }

    /// An empty file is a legitimate project file and must not break the
    /// offset arithmetic that follows it.
    #[test]
    fn a_zero_length_file_does_not_desynchronise_the_offsets() {
        let files: &[(&str, &[u8])] = &[
            ("empty.txt", b""),
            ("after.txt", b"this must still be found"),
        ];
        let (shipped, _dir) = ship(files);
        let mut cursor = Cursor::new(shipped);
        let index = read_index(&mut cursor).expect("readable").expect("present");
        let after = index
            .entries
            .iter()
            .find(|entry| entry.path == "after.txt")
            .expect("present");
        assert_eq!(read_entry(&mut cursor, after).expect("read"), b"this must still be found");
    }

    /// 🔴 Zip-slip. An entry that climbs out of the extraction root must be
    /// refused, on the machine doing the extracting.
    #[test]
    fn an_entry_that_escapes_the_root_is_refused() {
        let err = validate_entry_path("../../.ssh/authorized_keys").unwrap_err();
        assert!(err.contains(".."), "{err}");
        assert!(safe_join(Path::new("/srv/app"), "../etc/passwd").is_err());
    }

    #[test]
    fn an_absolute_entry_is_refused() {
        assert!(validate_entry_path("/etc/passwd").is_err());
        assert!(validate_entry_path("C:/Windows/System32/drivers/etc/hosts").is_err());
    }

    /// Backslashes are refused rather than translated: an entry written on
    /// Windows as `src\App.tsx` would be one filename on Linux, and silently
    /// accepting it produces a project whose files are all in the root.
    #[test]
    fn a_backslash_entry_is_refused() {
        assert!(validate_entry_path("src\\App.tsx").is_err());
    }

    /// A safe path joins where it should.
    #[test]
    fn an_ordinary_entry_joins_under_the_root() {
        assert_eq!(
            safe_join(Path::new("/srv/app"), "src/App.tsx"),
            Ok(PathBuf::from("/srv/app").join("src/App.tsx"))
        );
    }

    /// 🪤 A truncated copy — an interrupted `scp` — must say the file is
    /// damaged. Falling back to `Ok(None)` would report "no project found"
    /// and send the operator looking for a missing source tree.
    #[test]
    fn a_truncated_payload_is_an_error_not_a_silent_absence() {
        let (mut shipped, _dir) = ship(&[("src/App.tsx", b"export default () => null")]);
        // Cut out the middle, keeping the trailer, so the recorded index
        // offset now points past the end.
        let trailer = shipped.split_off(shipped.len() - TRAILER_LEN as usize);
        shipped.truncate(HOST.len());
        shipped.extend_from_slice(&trailer);

        let mut cursor = Cursor::new(shipped);
        let err = read_index(&mut cursor).unwrap_err();
        assert!(err.contains("truncated or corrupted"), "{err}");
    }

    /// A binary whose tail coincidentally ends in something else is not a
    /// payload — the magic is what decides, not the length.
    #[test]
    fn a_binary_ending_in_arbitrary_bytes_is_not_mistaken_for_a_payload() {
        let mut file = Cursor::new([HOST, &[0u8; 64][..]].concat());
        assert_eq!(read_index(&mut file), Ok(None));
    }

    /// The app's own files travel. The rule is a denylist because an app's
    /// asset directories are not enumerable in advance.
    #[test]
    fn the_project_and_its_build_output_travel() {
        assert!(should_ship("src/App.tsx"));
        assert!(should_ship(".albedo/dist/render-manifest.v2.json"));
        assert!(should_ship(".albedo/dist/_albedo/phosphor.js"));
        assert!(should_ship("public/logo.png"));
        assert!(should_ship("albedo.config.ts"));
        assert!(should_ship("fonts/Inter.woff2"));
    }

    /// The 58 MB that `npm_prebuilt` took out of the runtime must not come
    /// back in through the shipped file.
    #[test]
    fn node_modules_does_not_travel() {
        assert!(!should_ship("node_modules/react/index.js"));
        assert!(!should_ship("src/node_modules/anything.js"));
    }

    /// 🔑 Not a size rule — a correctness one. A baked database ships the
    /// developer's rows, makes every deployment start from identical state,
    /// and is not writable where it lands.
    #[test]
    fn the_database_does_not_travel() {
        assert!(!should_ship("forge.db"));
        assert!(!should_ship("forge.db-wal"));
        assert!(!should_ship("forge.db-shm"));
    }

    /// 🪤 Whole segments, not substrings: a component legitimately named
    /// `target-picker.tsx` lives in a directory that must still ship.
    #[test]
    fn a_filename_that_merely_contains_an_excluded_word_still_travels() {
        assert!(should_ship("src/components/target-picker.tsx"));
        assert!(should_ship("src/node_modules_helper.ts"));
        assert!(!should_ship("target/debug/albedo.exe"));
    }

    /// Shipping twice in one directory must not append the first result to the
    /// second, doubling the size on every run.
    #[test]
    fn a_previously_shipped_binary_does_not_travel() {
        assert!(!should_ship("myapp.albedo-bin"));
        assert!(!should_ship("myapp.albedo-bin.exe"));
        // 🪤 An interrupted ship leaves its staging file behind, and that file
        // is itself a whole runtime plus app.
        assert!(!should_ship("myapp.albedo-bin.partial"));
    }

    /// 🪤 A shipped binary run inside its own project leaves its unpacked tree
    /// beside itself. Shipping again would then embed a *complete copy of the
    /// previous build* — the app twice, growing every release, and with a
    /// stale `.albedo/dist` inside the new payload.
    #[test]
    fn a_previously_unpacked_project_does_not_travel() {
        assert!(!should_ship(".albedo-app-6cae2768a239ce6d/.albedo/dist/render-manifest.v2.json"));
        assert!(!should_ship(".albedo-app-6cae2768a239ce6d/src/App.tsx"));
        assert!(!should_ship(".albedo-app-6cae2768a239ce6d/.albedo-unpacked"));
    }

    /// Collection walks the tree, applies the rule, and returns a stable order
    /// so two ships of one tree produce identical bytes.
    #[test]
    fn collection_is_deterministic_and_applies_the_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        for path in [
            "src/App.tsx",
            "src/components/Button.tsx",
            ".albedo/dist/render-manifest.v2.json",
            "node_modules/react/index.js",
            "forge.db",
        ] {
            let full = dir.path().join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(&full, b"x").expect("write");
        }

        let collected = collect_project_files(dir.path()).expect("collect");
        let names: Vec<&str> = collected.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            names,
            vec![
                ".albedo/dist/render-manifest.v2.json",
                "src/App.tsx",
                "src/components/Button.tsx",
            ],
            "node_modules and forge.db must be absent, and the order sorted"
        );
    }

    /// The whole point, end to end at the module level: ship a tree, extract
    /// it somewhere else, and every file is where it should be with the bytes
    /// it had.
    #[test]
    fn a_shipped_tree_extracts_to_an_identical_tree() {
        let files: &[(&str, &[u8])] = &[
            ("src/App.tsx", b"export default () => <p>hi</p>"),
            (".albedo/dist/render-manifest.v2.json", b"{\"version\":2}"),
            ("public/logo.png", &[0x89, b'P', b'N', b'G', 0x00]),
        ];
        let (shipped, _source) = ship(files);

        let target = tempfile::tempdir().expect("tempdir");
        let mut cursor = Cursor::new(shipped);
        let index = read_index(&mut cursor).expect("readable").expect("present");
        extract_to(&mut cursor, &index, target.path()).expect("extract");

        for (path, expected) in files {
            let landed = target.path().join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            assert_eq!(
                std::fs::read(&landed).unwrap_or_else(|_| panic!("{path} was extracted")),
                *expected,
                "{path} differs after extraction"
            );
        }
    }

    /// 🔴 Zip-slip, at the point it would actually do harm: extraction.
    #[test]
    fn extraction_refuses_an_entry_that_escapes_the_root() {
        let index = PayloadIndex {
            build_id: "evil".to_string(),
            entries: vec![PayloadEntry {
                path: "../escaped.txt".to_string(),
                offset: 0,
                len: 0,
            }],
        };
        let target = tempfile::tempdir().expect("tempdir");
        let mut cursor = Cursor::new(Vec::new());
        let err = extract_to(&mut cursor, &index, target.path()).unwrap_err();
        assert!(err.contains(".."), "{err}");
        assert!(
            !target.path().parent().expect("parent").join("escaped.txt").exists(),
            "nothing may be written outside the extraction root"
        );
    }
}
