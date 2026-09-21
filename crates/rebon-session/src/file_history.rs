//! Per-turn file snapshots for `/rewind`, and the restore path behind it.
//!
//! A snapshot records, per tracked file, the bytes that were there *before* the
//! turn's `Write`/`Edit` calls ran, keyed by a cwd-relative (or, outside `cwd`,
//! absolute) tracking path. Rewinding a turn means putting those bytes back, so
//! everything here is built around proving what the working tree currently
//! holds before anything is overwritten: [`FileHistoryStore::restore_capability`]
//! is the preflight callers display, and [`FileHistoryStore::apply_snapshot`]
//! re-checks every path at commit time under the session's active lock.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::session_storage::{format_system_time_iso_ms, project_dir_path, SessionActiveLock};

const MANIFEST_FILE_NAME: &str = "manifest.json";
const BACKUPS_DIR_NAME: &str = "backups";
const MAX_SNAPSHOTS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHistoryBackup {
    #[serde(rename = "backupFileName")]
    pub backup_file_name: Option<String>,
    pub version: u32,
    #[serde(rename = "backupTime")]
    pub backup_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHistorySnapshot {
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(rename = "trackedFileBackups")]
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHistoryManifest {
    pub version: u32,
    pub cwd: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "trackedFiles")]
    pub tracked_files: BTreeSet<String>,
    pub snapshots: Vec<FileHistorySnapshot>,
    /// Authoritative bytes at the completion of the latest retained agent turn.
    /// Older manifests omit this field and remain conservatively unavailable.
    #[serde(default, rename = "currentHead")]
    pub current_head: Option<FileHistorySnapshot>,
    #[serde(rename = "snapshotSequence")]
    pub snapshot_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryDiffStats {
    pub files_changed: Vec<String>,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryApplyReport {
    pub restored: usize,
    pub skipped: usize,
    /// Paths skipped because their bytes changed after the clean preflight.
    pub conflicts: Vec<String>,
    /// Per-path I/O or backup-validation failures. Earlier successful paths may
    /// already have been restored, so callers must report this as partial.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRestoreUnavailableReason {
    MissingManifest,
    CorruptManifest(String),
    MissingSnapshot,
    MissingOrCorruptBackup(String),
    UnknownExpectedCurrent(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRestoreOperation {
    Noop,
    Write,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRestorePreflightPath {
    pub tracking_path: String,
    pub display_path: String,
    pub desired_hash: Option<[u8; 32]>,
    pub current_hash: Option<[u8; 32]>,
    pub expected_current_hash: Option<[u8; 32]>,
    pub operation: FileRestoreOperation,
    pub conflict_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRestorePreflight {
    pub message_id: String,
    pub paths: Vec<FileRestorePreflightPath>,
    pub writes: usize,
    pub deletes: usize,
    pub unchanged: usize,
    pub conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRestoreCapability {
    Unavailable(FileRestoreUnavailableReason),
    Clean(FileRestorePreflight),
    Conflicted(FileRestorePreflight),
}

impl FileHistoryApplyReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileHistoryLoadError {
    Missing,
    Corrupt(String),
}

impl std::fmt::Display for FileHistoryLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "file-history manifest not found"),
            Self::Corrupt(err) => write!(f, "file-history manifest is corrupt: {err}"),
        }
    }
}

impl std::error::Error for FileHistoryLoadError {}

#[derive(Debug, thiserror::Error)]
pub enum FileHistoryApplyError {
    #[error("the supplied active-session lock does not own this file-history session")]
    WrongSessionLock,
    #[error("file restore is unavailable: {0:?}")]
    Unavailable(FileRestoreUnavailableReason),
    #[error("file restore conflicts with current working-tree content")]
    Conflicted(FileRestorePreflight),
}

#[derive(Debug, Clone)]
pub struct FileHistoryStore {
    projects_root: PathBuf,
    cwd: PathBuf,
    cwd_display: String,
    session_id: String,
}

impl FileHistoryStore {
    pub fn new(
        projects_root: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        session_id: impl Into<String>,
    ) -> Self {
        let cwd = cwd.into();
        let cwd_display = cwd.to_string_lossy().to_string();
        Self {
            projects_root: projects_root.into(),
            cwd,
            cwd_display,
            session_id: session_id.into(),
        }
    }

    pub fn file_history_dir(&self) -> PathBuf {
        project_dir_path(&self.projects_root, &self.cwd_display)
            .join(format!("{}.file-history", self.session_id))
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.file_history_dir().join(MANIFEST_FILE_NAME)
    }

    fn backups_dir(&self) -> PathBuf {
        self.file_history_dir().join(BACKUPS_DIR_NAME)
    }

    fn lock_manifest_transaction(&self) -> std::io::Result<fs::File> {
        let dir = self.file_history_dir();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(dir.join("manifest.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    pub fn load_manifest(&self) -> Result<FileHistoryManifest, FileHistoryLoadError> {
        let path = self.manifest_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileHistoryLoadError::Missing)
            }
            Err(err) => return Err(FileHistoryLoadError::Corrupt(err.to_string())),
        };
        serde_json::from_slice(&bytes).map_err(|err| FileHistoryLoadError::Corrupt(err.to_string()))
    }

    pub fn load_or_empty_manifest(&self) -> FileHistoryManifest {
        self.load_manifest()
            .unwrap_or_else(|_| FileHistoryManifest {
                version: 1,
                cwd: self.cwd_display.clone(),
                session_id: self.session_id.clone(),
                tracked_files: BTreeSet::new(),
                snapshots: Vec::new(),
                current_head: None,
                snapshot_sequence: 0,
            })
    }

    fn save_manifest(&self, manifest: &FileHistoryManifest) -> std::io::Result<()> {
        let dir = self.file_history_dir();
        fs::create_dir_all(&dir)?;
        let path = self.manifest_path();
        let body = serde_json::to_vec_pretty(manifest)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        crate::session_storage::write_file_atomically(&path, &body)
    }

    pub fn track_before_write(&self, file_path: &Path, message_id: &str) -> anyhow::Result<()> {
        let _manifest_lock = self.lock_manifest_transaction()?;
        // Files inside cwd are keyed by a cwd-relative path; files outside it —
        // which have no relative form — are keyed by their absolute path so they
        // stay restorable rather than being dropped from the rewind set.
        let tracking_path = self.tracking_path(file_path)?;
        let mut manifest = self.load_or_empty_manifest();
        if manifest.snapshots.is_empty() {
            manifest.snapshots.push(FileHistorySnapshot {
                message_id: message_id.to_string(),
                tracked_file_backups: BTreeMap::new(),
                timestamp: format_system_time_iso_ms(SystemTime::now()),
            });
        }
        let last_index = manifest.snapshots.len().saturating_sub(1);
        if manifest.snapshots[last_index]
            .tracked_file_backups
            .contains_key(&tracking_path)
        {
            return Ok(());
        }
        let backup = self.create_backup(file_path, 1)?;
        manifest.tracked_files.insert(tracking_path.clone());
        manifest.snapshots[last_index]
            .tracked_file_backups
            .insert(tracking_path, backup);
        self.save_manifest(&manifest)?;
        Ok(())
    }

    pub fn make_snapshot(&self, message_id: &str) -> anyhow::Result<()> {
        let _manifest_lock = self.lock_manifest_transaction()?;
        let mut manifest = self.load_or_empty_manifest();
        if manifest
            .snapshots
            .last()
            .is_some_and(|snapshot| snapshot.message_id == message_id)
        {
            return Ok(());
        }
        let previous = manifest.snapshots.last().cloned();
        let mut tracked_file_backups = BTreeMap::new();
        let tracked_files = manifest.tracked_files.iter().cloned().collect::<Vec<_>>();
        for tracking_path in tracked_files {
            let file_path = self.expand_tracking_path(&tracking_path)?;
            let latest = previous
                .as_ref()
                .and_then(|snapshot| snapshot.tracked_file_backups.get(&tracking_path));
            let next_version = latest.map(|backup| backup.version + 1).unwrap_or(1);
            let backup = match latest {
                Some(latest_backup)
                    if latest_backup.backup_file_name.is_some()
                        && !self.origin_changed_from_backup(&file_path, latest_backup)? =>
                {
                    latest_backup.clone()
                }
                Some(latest_backup)
                    if latest_backup.backup_file_name.is_none() && !file_path.exists() =>
                {
                    latest_backup.clone()
                }
                _ => self.create_backup(&file_path, next_version)?,
            };
            tracked_file_backups.insert(tracking_path, backup);
        }

        if let Some(last) = manifest.snapshots.last() {
            for tracking_path in &manifest.tracked_files {
                if tracked_file_backups.contains_key(tracking_path) {
                    continue;
                }
                if let Some(inherited) = last.tracked_file_backups.get(tracking_path) {
                    tracked_file_backups.insert(tracking_path.clone(), inherited.clone());
                }
            }
        }

        manifest.snapshots.push(FileHistorySnapshot {
            message_id: message_id.to_string(),
            tracked_file_backups,
            timestamp: format_system_time_iso_ms(SystemTime::now()),
        });
        if manifest.snapshots.len() > MAX_SNAPSHOTS {
            let start = manifest.snapshots.len() - MAX_SNAPSHOTS;
            manifest.snapshots = manifest.snapshots.split_off(start);
        }
        manifest.snapshot_sequence = manifest.snapshot_sequence.saturating_add(1);
        self.save_manifest(&manifest)?;
        Ok(())
    }

    /// Record the authoritative working-tree head after an agent turn finishes.
    /// This is deliberately separate from the next prompt's pre-turn snapshot so
    /// the newest turn can be rewound and older turns compare against all known
    /// later agent edits rather than only their immediate successor.
    pub fn record_current_head(&self, message_id: &str) -> anyhow::Result<()> {
        let _manifest_lock = self.lock_manifest_transaction()?;
        let mut manifest = self.load_manifest().map_err(anyhow::Error::new)?;
        if !manifest
            .snapshots
            .iter()
            .any(|snapshot| snapshot.message_id == message_id)
        {
            anyhow::bail!("file-history snapshot for completed turn is unavailable");
        }
        let mut tracked_file_backups = BTreeMap::new();
        for tracking_path in manifest.tracked_files.iter().cloned() {
            let file_path = self.expand_tracking_path(&tracking_path)?;
            let prior_version = manifest
                .current_head
                .as_ref()
                .and_then(|head| head.tracked_file_backups.get(&tracking_path))
                .or_else(|| {
                    manifest
                        .snapshots
                        .last()
                        .and_then(|snapshot| snapshot.tracked_file_backups.get(&tracking_path))
                })
                .map_or(0, |backup| backup.version);
            tracked_file_backups.insert(
                tracking_path,
                self.create_backup(&file_path, prior_version.saturating_add(1))?,
            );
        }
        manifest.current_head = Some(FileHistorySnapshot {
            message_id: message_id.to_string(),
            tracked_file_backups,
            timestamp: format_system_time_iso_ms(SystemTime::now()),
        });
        manifest.snapshot_sequence = manifest.snapshot_sequence.saturating_add(1);
        self.save_manifest(&manifest)?;
        Ok(())
    }

    pub fn can_restore(&self, message_id: &str) -> bool {
        self.load_manifest().ok().is_some_and(|manifest| {
            manifest
                .snapshots
                .iter()
                .any(|snapshot| snapshot.message_id == message_id)
        })
    }

    pub fn diff_stats(
        &self,
        message_id: &str,
    ) -> Result<Option<FileHistoryDiffStats>, FileHistoryLoadError> {
        let manifest = self.load_manifest()?;
        Ok(self.diff_stats_from_manifest(&manifest, message_id))
    }

    fn diff_stats_from_manifest(
        &self,
        manifest: &FileHistoryManifest,
        message_id: &str,
    ) -> Option<FileHistoryDiffStats> {
        let target = manifest
            .snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.message_id == message_id)?;
        let mut files_changed = Vec::new();
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for tracking_path in &manifest.tracked_files {
            let Some(target_backup) = self.backup_for_target(manifest, target, tracking_path)
            else {
                continue;
            };
            let Ok(file_path) = self.expand_tracking_path(tracking_path) else {
                continue;
            };
            let target_content = match &target_backup.backup_file_name {
                Some(name) => fs::read_to_string(self.backup_path(name)).ok(),
                None => None,
            };
            let current_content = fs::read_to_string(&file_path).ok();
            if current_content == target_content {
                continue;
            }
            if target_backup.backup_file_name.is_none() && !file_path.exists() {
                continue;
            }
            let (adds, removes) = line_diff_counts(
                current_content.as_deref().unwrap_or(""),
                target_content.as_deref().unwrap_or(""),
            );
            files_changed.push(file_path.to_string_lossy().to_string());
            insertions += adds;
            deletions += removes;
        }
        Some(FileHistoryDiffStats {
            files_changed,
            insertions,
            deletions,
        })
    }

    /// Compute a conservative, read-only restore plan. A mutating operation is
    /// considered safe only when the current bytes still equal the authoritative
    /// head recorded at completion of the latest retained agent turn. This
    /// permits rewinding the newest turn and multiple later same-file edits while
    /// still refusing unrecorded manual/bash edits. Older manifests without a
    /// completed head remain honestly unavailable rather than guessing.
    pub fn restore_capability(&self, message_id: &str) -> FileRestoreCapability {
        let manifest = match self.load_manifest() {
            Ok(manifest) => manifest,
            Err(FileHistoryLoadError::Missing) => {
                return FileRestoreCapability::Unavailable(
                    FileRestoreUnavailableReason::MissingManifest,
                );
            }
            Err(FileHistoryLoadError::Corrupt(error)) => {
                return FileRestoreCapability::Unavailable(
                    FileRestoreUnavailableReason::CorruptManifest(error),
                );
            }
        };
        let Some(target_index) = manifest
            .snapshots
            .iter()
            .rposition(|snapshot| snapshot.message_id == message_id)
        else {
            return FileRestoreCapability::Unavailable(
                FileRestoreUnavailableReason::MissingSnapshot,
            );
        };
        let target = &manifest.snapshots[target_index];
        let expected_snapshot = manifest.current_head.as_ref();
        let mut preflight = FileRestorePreflight {
            message_id: message_id.to_string(),
            paths: Vec::new(),
            writes: 0,
            deletes: 0,
            unchanged: 0,
            conflicts: 0,
        };

        for tracking_path in &manifest.tracked_files {
            let Some(target_backup) = self.backup_for_target(&manifest, target, tracking_path)
            else {
                return FileRestoreCapability::Unavailable(
                    FileRestoreUnavailableReason::MissingOrCorruptBackup(tracking_path.clone()),
                );
            };
            let file_path = match self.expand_tracking_path(tracking_path) {
                Ok(path) => path,
                Err(error) => {
                    return FileRestoreCapability::Unavailable(
                        FileRestoreUnavailableReason::MissingOrCorruptBackup(error.to_string()),
                    );
                }
            };
            let desired = match self.read_backup_state(target_backup) {
                Ok(state) => state,
                Err(error) => {
                    return FileRestoreCapability::Unavailable(
                        FileRestoreUnavailableReason::MissingOrCorruptBackup(error.to_string()),
                    );
                }
            };
            let current = match read_optional_file(&file_path) {
                Ok(state) => state,
                Err(error) => {
                    return FileRestoreCapability::Unavailable(
                        FileRestoreUnavailableReason::MissingOrCorruptBackup(error.to_string()),
                    );
                }
            };
            let operation = if current == desired {
                FileRestoreOperation::Noop
            } else if desired.is_some() {
                FileRestoreOperation::Write
            } else {
                FileRestoreOperation::Delete
            };
            let expected_current = if operation == FileRestoreOperation::Noop {
                current.clone()
            } else {
                let Some(expected_snapshot) = expected_snapshot else {
                    return FileRestoreCapability::Unavailable(
                        FileRestoreUnavailableReason::UnknownExpectedCurrent(tracking_path.clone()),
                    );
                };
                let Some(expected_backup) =
                    self.backup_for_target(&manifest, expected_snapshot, tracking_path)
                else {
                    return FileRestoreCapability::Unavailable(
                        FileRestoreUnavailableReason::UnknownExpectedCurrent(tracking_path.clone()),
                    );
                };
                match self.read_backup_state(expected_backup) {
                    Ok(state) => state,
                    Err(_) => {
                        return FileRestoreCapability::Unavailable(
                            FileRestoreUnavailableReason::UnknownExpectedCurrent(
                                tracking_path.clone(),
                            ),
                        );
                    }
                }
            };
            let conflict_reason = (current != expected_current)
                .then(|| "current content differs from the recorded post-turn state".to_string());
            match operation {
                FileRestoreOperation::Noop => preflight.unchanged += 1,
                FileRestoreOperation::Write => preflight.writes += 1,
                FileRestoreOperation::Delete => preflight.deletes += 1,
            }
            if conflict_reason.is_some() {
                preflight.conflicts += 1;
            }
            preflight.paths.push(FileRestorePreflightPath {
                tracking_path: tracking_path.clone(),
                display_path: file_path.to_string_lossy().to_string(),
                desired_hash: optional_hash(desired.as_deref()),
                current_hash: optional_hash(current.as_deref()),
                expected_current_hash: optional_hash(expected_current.as_deref()),
                operation,
                conflict_reason,
            });
        }
        if preflight.conflicts == 0 {
            FileRestoreCapability::Clean(preflight)
        } else {
            FileRestoreCapability::Conflicted(preflight)
        }
    }

    fn read_backup_state(&self, backup: &FileHistoryBackup) -> std::io::Result<Option<Vec<u8>>> {
        match backup.backup_file_name.as_deref() {
            Some(name) => fs::read(self.backup_path(name)).map(Some),
            None => Ok(None),
        }
    }

    /// Apply a snapshot only while the caller owns the matching live-session
    /// lock. The capability is rebuilt here (never trusted from UI state), and
    /// any unavailable or conflicted preflight is rejected before mutation.
    /// Each path is then re-read immediately before its write/delete; a late
    /// manual/bash change is skipped and reported without being overwritten.
    pub fn apply_snapshot(
        &self,
        message_id: &str,
        active_lock: &SessionActiveLock,
    ) -> Result<FileHistoryApplyReport, FileHistoryApplyError> {
        if !active_lock.is_for(&self.projects_root, &self.cwd_display, &self.session_id) {
            return Err(FileHistoryApplyError::WrongSessionLock);
        }
        let preflight = match self.restore_capability(message_id) {
            FileRestoreCapability::Clean(preflight) => preflight,
            FileRestoreCapability::Conflicted(preflight) => {
                return Err(FileHistoryApplyError::Conflicted(preflight));
            }
            FileRestoreCapability::Unavailable(reason) => {
                return Err(FileHistoryApplyError::Unavailable(reason));
            }
        };
        let manifest = self.load_manifest().map_err(|error| match error {
            FileHistoryLoadError::Missing => {
                FileHistoryApplyError::Unavailable(FileRestoreUnavailableReason::MissingManifest)
            }
            FileHistoryLoadError::Corrupt(error) => FileHistoryApplyError::Unavailable(
                FileRestoreUnavailableReason::CorruptManifest(error),
            ),
        })?;
        let Some(target) = manifest
            .snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.message_id == message_id)
        else {
            return Err(FileHistoryApplyError::Unavailable(
                FileRestoreUnavailableReason::MissingSnapshot,
            ));
        };

        let mut report = FileHistoryApplyReport {
            restored: 0,
            skipped: 0,
            conflicts: Vec::new(),
            errors: Vec::new(),
        };
        for planned in &preflight.paths {
            if planned.operation == FileRestoreOperation::Noop {
                continue;
            }
            let file_path = match self.expand_tracking_path(&planned.tracking_path) {
                Ok(path) => path,
                Err(error) => {
                    report.skipped += 1;
                    report.errors.push(error.to_string());
                    continue;
                }
            };

            // This is the commit-time compare. Do not replace it with exists()
            // checks: absence is part of the expected state for delete/create.
            let current = match read_optional_file(&file_path) {
                Ok(current) => current,
                Err(error) => {
                    report.skipped += 1;
                    report.errors.push(format!(
                        "failed to recheck {} immediately before restore: {error}",
                        file_path.display()
                    ));
                    continue;
                }
            };
            if optional_hash(current.as_deref()) != planned.expected_current_hash {
                report.skipped += 1;
                report.conflicts.push(planned.display_path.clone());
                continue;
            }

            let Some(target_backup) =
                self.backup_for_target(&manifest, target, &planned.tracking_path)
            else {
                report.skipped += 1;
                report.errors.push(format!(
                    "missing backup metadata for {}",
                    planned.tracking_path
                ));
                continue;
            };
            let desired = match self.read_backup_state(target_backup) {
                Ok(desired) => desired,
                Err(error) => {
                    report.skipped += 1;
                    report.errors.push(format!(
                        "failed to read verified backup for {}: {error}",
                        planned.display_path
                    ));
                    continue;
                }
            };
            if optional_hash(desired.as_deref()) != planned.desired_hash {
                report.skipped += 1;
                report.errors.push(format!(
                    "backup changed after preflight for {}",
                    planned.display_path
                ));
                continue;
            }

            // Mutate the same opened object whose bytes are validated. This avoids
            // a second path lookup that could follow an editor's atomic save or a
            // symlink/path-component swap after validation.
            let result = match desired {
                Some(bytes) => {
                    if let Some(parent) = file_path.parent() {
                        if let Err(error) = fs::create_dir_all(parent) {
                            Err(error)
                        } else if planned.expected_current_hash.is_none() {
                            OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(&file_path)
                                .and_then(|mut file| {
                                    file.write_all(&bytes)?;
                                    file.sync_all()
                                })
                        } else {
                            open_existing_restore_target(&file_path).and_then(|mut file| {
                                let mut opened_bytes = Vec::new();
                                file.read_to_end(&mut opened_bytes)?;
                                if optional_hash(Some(&opened_bytes))
                                    != planned.expected_current_hash
                                    || !opened_file_still_names_path(&file, &file_path)?
                                {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::WouldBlock,
                                        "target changed before the opened-object commit",
                                    ));
                                }
                                file.seek(SeekFrom::Start(0))?;
                                file.set_len(0)?;
                                file.write_all(&bytes)?;
                                file.sync_all()?;
                                if !opened_file_still_names_path(&file, &file_path)? {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::WouldBlock,
                                        "target path identity changed during restore",
                                    ));
                                }
                                Ok(())
                            })
                        }
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "restore target has no parent directory",
                        ))
                    }
                }
                None if planned.expected_current_hash.is_none() => Ok(()),
                None => {
                    delete_opened_target_conditionally(&file_path, planned.expected_current_hash)
                }
            };
            match result {
                Ok(()) => report.restored += 1,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::AlreadyExists
                        || error.kind() == std::io::ErrorKind::NotFound =>
                {
                    report.skipped += 1;
                    report.conflicts.push(planned.display_path.clone());
                }
                Err(error) => {
                    report.skipped += 1;
                    report.errors.push(format!(
                        "failed to restore {}: {error}",
                        file_path.display()
                    ));
                }
            }
        }
        Ok(report)
    }

    fn backup_for_target<'a>(
        &self,
        manifest: &'a FileHistoryManifest,
        target: &'a FileHistorySnapshot,
        tracking_path: &str,
    ) -> Option<&'a FileHistoryBackup> {
        target.tracked_file_backups.get(tracking_path).or_else(|| {
            manifest.snapshots.iter().find_map(|snapshot| {
                match snapshot.tracked_file_backups.get(tracking_path) {
                    Some(backup) if backup.version == 1 => Some(backup),
                    _ => None,
                }
            })
        })
    }

    fn create_backup(&self, file_path: &Path, version: u32) -> anyhow::Result<FileHistoryBackup> {
        if !file_path.exists() {
            return Ok(FileHistoryBackup {
                backup_file_name: None,
                version,
                backup_time: format_system_time_iso_ms(SystemTime::now()),
            });
        }
        let tracking_path = self.tracking_path(file_path)?;
        let backup_file_name = backup_file_name(&tracking_path, version);
        let backup_path = self.backup_path(&backup_file_name);
        if let Some(parent) = backup_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(file_path, &backup_path)?;
        Ok(FileHistoryBackup {
            backup_file_name: Some(backup_file_name),
            version,
            backup_time: format_system_time_iso_ms(SystemTime::now()),
        })
    }

    fn backup_path(&self, backup_file_name: &str) -> PathBuf {
        self.backups_dir().join(backup_file_name)
    }

    fn origin_changed_from_backup(
        &self,
        file_path: &Path,
        backup: &FileHistoryBackup,
    ) -> std::io::Result<bool> {
        let Some(backup_file_name) = backup.backup_file_name.as_deref() else {
            return Ok(file_path.exists());
        };
        let backup_path = self.backup_path(backup_file_name);
        if !file_path.exists() || !backup_path.exists() {
            return Ok(file_path.exists() != backup_path.exists());
        }
        let origin = fs::read(file_path)?;
        let backup = fs::read(backup_path)?;
        Ok(origin != backup)
    }

    fn tracking_path(&self, file_path: &Path) -> anyhow::Result<String> {
        tracking_key(&self.cwd, file_path)
    }

    fn expand_tracking_path(&self, tracking_path: &str) -> anyhow::Result<PathBuf> {
        // Out-of-cwd files are keyed by their absolute path (see `tracking_key`),
        // so restore them in place. In-cwd keys are cwd-relative and safe-joined.
        let raw = Path::new(tracking_path);
        if raw.is_absolute() {
            return Ok(raw.to_path_buf());
        }
        let relative = safe_relative_tracking_path(tracking_path)?;
        Ok(self.cwd.join(relative))
    }
}

