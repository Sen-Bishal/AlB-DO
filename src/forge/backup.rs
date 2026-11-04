//! Backups — a snapshot of the data directory, checked as it is taken, and a
//! restore the server applies at its next start.
//!
//! ## A snapshot is a data directory
//!
//! `<backups>/2026-10-30T12-00-00Z/` holds `forge.db`, the `uploads/ab/abcd…`
//! files its records point at, and `manifest.json` — the layout of the
//! directory the server runs from, so one set of path rules
//! ([`path_for`]) covers both, and a snapshot goes offsite as one folder.
//!
//! ## What makes it consistent
//!
//! The database is copied by [`LibSqlSubstrate::snapshot_into`]: `VACUUM INTO`,
//! one instant, while the server keeps serving. The uploads are chosen by *the
//! snapshot's* records, read after it, so the files match the rows exactly — a
//! file uploaded a moment later is referenced by nothing in the copy and is not
//! taken.
//!
//! ## Why uploads cost almost nothing after the first snapshot
//!
//! An upload is named by the SHA-256 of its bytes, so a file the previous
//! snapshot holds under the same name *is* this file. It is hard-linked, not
//! copied: every snapshot stays self-contained, a second snapshot of an
//! unchanged store costs only its database, and retention is deleting a
//! directory — the filesystem counts the links. A file copied fresh is hashed
//! as it is copied; one whose bytes no longer match its name is reported as
//! damaged, not backed up.
//!
//! ## Not in a backup
//!
//! Secrets. Keys come from the environment, and backups travel to places a key
//! should not. Nor the ACME cache: a certificate can be issued again; a private
//! key in a backup cannot be un-leaked.
//!
//! ## Restore is staged, then applied at boot
//!
//! A running server holds the database open and its live state in memory.
//! Replacing the file under it is refused on Windows and, worse, *accepted* on
//! Unix: the server keeps writing to the unlinked file and the restore is
//! silently forked. So [`stage_restore`] checks everything — the manifest's
//! hash, SQLite's integrity check, and the app's schema through the boot's own
//! `evolve_schema` — into `forge.db.restore-pending`, and
//! [`apply_staged_restore`] swaps it in at the next boot, before the database
//! is opened. The same two steps everywhere, stage then restart, including a
//! container whose server is PID 1. The replaced database is moved aside, never
//! deleted.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::forge::drift::VerifyError;
use crate::forge::libsql::LibSqlSubstrate;
use crate::forge::skeleton::ForgeSchema;
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::SqlValue;
use crate::upload::store::{path_for, UPLOADS};

/// What a snapshot says about itself.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The database inside a snapshot.
pub const SNAPSHOT_DB: &str = "forge.db";

/// Bumped when a manifest stops meaning what an older reader thinks it means.
const FORMAT: u32 = 1;

/// A snapshot still being written. Ignored by [`list`]; [`prune`] clears it
/// once it is clearly abandoned.
const PARTIAL: &str = ".partial";

/// Snapshot names: sortable, and free of `:` so Windows accepts them.
const NAME_FORMAT: &str = "%Y-%m-%dT%H-%M-%SZ";

/// How old an abandoned `.partial` must be before [`prune`] removes it — long
/// enough that one another process is writing right now is left alone.
const ABANDONED_AFTER: Duration = Duration::from_secs(60 * 60);

/// Read and copy in pieces this size, so no file is held in memory whole.
const CHUNK: usize = 64 * 1024;

/// `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// See [`FORMAT`].
    pub format: u32,
    /// The albedo version that took it.
    pub albedo: String,
    /// RFC 3339, UTC.
    pub taken_at: String,
    /// The same instant, for ordering.
    pub taken_at_ms: i64,
    /// The snapshot's `forge.db`.
    pub database: DatabaseFacts,
    /// Rows per table, read back out of the snapshot.
    pub tables: BTreeMap<String, u64>,
    /// The upload files the snapshot's records point at.
    pub uploads: UploadFacts,
}

impl Manifest {
    /// Rows across every table.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.tables.values().sum()
    }
}

/// The snapshot's database file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseFacts {
    /// Hex SHA-256 of the file, checked again before any restore.
    pub sha256: String,
    /// Its size.
    pub bytes: u64,
}

/// What happened to each upload the snapshot references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadFacts {
    /// Files in the snapshot.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
    /// Of `files`, how many were hard-linked from the previous snapshot.
    pub linked: u64,
    /// Referenced by a record but absent from the live store. The damage
    /// predates the backup; it is named so it is not discovered at restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// Present, but its bytes no longer hash to its name. Not backed up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub damaged: Vec<String>,
    /// The previous snapshot's copy no longer hashed to its name, so this one
    /// was copied fresh from the live store instead of linked. The earlier
    /// snapshot still holds the damaged copy — the backup volume is losing
    /// bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier_damaged: Vec<String>,
}

/// A finished snapshot on disk.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// `<backups>/<name>`.
    pub dir: PathBuf,
    /// Its `manifest.json`.
    pub manifest: Manifest,
}

/// Why a backup or a restore did not happen.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// The filesystem refused.
    #[error("{0}")]
    Io(String),
    /// The database refused.
    #[error("{0}")]
    Database(String),
    /// The snapshot is not one to restore, and the message says why.
    #[error("{0}")]
    Refused(String),
}

