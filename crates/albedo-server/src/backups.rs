//! Scheduled backups — `ALBEDO_BACKUP_DIR`, `ALBEDO_BACKUP_EVERY`,
//! `ALBEDO_BACKUP_KEEP`. The snapshot itself is
//! [`dom_render_compiler::forge::backup`].
//!
//! 🔑 **The schedule is the newest snapshot's time, read off the backup
//! directory — not a timer started at boot.** A boot-relative interval is reset
//! by every restart, so a service deployed more often than it backs up never
//! backs up at all, and a crash loop, when a backup is most wanted, takes none.
//! Read from the directory, the clock survives restarts, a server that was down
//! through its slot takes one as soon as it is back, and there is no state to
//! keep beyond the snapshots themselves.
//!
//! 🪤 **A failure is printed, not only traced.** No `tracing` subscriber exists
//! unless `RUST_LOG` is set, and a backup that fails into a channel nobody reads
//! is discovered on the day of the restore. It goes to stderr — the container
//! log — and to `albedo_backup_failures_total`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
#[cfg(feature = "forge")]
use std::sync::Arc;
use std::time::Duration;

use crate::config::BackupSettings;

/// Where `albedo db backup` writes, beside the data, when nothing names a
/// directory. The server schedules nothing by default; the CLI still needs
/// somewhere to put a snapshot taken by hand.
pub const DEFAULT_DIR: &str = "backups";

/// After a failed snapshot, try again this soon rather than a period later.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// What the scheduler is set to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Absolute.
    pub dir: PathBuf,
    pub every: Duration,
    pub keep: usize,
}

impl Plan {
    /// `None` when no directory is configured.
    #[must_use]
    pub fn from_settings(settings: &BackupSettings, data_dir: &Path) -> Option<Self> {
        let dir = settings.dir.as_deref()?;
        Some(Self {
            dir: resolve_dir(dir, data_dir),
            every: Duration::from_secs(settings.every_secs),
            keep: settings.keep,
        })
    }

    /// One line for the boot banner.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "every {} to {}, keeping {}",
            human(self.every),
            self.dir.display(),
            self.keep
        )
    }
}

/// Where snapshots go: an absolute path as given, a relative one beside the
/// data.
#[must_use]
pub fn resolve_dir(dir: &str, data_dir: &Path) -> PathBuf {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        path
    } else {
        data_dir.join(path)
    }
}

/// How long until a snapshot is due, given the newest one's time.
#[must_use]
pub fn due_in(newest_ms: Option<i64>, now_ms: i64, every: Duration) -> Duration {
    let Some(newest) = newest_ms else {
        return Duration::ZERO;
    };
    let every_ms = i64::try_from(every.as_millis()).unwrap_or(i64::MAX);
    let due = newest.saturating_add(every_ms);
    u64::try_from(due.saturating_sub(now_ms)).map_or(Duration::ZERO, Duration::from_millis)
}

fn human(every: Duration) -> String {
    let secs = every.as_secs();
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// What a scrape reports. Shared between the scheduler and the metrics route.
#[derive(Debug, Default)]
pub struct BackupStatus {
    scheduled: AtomicBool,
    /// Unix ms of the newest snapshot; 0 for none.
    last_success_ms: AtomicI64,
    failures: AtomicU64,
}

impl BackupStatus {
    /// `(scheduled, last success in seconds since the epoch, failures)`.
    #[must_use]
    pub fn read(&self) -> (bool, f64, u64) {
        (
            self.scheduled.load(Ordering::Relaxed),
            self.last_success_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            self.failures.load(Ordering::Relaxed),
        )
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// Run the plan for as long as the runtime lives.
#[cfg(feature = "forge")]
pub fn spawn(
    plan: Plan,
    substrate: Arc<dom_render_compiler::forge::LibSqlSubstrate>,
    data_dir: PathBuf,
    status: Arc<BackupStatus>,
) {
    use dom_render_compiler::forge::backup;

    status.scheduled.store(true, Ordering::Relaxed);
    tokio::spawn(async move {
        loop {
            let newest = backup::list(&plan.dir)
                .first()
                .map(|snapshot| snapshot.manifest.taken_at_ms);
            if let Some(newest) = newest {
                status.last_success_ms.store(newest, Ordering::Relaxed);
            }
            tokio::time::sleep(due_in(newest, now_ms(), plan.every)).await;

            match backup::take(&substrate, &data_dir, &plan.dir).await {
                Ok(snapshot) => {
                    status
                        .last_success_ms
                        .store(snapshot.manifest.taken_at_ms, Ordering::Relaxed);
                    let uploads = &snapshot.manifest.uploads;
                    if !uploads.missing.is_empty() || !uploads.damaged.is_empty() {
                        eprintln!(
                            "  !  backup {}: {} upload file(s) missing and {} damaged in the live \
                             store — named in its manifest.json",
                            snapshot.dir.display(),
                            uploads.missing.len(),
                            uploads.damaged.len()
                        );
                    }
                    if !uploads.earlier_damaged.is_empty() {
                        eprintln!(
                            "  !  backup {}: {} file(s) in the previous backup no longer match \
                             their contents — the backup disk is losing bytes",
                            snapshot.dir.display(),
                            uploads.earlier_damaged.len()
                        );
                    }
                    tracing::info!(
                        target: "albedo.backup",
                        dir = %snapshot.dir.display(),
                        rows = snapshot.manifest.rows(),
                        files = uploads.files,
                        "backup taken"
                    );
                    if let Err(err) = backup::prune(&plan.dir, plan.keep) {
                        eprintln!("  !  backup retention: {err}");
                    }
                }
                Err(err) => {
                    status.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "  ✗  backup to {} FAILED: {err} — retrying in {}",
                        plan.dir.display(),
                        human(RETRY_AFTER)
                    );
                    tokio::time::sleep(RETRY_AFTER).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect a boot-relative timer has: a restart resets it. Here a
    /// restart changes nothing — the due time is a property of the newest
    /// snapshot, so a server that restarts hourly still backs up daily.
    #[test]
    fn the_due_time_comes_from_the_newest_snapshot_not_from_boot() {
        let day = Duration::from_secs(86_400);
        let hour_ms = 3_600_000;
        assert_eq!(due_in(None, 0, day), Duration::ZERO, "none yet: take one now");
        assert_eq!(
            due_in(Some(0), 23 * hour_ms, day),
            Duration::from_millis(hour_ms as u64)
        );
        assert_eq!(due_in(Some(0), 30 * hour_ms, day), Duration::ZERO, "overdue: now");
    }

    #[test]
    fn a_relative_directory_sits_beside_the_data() {
        let data = Path::new("/data");
        assert_eq!(resolve_dir("backups", data), PathBuf::from("/data/backups"));
        assert_eq!(resolve_dir("/mnt/offsite", data), PathBuf::from("/mnt/offsite"));
        let plan = Plan::from_settings(
            &BackupSettings {
                dir: Some("backups".into()),
                every_secs: 6 * 3_600,
                keep: 4,
            },
            data,
        )
        .unwrap();
        assert_eq!(
            plan.describe(),
            format!("every 6h to {}, keeping 4", data.join("backups").display())
        );
        assert_eq!(Plan::from_settings(&BackupSettings::default(), data), None);
    }
}