fn open_existing_restore_target(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Permit ordinary readers/writers but deny delete/rename sharing so an
        // atomic-save cannot detach the object while it is being restored.
        options.share_mode(0x0000_0001 | 0x0000_0002);
    }
    options.open(path)
}

#[cfg(unix)]
fn opened_file_still_names_path(file: &fs::File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    Ok(opened.dev() == named.dev() && opened.ino() == named.ino())
}

#[cfg(windows)]
fn opened_file_still_names_path(_file: &fs::File, _path: &Path) -> std::io::Result<bool> {
    // The deny-delete share held by `open_existing_restore_target` binds the
    // directory entry for the lifetime of the handle.
    Ok(true)
}

#[cfg(not(any(unix, windows)))]
fn opened_file_still_names_path(_file: &fs::File, _path: &Path) -> std::io::Result<bool> {
    Ok(true)
}

fn read_optional_file(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn delete_opened_target_conditionally(
    file_path: &Path,
    expected_hash: Option<[u8; 32]>,
) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    const GENERIC_READ: u32 = 0x8000_0000;
    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;

    #[repr(C)]
    struct FileDispositionInfo {
        delete_file: i32,
    }
    #[link(name = "Kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            class: u32,
            information: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    let mut file = OpenOptions::new()
        .access_mode(GENERIC_READ | DELETE_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(file_path)?;
    let mut opened_bytes = Vec::new();
    file.read_to_end(&mut opened_bytes)?;
    if optional_hash(Some(&opened_bytes)) != expected_hash {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "target changed before the opened-object delete",
        ));
    }
    let information = FileDispositionInfo { delete_file: 1 };
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            4,
            (&information as *const FileDispositionInfo).cast(),
            std::mem::size_of::<FileDispositionInfo>() as u32,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The same contract as the Windows arm, built from what Unix does have.