fn io(context: impl std::fmt::Display) -> impl FnOnce(std::io::Error) -> BackupError {
    move |err| BackupError::Io(format!("{context}: {err}"))
}

/// Snapshot the database behind `substrate` and the uploads under `data_dir`
/// into a new directory under `backups_dir`.
///
/// Written as `<name>.partial` and renamed when complete, so a snapshot either
/// exists whole or not at all; a crash part-way leaves a `.partial` that
/// [`list`] ignores.
///
/// # Errors
/// [`BackupError`] naming the step that failed. Nothing half-written is left
/// under the snapshot's final name.
pub async fn take(
    substrate: &LibSqlSubstrate,
    data_dir: &Path,
    backups_dir: &Path,
) -> Result<Snapshot, BackupError> {
    std::fs::create_dir_all(backups_dir)
        .map_err(io(format!("could not create {}", backups_dir.display())))?;
    let now = chrono::Utc::now();
    let (name, partial) = claim_partial(backups_dir, now)?;
    let previous = list(backups_dir).into_iter().next().map(|snapshot| snapshot.dir);

    let filled = fill(substrate, data_dir, &partial, previous, now).await;
    let manifest = match filled {
        Ok(manifest) => manifest,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(err);
        }
    };
    let dir = backups_dir.join(&name);
    std::fs::rename(&partial, &dir).map_err(io(format!(
        "could not finish {} (the snapshot is complete in {})",
        dir.display(),
        partial.display()
    )))?;
    Ok(Snapshot { dir, manifest })
}

/// Reserve a fresh `<name>.partial` directory.
fn claim_partial(
    backups_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(String, PathBuf), BackupError> {
    let base = now.format(NAME_FORMAT).to_string();
    for n in 1..100 {
        let name = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        if backups_dir.join(&name).exists() {
            continue;
        }
        let partial = backups_dir.join(format!("{name}{PARTIAL}"));
        // `create_dir`, not `_all`: it fails on an existing directory, which is
        // what keeps two backups started in the same second apart.
        match std::fs::create_dir(&partial) {
            Ok(()) => return Ok((name, partial)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(io(format!("could not create {}", partial.display()))(err)),
        }
    }
    Err(BackupError::Io(format!(
        "no free snapshot name for {base} in {}",
        backups_dir.display()
    )))
}

async fn fill(
    substrate: &LibSqlSubstrate,
    data_dir: &Path,
    partial: &Path,
    previous: Option<PathBuf>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Manifest, BackupError> {
    let db_file = partial.join(SNAPSHOT_DB);
    let facts = substrate
        .snapshot_into(&db_file)
        .await
        .map_err(|err| BackupError::Database(format!("the database snapshot failed: {err}")))?;

    // Hashing and copying files is blocking work, kept off the async workers
    // that are serving requests while this runs.
    let data_dir = data_dir.to_path_buf();
    let partial = partial.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (sha256, bytes) = hash_file(&db_file)?;
        let uploads = copy_uploads(&data_dir, &partial, previous.as_deref(), &facts.upload_ids)?;
        let manifest = Manifest {
            format: FORMAT,
            albedo: env!("CARGO_PKG_VERSION").to_string(),
            taken_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            taken_at_ms: now.timestamp_millis(),
            database: DatabaseFacts { sha256, bytes },
            tables: facts.tables,
            uploads,
        };
        write_json(&partial.join(MANIFEST_FILE), &manifest)?;
        Ok(manifest)
    })
    .await
    .map_err(|err| BackupError::Io(format!("the backup task stopped: {err}")))?
}

fn copy_uploads(
    data_dir: &Path,
    snapshot: &Path,
    previous: Option<&Path>,
    ids: &[String],
) -> Result<UploadFacts, BackupError> {
    let mut facts = UploadFacts::default();
    for id in ids {
        // A record holding something that is not an id names no file.
        let (Ok(source), Ok(dest)) = (path_for(data_dir, id), path_for(snapshot, id)) else {
            facts.damaged.push(id.clone());
            continue;
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io(format!("could not create {}", parent.display())))?;
        }
        // 🔴 Linked only once its bytes are checked. A name is a claim about
        // content, not proof of it: an earlier copy that rotted was once
        // linked unread into every later snapshot and certified good each
        // time, while retention deleted the alternatives. Reading costs I/O;
        // the link still saves the space, which is what it is for.
        if let Some(earlier) = previous
            .and_then(|dir| path_for(dir, id).ok())
            .filter(|earlier| earlier.is_file())
        {
            match hash_file(&earlier) {
                Ok((hash, bytes)) if hash == *id => {
                    if std::fs::hard_link(&earlier, &dest).is_ok() {
                        facts.files += 1;
                        facts.linked += 1;
                        facts.bytes += bytes;
                        continue;
                    }
                }
                Ok(_) => facts.earlier_damaged.push(id.clone()),
                // Unreadable is not proof of damage; the live copy decides.
                Err(_) => {}
            }
        }
        match copy_verified(&source, &dest, id)? {
            Copied::Bytes(n) => {
                facts.files += 1;
                facts.bytes += n;
            }
            Copied::Missing => facts.missing.push(id.clone()),
            Copied::Damaged => facts.damaged.push(id.clone()),
        }
    }
    Ok(facts)
}

enum Copied {
    Bytes(u64),
    Missing,
    Damaged,
}

