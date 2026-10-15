//! Durable authority for session moves and Git rename effects.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::storage::{atomic_write_verified, sync_parent_directory};

/// All current producers record complete authority before their first effect.
pub(crate) const MOVE_JOURNAL_VERSION: u32 = 2;

const JOURNAL_DIR_NAME: &str = ".move-journal";

/// A recorded identity, never a replacement resource recaptured during recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PathAuthority {
    pub(crate) path: PathBuf,
    pub(crate) resolved: PathBuf,
    pub(crate) identity: Option<(u64, u64)>,
    pub(crate) directory: bool,
    pub(crate) contents: Option<Vec<u8>>,
}

impl PathAuthority {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        let resolved = super::deletion::resolve_claim_path(path)
            .context("journal resource path cannot be resolved")?;
        let metadata = match fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let directory = metadata.as_ref().is_some_and(fs::Metadata::is_dir);
        let contents = if metadata.is_some() && !directory {
            Some(fs::read(path)?)
        } else {
            None
        };
        Ok(Self {
            path: path.to_path_buf(),
            resolved,
            identity: metadata.as_ref().map(super::storage::filesystem_identity),
            directory,
            contents,
        })
    }

    pub(crate) fn verify(&self) -> Result<()> {
        anyhow::ensure!(
            super::deletion::resolve_claim_path(&self.path).as_ref() == Some(&self.resolved),
            "journal resource route changed: {}",
            self.path.display()
        );
        self.verify_at(&self.path)
    }

    pub(crate) fn verify_at(&self, path: &Path) -> Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.as_ref().map(super::storage::filesystem_identity) == self.identity
                && metadata.as_ref().is_some_and(fs::Metadata::is_dir) == self.directory,
            "journal resource was replaced: {}",
            path.display()
        );
        if let Some(contents) = &self.contents {
            anyhow::ensure!(
                fs::read(path)? == *contents,
                "journal resource contents changed: {}",
                path.display()
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProfileAuthority {
    pub(crate) profile: String,
    pub(crate) sessions_path: PathBuf,
    pub(crate) identity: (u64, u64),
}

impl ProfileAuthority {
    pub(crate) fn capture(storage: &super::Storage) -> Result<Self> {
        Ok(Self {
            profile: storage.profile().to_owned(),
            sessions_path: storage.sessions_path().to_path_buf(),
            identity: storage.physical_profile_identity()?,
        })
    }

    pub(crate) fn matches(&self, storage: &super::Storage) -> Result<bool> {
        Ok(storage.physical_profile_identity()? == self.identity)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckoutAuthority {
    pub(crate) checkout: PathAuthority,
    pub(crate) parent: PathAuthority,
    pub(crate) git: PathAuthority,
    pub(crate) common: PathAuthority,
    pub(crate) head: PathAuthority,
    pub(crate) tip: String,
}

impl CheckoutAuthority {
    pub(crate) fn verify_directories_at(&self, path: &Path) -> Result<()> {
        self.parent.verify()?;
        self.checkout.verify_at(path)?;
        let repo = git2::Repository::open(path)?;
        self.git.verify_at(repo.path())?;
        self.common.verify_at(repo.commondir())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BranchAuthority {
    pub(crate) repository: PathAuthority,
    pub(crate) git: PathAuthority,
    pub(crate) common: PathAuthority,
    pub(crate) branch: String,
    pub(crate) reference: String,
    pub(crate) tip: String,
    pub(crate) loose: PathAuthority,
    pub(crate) packed: PathAuthority,
    pub(crate) routes: Vec<PathAuthority>,
}

impl BranchAuthority {
    pub(crate) fn verify_directories(&self) -> Result<()> {
        self.repository.verify()?;
        let repo = git2::Repository::open(&self.repository.path)?;
        self.git.verify_at(repo.path())?;
        self.common.verify_at(repo.commondir())?;
        for route in &self.routes {
            route.verify()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RowDelta {
    pub(crate) before: super::Instance,
    pub(crate) after: super::Instance,
}

pub(crate) fn row_resources_match(current: &super::Instance, expected: &super::Instance) -> bool {
    current.id == expected.id
        && current.created_at == expected.created_at
        && current.lifecycle_generation == expected.lifecycle_generation
        && current.project_path == expected.project_path
        && current.worktree_info == expected.worktree_info
        && current.pre_trash_project_path == expected.pre_trash_project_path
        && current.trashed_at == expected.trashed_at
        && current.trash_lifecycle_id == expected.trash_lifecycle_id
        && current.workspace_info.as_ref().map(|workspace| {
            (
                &workspace.workspace_dir,
                &workspace.branch,
                &workspace.repos,
                workspace.created_at,
                workspace.cleanup_on_delete,
            )
        }) == expected.workspace_info.as_ref().map(|workspace| {
            (
                &workspace.workspace_dir,
                &workspace.branch,
                &workspace.repos,
                workspace.created_at,
                workspace.cleanup_on_delete,
            )
        })
}

pub(crate) fn row_matches_delta(current: &super::Instance, delta: &RowDelta, post: bool) -> bool {
    let expected = if post { &delta.after } else { &delta.before };
    row_resources_match(current, expected)
        && (current.lifecycle_reservation == expected.lifecycle_reservation
            || (!post
                && current.lifecycle_reservation.is_none()
                && expected.lifecycle_reservation_is_owned(
                    super::LifecycleOperation::Rename,
                    expected.lifecycle_generation,
                )))
        && (delta.before.title == delta.after.title || current.title == expected.title)
        && (delta.before.group_path == delta.after.group_path
            || current.group_path == expected.group_path)
        && (delta.before.tool == delta.after.tool || current.tool == expected.tool)
        && (delta.before.command == delta.after.command || current.command == expected.command)
        && (delta.before.extra_args == delta.after.extra_args
            || current.extra_args == expected.extra_args)
}

/// Removing a source copy must not discard an edit absent from its target.
pub(crate) fn source_changes_preserved(
    before: &super::Instance,
    source: &super::Instance,
    target: &super::Instance,
) -> Result<bool> {
    let before = serde_json::to_value(before)?;
    let source = serde_json::to_value(source)?;
    let target = serde_json::to_value(target)?;
    let before = before
        .as_object()
        .context("original session snapshot is not an object")?;
    let source = source
        .as_object()
        .context("session source snapshot is not an object")?;
    Ok(source
        .iter()
        .all(|(field, value)| before.get(field) == Some(value) || target.get(field) == Some(value))
        && before
            .keys()
            .filter(|field| !source.contains_key(*field))
            .all(|field| target.get(field).is_none()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectPhase {
    Prepared,
    BranchPending,
    BranchRemoved,
    BranchApplied,
    MovePending,
    MoveApplied,
    Published,
    Compensating,
    Compensated,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EffectObservation {
    pub(crate) old_ref: Option<String>,
    pub(crate) new_ref: Option<String>,
    pub(crate) checkout_path: Option<PathBuf>,
    pub(crate) checkout_head: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RenameIntent {
    pub(crate) source: ProfileAuthority,
    pub(crate) target: ProfileAuthority,
    pub(crate) row: RowDelta,
    pub(crate) checkout: Option<CheckoutAuthority>,
    pub(crate) branch: Option<BranchAuthority>,
    pub(crate) phase: EffectPhase,
    pub(crate) observation: Option<EffectObservation>,
    pub(crate) branch_receipt: Option<PathAuthority>,
    pub(crate) checkout_receipt: Option<CheckoutAuthority>,
    pub(crate) compensation_receipt: Option<PathAuthority>,
    pub(crate) conflict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MoveAuthority {
    Metadata {
        source: ProfileAuthority,
        target: ProfileAuthority,
        rows: Vec<RowDelta>,
    },
    Rename {
        intent: Box<RenameIntent>,
    },
    LegacyConflict {
        legacy: serde_json::Value,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MoveJournalEntry {
    pub(crate) version: u32,
    /// Sorted session ids covered by one move batch.
    pub(crate) ids: Vec<String>,
    pub(crate) source_profile: String,
    pub(crate) target_profile: String,
    /// Absolute paths of both profiles' `sessions.json`, as recorded at
    /// journal-write time. Recovery re-checks them against the live stores.
    pub(crate) source_sessions_path: PathBuf,
    pub(crate) target_sessions_path: PathBuf,
    pub(crate) group_move_source_path: String,
    pub(crate) group_move_target_path: String,
    pub(crate) group_move_subtree: bool,
    pub(crate) created_at_epoch_ms: u64,
    pub(crate) authority: MoveAuthority,
    #[serde(skip_serializing)]
    pub(crate) live_lock_identity: Option<(u64, u64)>,
}

impl MoveJournalEntry {
    pub(crate) fn is_current(&self) -> bool {
        self.version == MOVE_JOURNAL_VERSION
    }
}

pub(crate) fn journal_dir() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join(JOURNAL_DIR_NAME))
}

/// The stable flock is separate from the atomically rewritten JSON inode.
#[derive(Debug)]
pub(crate) struct JournalTransaction {
    pub(crate) path: PathBuf,
    _lease: fs::File,
}

impl JournalTransaction {
    pub(crate) fn rewrite(&self, entry: &MoveJournalEntry) -> Result<()> {
        rewrite_with_lease(&self.path, &self._lease, entry)
    }

    pub(crate) fn read(&self) -> Result<MoveJournalEntry> {
        Ok(serde_json::from_slice(&fs::read(&self.path)?)?)
    }

    pub(crate) fn lease(&self) -> &fs::File {
        &self._lease
    }

    fn bytes(&self, entry: &MoveJournalEntry) -> Result<Vec<u8>> {
        entry_bytes(entry, &self._lease)
    }
}

fn entry_bytes(entry: &MoveJournalEntry, lease: &fs::File) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Stored<'a> {
        #[serde(flatten)]
        entry: &'a MoveJournalEntry,
        live_lock_identity: (u64, u64),
    }
    Ok(serde_json::to_vec_pretty(&Stored {
        entry,
        live_lock_identity: super::storage::filesystem_identity(&lease.metadata()?),
    })?)
}

pub(crate) fn rewrite_with_lease(
    path: &Path,
    lease: &fs::File,
    entry: &MoveJournalEntry,
) -> Result<()> {
    anyhow::ensure!(
        super::storage::filesystem_identity(&lease.metadata()?)
            == super::storage::filesystem_identity(&fs::metadata(path.with_extension("lock"))?),
        "live journal lease was replaced"
    );
    atomic_write_verified(path, &entry_bytes(entry, lease)?)?;
    sync_parent_directory(path)
}

fn open_lease(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.with_extension("lock"))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "journal lease is not a regular file"
    );
    Ok(file)
}

pub(crate) fn try_recovery_lease(
    path: &Path,
    entry: &MoveJournalEntry,
) -> Result<Option<fs::File>> {
    use fs2::FileExt;
    let file = open_lease(path)?;
    anyhow::ensure!(
        Some(super::storage::filesystem_identity(&file.metadata()?)) == entry.live_lock_identity,
        "journal lease identity changed or is absent"
    );
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn record_live(entry: &MoveJournalEntry) -> Result<JournalTransaction> {
    record_with_sync(entry, &journal_dir()?, sync_parent_directory)
}

/// Model an interrupted producer in recovery fixtures.
#[cfg(test)]
pub(crate) fn record(entry: &MoveJournalEntry) -> Result<PathBuf> {
    Ok(record_live(entry)?.path.clone())
}

fn record_with_sync<S>(
    entry: &MoveJournalEntry,
    dir: &Path,
    mut sync: S,
) -> Result<JournalTransaction>
where
    S: FnMut(&Path) -> Result<()>,
{
    use fs2::FileExt;
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    sync(dir)
        .with_context(|| format!("journal directory {} was not made durable", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = dir.join(format!("move-{nanos}-{}.json", uuid::Uuid::new_v4()));
    let lease = open_lease(&path)?;
    lease.lock_exclusive()?;
    let transaction = JournalTransaction {
        path,
        _lease: lease,
    };
    atomic_write_verified(&transaction.path, &transaction.bytes(entry)?).with_context(|| {
        format!(
            "failed to write move journal {}",
            transaction.path.display()
        )
    })?;
    sync(&transaction.path).with_context(|| {
        format!(
            "move journal {} was not made durable",
            transaction.path.display()
        )
    })?;
    Ok(transaction)
}

/// Nanosecond creation order encoded in record's filename. Used only as a
/// durable tie-breaker when two entries share the millisecond JSON timestamp.
pub(crate) fn file_created_at_nanos(path: &Path) -> Option<u128> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("move-"))
        .and_then(|name| name.split_once('-').map(|(nanos, _)| nanos))
        .and_then(|nanos| nanos.parse().ok())
}

/// Delete one consumed entry and sync its parent directory so the removal
/// itself is durable. Idempotent: a missing file is already consumed.
pub(crate) fn consume(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()))
        }
    }
    match fs::remove_file(path.with_extension("lock")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(path)
        .with_context(|| format!("removal of {} was not made durable", path.display()))
}

/// Result of scanning the one application-root journal.
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<(PathBuf, std::result::Result<MoveJournalEntry, String>)>,
    pub(crate) unreadable_dirs: Vec<(PathBuf, String)>,
}

pub(crate) fn scan() -> ScanResult {
    let dir = match journal_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return ScanResult {
                entries: Vec::new(),
                unreadable_dirs: vec![(
                    PathBuf::from(JOURNAL_DIR_NAME),
                    format!("resolving application journal: {error:#}"),
                )],
            }
        }
    };
    scan_in(&dir)
}

fn scan_in(dir: &Path) -> ScanResult {
    let mut result = ScanResult {
        entries: Vec::new(),
        unreadable_dirs: Vec::new(),
    };
    {
        let read_dir = match fs::read_dir(dir) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return result,
            Err(error) => {
                result.unreadable_dirs.push((
                    dir.to_path_buf(),
                    format!("failed to list {}: {error}", dir.display()),
                ));
                return result;
            }
        };
        let mut paths = Vec::new();
        for entry in read_dir {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                    paths.push(entry.path())
                }
                Ok(_) => {}
                Err(error) => {
                    result.unreadable_dirs.push((
                        dir.to_path_buf(),
                        format!("incomplete journal inventory: {error}"),
                    ));
                    return result;
                }
            }
        }
        paths.sort();
        for path in paths {
            let parsed = fs::read(&path)
                .context("failed to read move journal entry")
                .and_then(|bytes| {
                    serde_json::from_slice::<MoveJournalEntry>(&bytes)
                        .context("malformed move journal entry")
                })
                .map_err(|error| format!("{error:#}"))
                .and_then(|entry| {
                    if entry.is_current() {
                        Ok(entry)
                    } else {
                        Err(format!(
                            "move journal version {} is not supported (current: {MOVE_JOURNAL_VERSION})",
                            entry.version
                        ))
                    }
                });
            result.entries.push((path, parsed));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &Path, target: &Path) -> MoveJournalEntry {
        MoveJournalEntry {
            version: MOVE_JOURNAL_VERSION,
            ids: vec!["session-id".to_string()],
            source_profile: "source".to_string(),
            target_profile: "target".to_string(),
            source_sessions_path: source.to_path_buf(),
            target_sessions_path: target.to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: 1,
            authority: MoveAuthority::Metadata {
                source: ProfileAuthority {
                    profile: "source".into(),
                    sessions_path: source.to_path_buf(),
                    identity: super::super::storage::filesystem_identity(
                        &fs::metadata(source.parent().unwrap()).unwrap(),
                    ),
                },
                target: ProfileAuthority {
                    profile: "target".into(),
                    sessions_path: target.to_path_buf(),
                    identity: super::super::storage::filesystem_identity(
                        &fs::metadata(target.parent().unwrap()).unwrap(),
                    ),
                },
                rows: Vec::new(),
            },
            live_lock_identity: None,
        }
    }

    #[test]
    fn record_requires_profile_parent_barrier_before_writing_entry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let expected_dir = temp.path().join(".move-journal");
        let mut calls = Vec::new();

        let error = record_with_sync(&entry(&source, &target), &expected_dir, |path| {
            calls.push(path.to_path_buf());
            Err(anyhow::anyhow!("forced profile-parent barrier failure"))
        })
        .expect_err("journal record must fail before an unverified directory can be used");

        assert!(error.to_string().contains("journal directory"));
        assert_eq!(calls, vec![expected_dir.clone()]);
        assert!(
            expected_dir.exists(),
            "directory was created before its barrier"
        );
        assert!(
            fs::read_dir(expected_dir)?.next().is_none(),
            "no entry may be written before the parent barrier"
        );
        Ok(())
    }

    #[test]
    fn scan_separates_directory_failures_from_entry_results() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let sessions = temp.path().join("profile/sessions.json");
        fs::create_dir_all(sessions.parent().unwrap())?;
        let journal_dir = temp.path().join(".move-journal");
        fs::write(&journal_dir, b"not-a-directory")?;

        let result = scan_in(&journal_dir);

        assert!(result.entries.is_empty());
        assert_eq!(result.unreadable_dirs.len(), 1);
        assert_eq!(result.unreadable_dirs[0].0, journal_dir);
        Ok(())
    }
}