///
/// This stood unimplemented — every call returned `Unsupported` — which meant
/// that on Linux and macOS a rewind never deleted anything. A file the turn
/// created stayed on disk and the restore reported an error nobody could act
/// on, because the platform, not the file, was what it was complaining about.
///
/// What the Windows side gets from the kernel, this assembles:
///
/// - **The bytes checked are the bytes of the object removed.** The file is
///   opened once and read through that handle, so the comparison is against an
///   object rather than against a path read twice.
/// - **The object still answers to this path.** `opened_file_still_names_path`
///   compares the open handle's device and inode against `symlink_metadata` of
///   the path — the same guard the write direction already commits behind. It
///   is also what makes a symlinked target safe: reading follows the link,
///   `remove_file` would unlink the link itself, and those are two different
///   inodes, so the mismatch refuses the delete instead of removing the wrong
///   name.
///
/// What it does not have is Windows' delete-on-close, which binds the removal
/// to the handle. The unlink resolves the path once more, so a rename landing
/// in that window would remove whatever holds the name by then. Narrowing it
/// further needs `unlinkat` against a pinned directory descriptor, which is a
/// dependency this crate does not carry. A caller that loses that race gets
/// `WouldBlock` from the guard above and reports a conflict, which is how the
/// write direction already handles the same race.
#[cfg(unix)]
fn delete_opened_target_conditionally(
    file_path: &Path,
    expected_hash: Option<[u8; 32]>,
) -> std::io::Result<()> {
    let mut file = OpenOptions::new().read(true).open(file_path)?;
    let mut opened_bytes = Vec::new();
    file.read_to_end(&mut opened_bytes)?;
    if optional_hash(Some(&opened_bytes)) != expected_hash {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "target changed before the opened-object delete",
        ));
    }
    if !opened_file_still_names_path(&file, file_path)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "target path identity changed before the opened-object delete",
        ));
    }
    fs::remove_file(file_path)
}