/// Copy `source` to `dest` through `dest.part`, keeping it only if its bytes
/// still hash to `id`.
fn copy_verified(source: &Path, dest: &Path, id: &str) -> Result<Copied, BackupError> {
    let mut input = match std::fs::File::open(source) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Copied::Missing),
        Err(err) => return Err(io(format!("could not read {}", source.display()))(err)),
    };
    let part = sibling(dest, ".part");
    let (hash, bytes) = {
        let mut output = std::fs::File::create(&part)
            .map_err(io(format!("could not create {}", part.display())))?;
        let copied = pump(&mut input, Some(&mut output))
            .map_err(io(format!("could not copy {}", source.display())));
        match copied {
            Ok(done) => done,
            Err(err) => {
                let _ = std::fs::remove_file(&part);
                return Err(err);
            }
        }
    };
    if hash != id {
        let _ = std::fs::remove_file(&part);
        return Ok(Copied::Damaged);
    }
    std::fs::rename(&part, dest).map_err(io(format!("could not finish {}", dest.display())))?;
    Ok(Copied::Bytes(bytes))
}

/// Read `input` to the end, hashing it and writing it on to `output` if given.
fn pump(input: &mut impl Read, mut output: Option<&mut std::fs::File>) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buffer[..n])?;
        }
        total += n as u64;
    }
    if let Some(out) = output {
        out.sync_all()?;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

fn hash_file(path: &Path) -> Result<(String, u64), BackupError> {
    let mut file =
        std::fs::File::open(path).map_err(io(format!("could not read {}", path.display())))?;
    pump(&mut file, None).map_err(io(format!("could not read {}", path.display())))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), BackupError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| BackupError::Io(format!("could not encode {}: {err}", path.display())))?;
    let tmp = sibling(path, ".tmp");
    std::fs::write(&tmp, text).map_err(io(format!("could not write {}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(io(format!("could not write {}", path.display())))
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Read a snapshot's manifest.
///
/// # Errors
/// [`BackupError::Refused`] when there is no manifest, it does not parse, or a
/// newer albedo wrote it.
pub fn read_manifest(snapshot: &Path) -> Result<Manifest, BackupError> {
    let path = snapshot.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        BackupError::Refused(format!(
            "{} is not a backup albedo took ({} — {err})",
            snapshot.display(),
            path.display()
        ))
    })?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|err| BackupError::Refused(format!("{} does not parse: {err}", path.display())))?;
    if manifest.format != FORMAT {
        return Err(BackupError::Refused(format!(
            "{} is format {}, written by albedo {}; this albedo reads format {FORMAT}",
            path.display(),
            manifest.format,
            manifest.albedo
        )));
    }
    Ok(manifest)
}

/// Every finished snapshot under `backups_dir`, newest first.
///
/// Only directories with a readable manifest count. Anything else in the
/// directory is not ours and is neither listed nor pruned.
#[must_use]
pub fn list(backups_dir: &Path) -> Vec<Snapshot> {
    let Ok(entries) = std::fs::read_dir(backups_dir) else {
        return Vec::new();
    };
    let mut snapshots: Vec<Snapshot> = entries
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            let partial = dir.extension().is_some_and(|ext| ext == &PARTIAL[1..]);
            if partial || !dir.is_dir() {
                return None;
            }
            let manifest = read_manifest(&dir).ok()?;
            Some(Snapshot { dir, manifest })
        })
        .collect();
    snapshots.sort_by(|a, b| {
        b.manifest
            .taken_at_ms
            .cmp(&a.manifest.taken_at_ms)
            .then_with(|| b.dir.cmp(&a.dir))
    });
    snapshots
}

/// Keep the newest `keep` snapshots (at least one) and delete the rest, plus
/// any `.partial` abandoned for an hour.
///
/// # Errors
/// [`BackupError::Io`] if a directory cannot be removed; ones removed before
/// it stay removed.
pub fn prune(backups_dir: &Path, keep: usize) -> Result<Vec<PathBuf>, BackupError> {
    let mut removed = Vec::new();
    for snapshot in list(backups_dir).into_iter().skip(keep.max(1)) {
        std::fs::remove_dir_all(&snapshot.dir)
            .map_err(io(format!("could not remove {}", snapshot.dir.display())))?;
        removed.push(snapshot.dir);
    }
    if let Ok(entries) = std::fs::read_dir(backups_dir) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let partial = dir.extension().is_some_and(|ext| ext == &PARTIAL[1..]);
            let abandoned = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age > ABANDONED_AFTER);
            if partial && dir.is_dir() && abandoned && std::fs::remove_dir_all(&dir).is_ok() {
                removed.push(dir);
            }
        }
    }
    Ok(removed)
}

/// Where a staged restore waits for the next boot.
#[must_use]
pub fn pending_path(db_path: &Path) -> PathBuf {
    sibling(db_path, ".restore-pending")
}

fn pending_note_path(db_path: &Path) -> PathBuf {
    sibling(db_path, ".restore-pending.json")
}

/// Written beside the pending database, so the boot that applies it can say
/// what it applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StagedNote {
    snapshot: String,
    taken_at: String,
    /// In [`NAME_FORMAT`]. Names the replaced database, so a boot that dies
    /// part-way through the swap and runs it again picks the same name.
    staged: String,
}

/// A restore checked and waiting for the next boot.
#[derive(Debug, Clone)]
pub struct Staged {
    /// The snapshot's manifest.
    pub manifest: Manifest,
    /// `forge.db.restore-pending`.
    pub pending: PathBuf,
    /// Upload files copied into the live store, which lacked them.
    pub uploads_added: u64,
    /// Upload files the live store already held, checked against their names.
    pub uploads_present: u64,
    /// Live upload files whose bytes no longer matched their names, replaced
    /// from the backup.
    pub uploads_repaired: u64,
    /// Files the restored records point at that neither the live store nor the
    /// backup can supply — they will answer 404. Each was either recorded as
    /// absent when the backup was taken, or accepted with
    /// `allow_missing_uploads`.
    pub uploads_unavailable: Vec<String>,
    /// An earlier staged restore that this one replaced.
    pub replaced_earlier_stage: bool,
}

/// Check `snapshot` against the app as it is declared now and stage it for the
/// next boot of the server whose database is `db_path`.
///
/// Safe while that server is running: the live database is not touched. The
/// only live change is to upload files — one the store lacks is added, one
/// whose bytes no longer match its name is replaced — and both are
/// content-addressed, so the only bytes ever written under a name are the bytes
/// that name means.
///
/// 🔑 **The files a restore owes are the ones the restored records name**, read
/// from the checked copy of the snapshot's database — not whatever the backup's
/// folder happens to hold. Every one is accounted for: present and checked,
/// restored, or named as unavailable. A backup that lost a file after it was
/// taken is refused unless `allow_missing_uploads`; a file the manifest already
/// recorded as absent at backup time is named, not refused, since the backup is
/// faithful about it.
///
/// # Errors
/// [`BackupError::Refused`] when the snapshot fails a check, with nothing
/// staged; [`BackupError::Io`] / [`BackupError::Database`] otherwise.
pub async fn stage_restore(
    snapshot: &Path,
    db_path: &Path,
    data_dir: &Path,
    schema: &ForgeSchema,
    allow_missing_uploads: bool,
) -> Result<Staged, BackupError> {
    let manifest = read_manifest(snapshot)?;
    let staging = sibling(db_path, ".restore-staging");
    let clear_staging = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(sibling(&staging, suffix));
        }
    };
    clear_staging();

    // 1 · the bytes are the bytes that were checked when it was taken.
    let source = snapshot.join(SNAPSHOT_DB);
    let (sha256, _) = {
        let mut input = std::fs::File::open(&source)
            .map_err(io(format!("could not read {}", source.display())))?;
        let mut output = std::fs::File::create(&staging)
            .map_err(io(format!("could not create {}", staging.display())))?;
        pump(&mut input, Some(&mut output))
            .map_err(io(format!("could not copy {}", source.display())))?
    };
    if sha256 != manifest.database.sha256 {
        clear_staging();
        return Err(BackupError::Refused(format!(
            "{} does not match its manifest (sha256 {sha256}, manifest says {}) — it was \
             changed or damaged after the backup was taken. Nothing was staged.",
            source.display(),
            manifest.database.sha256
        )));
    }

    // 2 · it fits the app as declared now: the boot's own check, run on the
    // copy, so a backup that would leave a server refusing to start is refused
    // here instead — before the live database is anywhere near it.
    let upload_ids = match prepare(&staging, schema).await {
        Ok(ids) => ids,
        Err(err) => {
            clear_staging();
            return Err(err);
        }
    };
    if let Err(err) = LibSqlSubstrate::seal(&staging).await {
        clear_staging();
        return Err(BackupError::Database(format!("could not seal the staged copy: {err}")));
    }

    // 3 · every file those records name, decided before any is written, so a
    // refusal leaves the live store exactly as it was.
    let plan = match plan_uploads(snapshot, data_dir, &upload_ids, &manifest) {
        Ok(plan) => plan,
        Err(err) => {
            clear_staging();
            return Err(err);
        }
    };
    let lost: Vec<&str> = plan
        .iter()
        .filter(|(_, step)| matches!(step, Step::Unavailable { recorded: false }))
        .map(|(id, _)| id.as_str())
        .collect();
    if !lost.is_empty() && !allow_missing_uploads {
        clear_staging();
        let shown = lost.iter().take(5).copied().collect::<Vec<_>>().join(", ");
        return Err(BackupError::Refused(format!(
            "this backup is incomplete: {} upload file(s) its records point at are missing or \
             damaged in it, and the live store cannot supply them ({shown}{}). They were present \
             when the backup was taken, so it lost them afterwards. Nothing was staged. Restore \
             an earlier backup, or accept the loss with --allow-missing-uploads.",
            lost.len(),
            if lost.len() > 5 { ", …" } else { "" }
        )));
    }

    let (mut uploads_added, mut uploads_present, mut uploads_repaired) = (0u64, 0u64, 0u64);
    let mut uploads_unavailable = Vec::new();
    for (id, step) in plan {
        match step {
            Step::Present => uploads_present += 1,
            Step::Unavailable { .. } => uploads_unavailable.push(id),
            Step::Copy { repair } => {
                let (Ok(from), Ok(to)) = (path_for(snapshot, &id), path_for(data_dir, &id)) else {
                    uploads_unavailable.push(id);
                    continue;
                };
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(io(format!("could not create {}", parent.display())))?;
                }
                match copy_verified(&from, &to, &id)? {
                    Copied::Bytes(_) if repair => uploads_repaired += 1,
                    Copied::Bytes(_) => uploads_added += 1,
                    // Checked a moment ago; changed since. Named, not hidden.
                    Copied::Missing | Copied::Damaged => uploads_unavailable.push(id),
                }
            }
        }
    }

    // 4 · the note, then the database: the rename is the commit point.
    let pending = pending_path(db_path);
    let replaced_earlier_stage = pending.exists();
    write_json(
        &pending_note_path(db_path),
        &StagedNote {
            snapshot: snapshot.display().to_string(),
            taken_at: manifest.taken_at.clone(),
            staged: chrono::Utc::now().format(NAME_FORMAT).to_string(),
        },
    )?;
    std::fs::rename(&staging, &pending)
        .map_err(io(format!("could not stage {}", pending.display())))?;

    Ok(Staged {
        manifest,
        pending,
        uploads_added,
        uploads_present,
        uploads_repaired,
        uploads_unavailable,
        replaced_earlier_stage,
    })
}