#[cfg(not(any(windows, unix)))]
fn delete_opened_target_conditionally(
    _file_path: &Path,
    _expected_hash: Option<[u8; 32]>,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "safe identity-bound deletion is unavailable on this platform; refusing to delete the path",
    ))
}

fn optional_hash(bytes: Option<&[u8]>) -> Option<[u8; 32]> {
    bytes.map(|bytes| Sha256::digest(bytes).into())
}

fn backup_file_name(tracking_path: &str, version: u32) -> String {
    let hash = fnv1a64(tracking_path.as_bytes());
    format!("{hash:016x}@v{version}")
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Compute the manifest tracking key for `file_path`.
///
/// Files inside `cwd` are keyed by their cwd-relative path, which keeps the
/// manifest portable. Files outside `cwd` have no cwd-relative form, so they
/// are keyed by their absolute path instead — that is what makes them
/// restorable rather than skipped (the on-disk backup is named by the key's
/// hash either way). Both forms are forward-slashed for a stable,
/// platform-independent key; `expand_tracking_path` tells them apart by
/// `Path::is_absolute`.
///
/// The inside/outside decision is case-insensitive on Windows, while the
/// relative path is derived by dropping the cwd's leading components (rather
/// than a case-sensitive `strip_prefix`). That keeps a correctly-nested file
/// trackable even when its prefix differs only in case, and preserves the
/// file's original casing in the key. `Err` is reserved for genuinely
/// malformed input — a path that escapes the filesystem root, or one that
/// points at `cwd` itself instead of a file beneath it.
fn tracking_key(cwd: &Path, file_path: &Path) -> anyhow::Result<String> {
    let cwd_abs = absolute_lexical(cwd)?;
    let file_abs = absolute_lexical(file_path)?;
    if !case_key(&file_abs).starts_with(case_key(&cwd_abs)) {
        // Outside cwd: no relative form exists, so key by absolute path.
        return Ok(file_abs.to_string_lossy().replace('\\', "/"));
    }
    let relative: PathBuf = file_abs
        .components()
        .skip(cwd_abs.components().count())
        .collect();
    if relative.as_os_str().is_empty() {
        anyhow::bail!("file path points at cwd, not a file");
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn absolute_lexical(path: &Path) -> anyhow::Result<PathBuf> {
    let mut base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            Component::RootDir => base.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !base.pop() {
                    anyhow::bail!("path escapes root: {}", path.display());
                }
            }
            Component::Normal(part) => base.push(part),
        }
    }
    Ok(base)
}

fn safe_relative_tracking_path(path: &str) -> anyhow::Result<PathBuf> {
    let raw = Path::new(path);
    if raw.is_absolute() {
        anyhow::bail!("tracking path must be relative: {path}");
    }
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("unsafe tracking path: {path}");
            }
        }
    }
    if out.as_os_str().is_empty() {
        anyhow::bail!("tracking path is empty");
    }
    Ok(out)
}

#[cfg(windows)]
fn case_key(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
}

#[cfg(not(windows))]
fn case_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn line_diff_counts(current: &str, target: &str) -> (usize, usize) {
    if current == target {
        return (0, 0);
    }
    (line_count(target), line_count(current))
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_dir() -> TempDir {
        tempfile::Builder::new()
            .prefix("rebon-file-history-test-")
            .tempdir()
            .unwrap()
    }

    fn store(root: &Path, cwd: &Path) -> FileHistoryStore {
        FileHistoryStore::new(root, cwd, "session-1")
    }

    fn active_lock(store: &FileHistoryStore) -> SessionActiveLock {
        crate::session_storage::try_acquire_session_active_lock(
            &store.projects_root,
            &store.cwd_display,
            &store.session_id,
        )
        .unwrap()
        .expect("test owns session lock")
    }

    fn apply_snapshot(
        store: &FileHistoryStore,
        message_id: &str,
    ) -> Result<FileHistoryApplyReport, FileHistoryApplyError> {
        // Production records the completed turn's authoritative head before the
        // mutator can run. Legacy fixtures use this helper to do the same.
        if store.can_restore(message_id) {
            store.record_current_head(message_id).unwrap();
        }
        let lock = active_lock(store);
        store.apply_snapshot(message_id, &lock)
    }

    #[test]
    fn track_snapshot_and_restore_update() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("demo.txt");
        fs::write(&file, "one").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();
        fs::write(&file, "two").unwrap();
        store.make_snapshot("u-1").unwrap();

        assert!(store.can_restore("u-1"));
        let diff = store.diff_stats("u-1").unwrap().unwrap();
        assert_eq!(diff.files_changed.len(), 1);
        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    }

    #[test]
    fn restore_deletes_file_created_after_snapshot() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("new.txt");
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();
        fs::write(&file, "created").unwrap();
        store.make_snapshot("u-1").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(!file.exists());
    }

    #[test]
    fn tracks_and_restores_file_outside_cwd() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        // A sibling of cwd: outside it, so keyed by absolute path rather than
        // skipped. The write already had permission; undo must still cover it.
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "one").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&outside, "u-1").unwrap();
        fs::write(&outside, "two").unwrap();
        store.make_snapshot("u-1").unwrap();

        assert!(store.can_restore("u-1"));
        let diff = store.diff_stats("u-1").unwrap().unwrap();
        assert_eq!(diff.files_changed.len(), 1);
        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(!report.has_errors());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "one");
    }

    /// A tracked path that has become a symlink is refused, not followed.
    ///
    /// The delete resolves the path once more, and on Unix reading follows a
    /// link while `remove_file` unlinks the link itself — so without an
    /// identity check the two halves would disagree about which object the
    /// operation is about. The device/inode guard is what catches that, and
    /// this pins it: the run reports a conflict, the link stays, and the file
    /// it points at is untouched.
    #[cfg(unix)]
    #[test]
    fn restore_refuses_to_delete_through_a_symlink() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let tracked = cwd.join("new.txt");
        let store = store(&projects, &cwd);

        store.track_before_write(&tracked, "u-1").unwrap();
        fs::write(&tracked, "created").unwrap();
        store.make_snapshot("u-1").unwrap();

        // The turn's file is replaced by a link to somebody else's, carrying
        // the same bytes so that only the identity check can tell them apart.
        let elsewhere = temp.path().join("elsewhere.txt");
        fs::write(&elsewhere, "created").unwrap();
        fs::remove_file(&tracked).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &tracked).unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 0);
        assert_eq!(report.skipped, 1);
        assert!(
            fs::symlink_metadata(&tracked).is_ok(),
            "the link was removed instead of being refused"
        );
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "created");
    }

    #[test]
    fn restore_deletes_outside_file_created_after_snapshot() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let outside = temp.path().join("created-outside.txt");
        let store = store(&projects, &cwd);

        store.track_before_write(&outside, "u-1").unwrap();
        fs::write(&outside, "created").unwrap();
        store.make_snapshot("u-1").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(!outside.exists());
    }

    #[test]
    fn corrupt_manifest_is_reported() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let store = store(&projects, &cwd);
        fs::create_dir_all(store.file_history_dir()).unwrap();
        fs::write(store.manifest_path(), b"{").unwrap();

        assert!(matches!(
            store.load_manifest(),
            Err(FileHistoryLoadError::Corrupt(_))
        ));
    }

    // --- rewind safety: restore writes/deletes real files, so the dangerous
    // edges below (wrong location, wrong version, traversal, missing backup)
    // must each fail safe rather than corrupt or escape. ---

    /// A sibling dir whose name *string* is a prefix of cwd (`proj` vs
    /// `proj-sibling`) must be classified OUTSIDE cwd. Component-wise matching
    /// guards against a naive string `starts_with` that would mis-key the
    /// sibling as in-cwd and later restore it into the wrong directory.
    #[test]
    fn sibling_directory_is_treated_as_outside_cwd() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let sibling_dir = temp.path().join("proj-sibling");
        fs::create_dir_all(&sibling_dir).unwrap();
        let sibling_file = sibling_dir.join("x.txt");
        fs::write(&sibling_file, "one").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&sibling_file, "u-1").unwrap();
        fs::write(&sibling_file, "two").unwrap();
        store.make_snapshot("u-1").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(!report.has_errors());
        // Restored in place (its own dir), never rebased under cwd.
        assert_eq!(fs::read_to_string(&sibling_file).unwrap(), "one");
        assert!(!cwd.join("x.txt").exists());
    }

    /// One turn edits a file inside cwd and one outside it; the snapshot must
    /// carry both and restore each to its own location.
    #[test]
    fn restores_mixed_inside_and_outside_files_in_one_snapshot() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let inside = cwd.join("in.txt");
        let outside = temp.path().join("out.txt");
        fs::write(&inside, "in-one").unwrap();
        fs::write(&outside, "out-one").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&inside, "u-1").unwrap();
        store.track_before_write(&outside, "u-1").unwrap();
        fs::write(&inside, "in-two").unwrap();
        fs::write(&outside, "out-two").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 2);
        assert!(!report.has_errors());
        assert_eq!(fs::read_to_string(&inside).unwrap(), "in-one");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "out-one");
    }

    /// A tracking key containing `..` (only possible via a corrupted/tampered
    /// manifest — the API never emits one) must be refused on expand, so
    /// restore can never escape cwd. Even with a valid backup blob present,
    /// nothing is written to the traversal target.
    #[test]
    fn traversal_tracking_key_is_refused_not_written_outside_cwd() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let store = store(&projects, &cwd);

        let manifest = FileHistoryManifest {
            version: 1,
            cwd: cwd.to_string_lossy().to_string(),
            session_id: "session-1".to_string(),
            tracked_files: BTreeSet::from(["../evil.txt".to_string()]),
            snapshots: vec![FileHistorySnapshot {
                message_id: "u-1".to_string(),
                tracked_file_backups: BTreeMap::from([(
                    "../evil.txt".to_string(),
                    FileHistoryBackup {
                        backup_file_name: Some("dead@v1".to_string()),
                        version: 1,
                        backup_time: "t".to_string(),
                    },
                )]),
                timestamp: "t".to_string(),
            }],
            current_head: None,
            snapshot_sequence: 1,
        };
        fs::create_dir_all(store.file_history_dir()).unwrap();
        fs::write(
            store.manifest_path(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let backups = store.file_history_dir().join("backups");
        fs::create_dir_all(&backups).unwrap();
        fs::write(backups.join("dead@v1"), "PWNED").unwrap();

        let evil = temp.path().join("evil.txt"); // where `cwd/../evil.txt` resolves
        assert!(!evil.exists());
        let error = store
            .apply_snapshot("u-1", &active_lock(&store))
            .unwrap_err();
        assert!(matches!(error, FileHistoryApplyError::Unavailable(_)));
        assert!(!evil.exists(), "traversal key must never write outside cwd");
    }

    /// Restoring an OLDER snapshot must yield the OLDER content, not the latest
    /// backup — picking the wrong version would silently corrupt the file.
    /// Exercised on an out-of-cwd (absolute-keyed) file.
    #[test]
    fn restores_correct_version_across_snapshots() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let outside = temp.path().join("ver.txt");
        fs::write(&outside, "A").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&outside, "u-1").unwrap(); // v1 = "A"
        fs::write(&outside, "B").unwrap();
        store.make_snapshot("u-2").unwrap(); // v2 = "B"
        fs::write(&outside, "C").unwrap();

        let r2 = apply_snapshot(&store, "u-2").unwrap();
        assert_eq!(r2.restored, 1);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "B");

        let r1 = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(r1.restored, 1);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "A");
    }

    /// If the user deletes a tracked out-of-cwd file, restore must bring it
    /// back from the backup rather than leaving it missing.
    #[test]
    fn restore_recreates_user_deleted_outside_file() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let outside = temp.path().join("deleteme.txt");
        fs::write(&outside, "keep").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&outside, "u-1").unwrap();
        fs::remove_file(&outside).unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(outside.exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    }

    /// When the file already matches the snapshot, restore must be a no-op:
    /// no spurious rewrite (which would churn mtime and risk clobbering).
    #[test]
    fn restore_is_noop_when_content_already_matches() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let outside = temp.path().join("same.txt");
        fs::write(&outside, "same").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&outside, "u-1").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 0);
        assert!(!report.has_errors());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "same");
    }

    /// A backup blob vanishing from disk (manual cleanup / corruption) must be
    /// reported and skipped — never restored as empty/garbage over the origin.
    #[test]
    fn missing_backup_file_is_reported_without_corrupting_origin() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("g.txt");
        fs::write(&file, "orig").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();
        fs::write(&file, "changed").unwrap();
        fs::remove_dir_all(store.file_history_dir().join("backups")).unwrap();

        let error = apply_snapshot(&store, "u-1").unwrap_err();
        assert!(matches!(error, FileHistoryApplyError::Unavailable(_)));
        // Origin left intact, not truncated or blanked.
        assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
    }

    /// Restoring an unknown snapshot id reports an error and writes nothing.
    #[test]
    fn apply_unknown_snapshot_reports_error_without_writing() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("h.txt");
        fs::write(&file, "x").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();

        let error = apply_snapshot(&store, "does-not-exist").unwrap_err();
        assert!(matches!(
            error,
            FileHistoryApplyError::Unavailable(FileRestoreUnavailableReason::MissingSnapshot)
        ));
        assert_eq!(fs::read_to_string(&file).unwrap(), "x");
    }

    #[test]
    fn restore_preflight_requires_recorded_expected_current_and_detects_manual_edit() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("safe.txt");
        fs::write(&file, "before").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();
        fs::write(&file, "after turn").unwrap();
        store.record_current_head("u-1").unwrap();

        let FileRestoreCapability::Clean(clean) = store.restore_capability("u-1") else {
            panic!("recorded post-turn bytes should produce a clean preflight");
        };
        assert_eq!(clean.writes, 1);
        assert_eq!(clean.conflicts, 0);
        assert_ne!(clean.paths[0].desired_hash, clean.paths[0].current_hash);

        fs::write(&file, "manual edit").unwrap();
        let FileRestoreCapability::Conflicted(conflicted) = store.restore_capability("u-1") else {
            panic!("manual edit must conflict instead of being overwritten");
        };
        assert_eq!(conflicted.conflicts, 1);
        assert!(conflicted.paths[0].conflict_reason.is_some());
        let error = store
            .apply_snapshot("u-1", &active_lock(&store))
            .unwrap_err();
        assert!(matches!(error, FileHistoryApplyError::Conflicted(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "manual edit");
    }

    #[test]
    fn newest_and_three_turn_same_file_rewind_use_completed_head() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("head.txt");
        fs::write(&file, "zero").unwrap();
        let store = store(&projects, &cwd);

        for (message, content) in [("u-1", "one"), ("u-2", "two"), ("u-3", "three")] {
            store.make_snapshot(message).unwrap();
            store.track_before_write(&file, message).unwrap();
            fs::write(&file, content).unwrap();
            store.record_current_head(message).unwrap();
        }

        let FileRestoreCapability::Clean(newest) = store.restore_capability("u-3") else {
            panic!("the newest completed edited turn must be rewindable");
        };
        assert_eq!(newest.writes, 1);
        let FileRestoreCapability::Clean(oldest) = store.restore_capability("u-1") else {
            panic!("known later same-file turns must compare against the latest head");
        };
        assert_eq!(oldest.conflicts, 0);

        let report = store.apply_snapshot("u-1", &active_lock(&store)).unwrap();
        assert_eq!(report.restored, 1);
        assert_eq!(fs::read_to_string(file).unwrap(), "zero");
    }

    #[test]
    fn concurrent_manifest_transactions_retain_both_files() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let first = cwd.join("first.txt");
        let second = cwd.join("second.txt");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let store = store(&projects, &cwd);
        store.make_snapshot("u-1").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = [first, second]
            .into_iter()
            .map(|path| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.track_before_write(&path, "u-1").unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let manifest = store.load_manifest().unwrap();
        assert_eq!(manifest.tracked_files.len(), 2);
        assert_eq!(manifest.snapshots[0].tracked_file_backups.len(), 2);
    }

    #[test]
    fn restore_preflight_refuses_unknown_expected_current_state() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("unknown.txt");
        fs::write(&file, "before").unwrap();
        let store = store(&projects, &cwd);

        store.track_before_write(&file, "u-1").unwrap();
        fs::write(&file, "unrecorded post-turn bytes").unwrap();

        assert!(matches!(
            store.restore_capability("u-1"),
            FileRestoreCapability::Unavailable(
                FileRestoreUnavailableReason::UnknownExpectedCurrent(_)
            )
        ));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "unrecorded post-turn bytes"
        );
    }

    /// On Windows a file referenced through a differently-cased cwd prefix
    /// (`f:\project\...` vs `F:\Project\...`) must still be tracked by its
    /// RELATIVE key and restored to the real file — not mis-keyed as an
    /// absolute out-of-cwd path. Guards the case-insensitive prefix decision.
    #[cfg(windows)]
    #[test]
    fn windows_tracks_file_with_case_differing_prefix() {
        let temp = temp_dir();
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("Project");
        fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("Case.txt");
        fs::write(&file, "one").unwrap();
        let store = store(&projects, &cwd);

        let lower_cwd = PathBuf::from(cwd.to_string_lossy().to_lowercase());
        let aliased = lower_cwd.join("Case.txt");

        store.track_before_write(&aliased, "u-1").unwrap();
        fs::write(&file, "two").unwrap();

        let report = apply_snapshot(&store, "u-1").unwrap();
        assert_eq!(report.restored, 1);
        assert!(!report.has_errors());
        assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    }
}