/// Check, migrate and bootstrap the staged copy as the boot would, and return
/// the upload ids its records name.
async fn prepare(staging: &Path, schema: &ForgeSchema) -> Result<Vec<String>, BackupError> {
    let database = |err: crate::forge::value::SubstrateError| {
        BackupError::Database(format!("the staged copy: {err}"))
    };
    let substrate = LibSqlSubstrate::open_local(staging).await.map_err(database)?;
    let check = substrate
        .query("PRAGMA quick_check", &[])
        .await
        .map_err(database)?;
    let verdicts: Vec<&SqlValue> = check.rows.iter().filter_map(|row| row.get(0)).collect();
    if !matches!(verdicts.as_slice(), [SqlValue::Text(ok)] if ok == "ok") {
        return Err(BackupError::Refused(format!(
            "the backup's database fails SQLite's integrity check ({verdicts:?}). Nothing was \
             staged."
        )));
    }
    match crate::forge::drift::evolve_schema(&substrate, schema).await {
        Ok(_) => {}
        Err(VerifyError::Drift(drift)) => {
            let mut lines = String::new();
            for collection in &drift.collections {
                lines.push_str(&format!(
                    "\n  collection '{}' (table '{}')",
                    collection.topic, collection.table
                ));
                for change in &collection.changes {
                    lines.push_str(&format!("\n    · {change}"));
                }
            }
            return Err(BackupError::Refused(format!(
                "this backup does not fit the app as it is declared now — the server would \
                 refuse to boot on it:{lines}\nNothing was staged. Restore it with the `forge` \
                 block it was taken with."
            )));
        }
        Err(VerifyError::Substrate(err)) => return Err(database(err)),
    }
    crate::forge::skeleton::bootstrap_schema(&substrate, schema)
        .await
        .map_err(database)?;

    let has_uploads = substrate
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            &[SqlValue::Text(UPLOADS.to_string())],
        )
        .await
        .map_err(database)?;
    if has_uploads.rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids = substrate
        .query(
            &format!("SELECT DISTINCT upload_id FROM {UPLOADS} ORDER BY upload_id"),
            &[],
        )
        .await
        .map_err(database)?;
    Ok(ids
        .rows
        .iter()
        .filter_map(|row| match row.get(0) {
            Some(SqlValue::Text(id)) => Some(id.clone()),
            _ => None,
        })
        .collect())
}

/// What a restore does for one referenced upload.
enum Step {
    /// The live store holds it, checked.
    Present,
    /// Copy it from the backup — `repair` when a damaged live copy is replaced.
    Copy { repair: bool },
    /// Neither side can supply it. `recorded` when the manifest already said so
    /// at backup time.
    Unavailable { recorded: bool },
}

/// A file checked against the id it is stored under.
enum Checked {
    Good,
    Absent,
    Damaged,
}

/// Is the file at `path` the bytes `id` names? An unreadable file is an error,
/// never an absence: a permissions fault must not read as "nothing to restore".
fn check(path: &Path, id: &str) -> Result<Checked, BackupError> {
    match std::fs::File::open(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Checked::Absent),
        Err(err) => Err(io(format!("could not read {}", path.display()))(err)),
        Ok(mut file) => {
            let (hash, _) =
                pump(&mut file, None).map_err(io(format!("could not read {}", path.display())))?;
            Ok(if hash == id { Checked::Good } else { Checked::Damaged })
        }
    }
}

fn plan_uploads(
    snapshot: &Path,
    data_dir: &Path,
    ids: &[String],
    manifest: &Manifest,
) -> Result<Vec<(String, Step)>, BackupError> {
    let recorded = |id: &str| {
        manifest
            .uploads
            .missing
            .iter()
            .chain(&manifest.uploads.damaged)
            .any(|known| known == id)
    };
    let mut plan = Vec::with_capacity(ids.len());
    for id in ids {
        // A record holding something that is not an id names no file anywhere.
        let (Ok(live), Ok(backed_up)) = (path_for(data_dir, id), path_for(snapshot, id)) else {
            plan.push((id.clone(), Step::Unavailable { recorded: recorded(id) }));
            continue;
        };
        let live = check(&live, id)?;
        if matches!(live, Checked::Good) {
            plan.push((id.clone(), Step::Present));
            continue;
        }
        let step = match check(&backed_up, id)? {
            Checked::Good => Step::Copy {
                repair: matches!(live, Checked::Damaged),
            },
            Checked::Absent | Checked::Damaged => Step::Unavailable { recorded: recorded(id) },
        };
        plan.push((id.clone(), step));
    }
    Ok(plan)
}

/// A staged restore the boot just applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreApplied {
    /// When the restored backup was taken, if its note survived.
    pub taken_at: Option<String>,
    /// Where it was staged from.
    pub snapshot: Option<String>,
    /// Where the database it replaced was moved. `None` when there was none.
    pub previous: Option<PathBuf>,
}

/// Swap a staged restore into place. Call before the database is opened.
///
/// 🔑 **The old `-wal` must go with the old database.** A server stopped
/// uncleanly leaves its last commits in `forge.db-wal`; left beside the
/// restored file, SQLite replays those frames onto it — measured: the next boot
/// fails `database disk image is malformed`. Moved under the aside name, the
/// pair stays whole and opening it recovers every commit
/// (`tests/backup_restore_cli.rs`).
///
/// Order matters, because a boot can die anywhere in it and the pending file
/// makes the next boot run it again: the old `-wal` moves first, then the old
/// database after it under the same name, then the staged file into place.
/// Every intermediate state either still has the old pair together or has it
/// split only until this runs again — and the aside name is derived from the
/// staging time, not the clock, so the rerun joins the pair rather than
/// stranding half of it.
///
/// # Errors
/// [`BackupError::Io`] if a file cannot be moved — on Windows, typically
/// because another process still has the database open.
pub fn apply_staged_restore(db_path: &Path) -> Result<Option<RestoreApplied>, BackupError> {
    let pending = pending_path(db_path);
    if !pending.is_file() {
        return Ok(None);
    }
    let note_path = pending_note_path(db_path);
    let note: Option<StagedNote> = std::fs::read_to_string(&note_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let stamp = note.as_ref().map(|note| note.staged.clone()).or_else(|| {
        let modified = std::fs::metadata(&pending).and_then(|meta| meta.modified()).ok()?;
        Some(chrono::DateTime::<chrono::Utc>::from(modified).format(NAME_FORMAT).to_string())
    });
    let stamp = stamp.unwrap_or_else(|| "staged".to_string());
    let aside = (1..)
        .map(|n| {
            let suffix = if n == 1 {
                format!(".before-restore-{stamp}")
            } else {
                format!(".before-restore-{stamp}-{n}")
            };
            sibling(db_path, &suffix)
        })
        .find(|candidate| !candidate.exists())
        .expect("an unbounded range always has a free name");

    let moved = |from: &Path, to: &Path| -> Result<bool, BackupError> {
        match std::fs::rename(from, to) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(io(format!(
                "could not move {} to {} to apply the staged restore — is another process using \
                 the database?",
                from.display(),
                to.display()
            ))(err)),
        }
    };
    moved(&sibling(db_path, "-wal"), &sibling(&aside, "-wal"))?;
    match std::fs::remove_file(sibling(db_path, "-shm")) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(io(format!(
                "could not clear {}-shm to apply the staged restore",
                db_path.display()
            ))(err))
        }
    }
    let previous = moved(db_path, &aside)?.then_some(aside);
    std::fs::rename(&pending, db_path).map_err(io(format!(
        "could not move the staged restore into {}",
        db_path.display()
    )))?;
    let _ = std::fs::remove_file(&note_path);

    Ok(Some(RestoreApplied {
        taken_at: note.as_ref().map(|note| note.taken_at.clone()),
        snapshot: note.map(|note| note.snapshot),
        previous,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn live(dir: &Path) -> LibSqlSubstrate {
        let substrate = LibSqlSubstrate::open_local(dir.join("forge.db")).await.unwrap();
        substrate
            .migrate("CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)")
            .await
            .unwrap();
        substrate
            .execute("INSERT INTO notes (body) VALUES ('kept')", &[])
            .await
            .unwrap();
        substrate
    }

    /// The property a backup exists for: a commit the server has made is in
    /// it. A file copy of `forge.db` misses exactly these — they sit in the
    /// WAL until a checkpoint, and the server never closes, so it never
    /// checkpoints on its own schedule.
    #[tokio::test]
    async fn a_snapshot_holds_commits_still_in_the_wal() {
        let temp = tempfile::tempdir().unwrap();
        let substrate = live(temp.path()).await;
        assert!(temp.path().join("forge.db-wal").exists(), "the commit is in the WAL");

        let snapshot = take(&substrate, temp.path(), &temp.path().join("backups")).await.unwrap();

        assert_eq!(snapshot.manifest.tables.get("notes"), Some(&1));
        assert!(!sibling(&snapshot.dir.join(SNAPSHOT_DB), "-wal").exists(), "sealed: one file");
        let (sha, _) = hash_file(&snapshot.dir.join(SNAPSHOT_DB)).unwrap();
        assert_eq!(sha, snapshot.manifest.database.sha256);
    }

    #[tokio::test]
    async fn a_staged_restore_swaps_in_at_boot_and_keeps_the_old_database() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("forge.db");
        let backups = temp.path().join("backups");
        let substrate = live(temp.path()).await;
        let snapshot = take(&substrate, temp.path(), &backups).await.unwrap();
        substrate
            .execute("INSERT INTO notes (body) VALUES ('after the backup')", &[])
            .await
            .unwrap();

        let schema = ForgeSchema::empty();
        stage_restore(&snapshot.dir, &db, temp.path(), &schema, false).await.unwrap();
        drop(substrate);

        let applied = apply_staged_restore(&db).unwrap().expect("a restore was pending");
        let previous = applied.previous.expect("the old database was kept");
        assert!(previous.exists());
        assert!(!pending_path(&db).exists());

        let restored = LibSqlSubstrate::open_local(&db).await.unwrap();
        let rows = restored.query("SELECT count(*) FROM notes", &[]).await.unwrap();
        assert_eq!(rows.rows[0].get(0), Some(&SqlValue::Integer(1)));
        drop(restored);
        let kept = LibSqlSubstrate::open_local(&previous).await.unwrap();
        let rows = kept.query("SELECT count(*) FROM notes", &[]).await.unwrap();
        assert_eq!(rows.rows[0].get(0), Some(&SqlValue::Integer(2)), "nothing lost");
    }

    #[tokio::test]
    async fn a_snapshot_changed_after_it_was_taken_is_not_staged() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("forge.db");
        let substrate = live(temp.path()).await;
        let snapshot = take(&substrate, temp.path(), &temp.path().join("backups")).await.unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(snapshot.dir.join(SNAPSHOT_DB))
            .unwrap();
        file.write_all(b"x").unwrap();
        drop(file);

        let err = stage_restore(&snapshot.dir, &db, temp.path(), &ForgeSchema::empty(), false)
            .await
            .unwrap_err();
        assert!(matches!(err, BackupError::Refused(_)), "{err}");
        assert!(!pending_path(&db).exists());
    }

    /// The files follow the snapshot's records: a good one is copied, then
    /// linked by the next snapshot; one whose bytes rotted and one that is gone
    /// are named rather than silently missing; and a restore onto an empty
    /// machine brings the good one back.
    #[tokio::test]
    async fn uploads_follow_the_records_and_unchanged_ones_are_linked() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let substrate = live(data).await;
        let id_of = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let (good, rotted, gone) = (id_of(b"a photo"), id_of(b"what it was"), id_of(b"deleted"));
        for (id, bytes) in [(&good, &b"a photo"[..]), (&rotted, &b"bit rot"[..])] {
            let path = path_for(data, id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        substrate
            .migrate("CREATE TABLE albedo_uploads (id INTEGER PRIMARY KEY AUTOINCREMENT, upload_id TEXT NOT NULL)")
            .await
            .unwrap();
        for id in [&good, &rotted, &gone] {
            substrate
                .execute(
                    "INSERT INTO albedo_uploads (upload_id) VALUES (?1)",
                    &[SqlValue::Text(id.clone())],
                )
                .await
                .unwrap();
        }
        let backups = data.join("backups");

        let first = take(&substrate, data, &backups).await.unwrap();
        let uploads = &first.manifest.uploads;
        assert_eq!((uploads.files, uploads.linked), (1, 0));
        assert_eq!(uploads.missing, vec![gone.clone()]);
        assert_eq!(uploads.damaged, vec![rotted.clone()]);
        assert_eq!(std::fs::read(path_for(&first.dir, &good).unwrap()).unwrap(), b"a photo");

        let second = take(&substrate, data, &backups).await.unwrap();
        assert_eq!(second.manifest.uploads.linked, 1, "an unchanged file is linked, not copied");

        let fresh = tempfile::tempdir().unwrap();
        let staged = stage_restore(
            &second.dir,
            &fresh.path().join("forge.db"),
            fresh.path(),
            &ForgeSchema::empty(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(staged.uploads_added, 1);
        assert_eq!(
            std::fs::read(path_for(fresh.path(), &good).unwrap()).unwrap(),
            b"a photo"
        );
        // Absent when the backup was taken, and recorded so: named, not refused.
        let mut unavailable = staged.uploads_unavailable.clone();
        unavailable.sort();
        let mut expected = vec![rotted, gone];
        expected.sort();
        assert_eq!(unavailable, expected);
    }

    /// One referenced upload, on disk under its content id.
    async fn with_upload(data: &Path) -> (LibSqlSubstrate, String) {
        let substrate = live(data).await;
        substrate
            .migrate("CREATE TABLE albedo_uploads (id INTEGER PRIMARY KEY AUTOINCREMENT, upload_id TEXT NOT NULL)")
            .await
            .unwrap();
        let id = format!("{:x}", Sha256::digest(b"a recoverable photo"));
        substrate
            .execute(
                "INSERT INTO albedo_uploads (upload_id) VALUES (?1)",
                &[SqlValue::Text(id.clone())],
            )
            .await
            .unwrap();
        let path = path_for(data, &id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"a recoverable photo").unwrap();
        (substrate, id)
    }

    fn holds(path: PathBuf, id: &str) -> bool {
        std::fs::read(path).is_ok_and(|bytes| format!("{:x}", Sha256::digest(&bytes)) == id)
    }

    /// Found by review 2026-10-30: an earlier snapshot's rotted copy was linked
    /// unread into the next and certified good. Now it is checked, the next
    /// snapshot copies the healthy live file instead, and the rot is named.
    #[tokio::test]
    async fn a_rotted_earlier_copy_is_not_carried_into_the_next_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let (substrate, id) = with_upload(temp.path()).await;
        let backups = temp.path().join("backups");
        let first = take(&substrate, temp.path(), &backups).await.unwrap();
        std::fs::write(path_for(&first.dir, &id).unwrap(), b"damaged backup bytes").unwrap();

        let second = take(&substrate, temp.path(), &backups).await.unwrap();

        assert!(holds(path_for(&second.dir, &id).unwrap(), &id), "rot carried forward");
        let uploads = &second.manifest.uploads;
        assert_eq!((uploads.files, uploads.linked), (1, 0));
        assert_eq!(uploads.earlier_damaged, vec![id]);
    }

    /// Found by review 2026-10-30: a damaged live file was counted present
    /// because its name existed. Restore is often the answer to exactly that.
    #[tokio::test]
    async fn a_restore_repairs_a_damaged_live_file() {
        let temp = tempfile::tempdir().unwrap();
        let (substrate, id) = with_upload(temp.path()).await;
        let snapshot = take(&substrate, temp.path(), &temp.path().join("backups")).await.unwrap();
        drop(substrate);
        std::fs::write(path_for(temp.path(), &id).unwrap(), b"damaged live bytes").unwrap();

        let staged = stage_restore(
            &snapshot.dir,
            &temp.path().join("forge.db"),
            temp.path(),
            &ForgeSchema::empty(),
            false,
        )
        .await
        .unwrap();

        assert!(holds(path_for(temp.path(), &id).unwrap(), &id));
        assert_eq!((staged.uploads_repaired, staged.uploads_present), (1, 0));
    }

    /// Found by review 2026-10-30: a backup missing a file it promised staged
    /// as complete. Now the records decide what is owed, the loss is refused by
    /// name with nothing staged or written, and accepting it is explicit.
    #[tokio::test]
    async fn a_backup_that_lost_a_file_is_refused_unless_the_loss_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let (substrate, id) = with_upload(temp.path()).await;
        let snapshot = take(&substrate, temp.path(), &temp.path().join("backups")).await.unwrap();
        drop(substrate);
        std::fs::remove_file(path_for(&snapshot.dir, &id).unwrap()).unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let db = fresh.path().join("forge.db");

        let refused = stage_restore(&snapshot.dir, &db, fresh.path(), &ForgeSchema::empty(), false)
            .await
            .unwrap_err();
        assert!(matches!(&refused, BackupError::Refused(why) if why.contains(&id)), "{refused}");
        assert!(!pending_path(&db).exists());
        assert!(!fresh.path().join(UPLOAD_DIR_FOR_TESTS).exists(), "a refusal wrote files");

        let accepted = stage_restore(&snapshot.dir, &db, fresh.path(), &ForgeSchema::empty(), true)
            .await
            .unwrap();
        assert_eq!(accepted.uploads_unavailable, vec![id]);
        assert!(pending_path(&db).exists());
    }

    const UPLOAD_DIR_FOR_TESTS: &str = crate::upload::store::UPLOAD_DIR;

    /// The swap resumed from each state a boot can die in. Constructed by the
    /// 2026-10-30 review with sentinel files: process-death states, not a
    /// power-loss or fsync test.
    #[test]
    fn an_interrupted_swap_resumes_without_splitting_the_old_pair() {
        for state in 0..4 {
            let temp = tempfile::tempdir().unwrap();
            let db = temp.path().join("forge.db");
            let aside = temp.path().join("forge.db.before-restore-review");
            let wal = temp.path().join("forge.db-wal");
            std::fs::write(&db, b"old database").unwrap();
            std::fs::write(&wal, b"old WAL").unwrap();
            std::fs::write(pending_path(&db), b"restored database").unwrap();
            std::fs::write(
                pending_note_path(&db),
                r#"{"snapshot":"review","taken_at":"review","staged":"review"}"#,
            )
            .unwrap();
            if state >= 1 {
                std::fs::rename(&wal, sibling(&aside, "-wal")).unwrap();
            }
            if state >= 2 {
                std::fs::rename(&db, &aside).unwrap();
            }
            if state >= 3 {
                std::fs::rename(pending_path(&db), &db).unwrap();
            }

            apply_staged_restore(&db).unwrap();

            assert_eq!(std::fs::read(&db).unwrap(), b"restored database", "state {state}");
            assert_eq!(std::fs::read(&aside).unwrap(), b"old database", "state {state}");
            assert_eq!(std::fs::read(sibling(&aside, "-wal")).unwrap(), b"old WAL", "state {state}");
            assert!(!wal.exists(), "state {state}");
        }
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_what_it_did_not_write() {
        let temp = tempfile::tempdir().unwrap();
        for (name, ms) in [("a", 1), ("b", 3), ("c", 2)] {
            let dir = temp.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            let manifest = Manifest {
                format: FORMAT,
                albedo: "test".into(),
                taken_at: String::new(),
                taken_at_ms: ms,
                database: DatabaseFacts { sha256: String::new(), bytes: 0 },
                tables: BTreeMap::new(),
                uploads: UploadFacts::default(),
            };
            write_json(&dir.join(MANIFEST_FILE), &manifest).unwrap();
        }
        std::fs::create_dir(temp.path().join("not-a-backup")).unwrap();

        let removed = prune(temp.path(), 2).unwrap();

        assert_eq!(removed, vec![temp.path().join("a")]);
        assert!(temp.path().join("b").exists() && temp.path().join("c").exists());
        assert!(temp.path().join("not-a-backup").exists());
    }
}
