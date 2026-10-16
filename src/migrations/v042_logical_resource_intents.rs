//! Retain physical v2 evidence as scoped conflicts, never logical authority.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::session::move_journal::{
    LegacyScope, MoveAuthority, MoveJournalEntry, ProfileAuthority, ResourceKey,
};

#[derive(Deserialize)]
struct Header {
    version: u32,
    ids: Vec<String>,
    source_profile: String,
    target_profile: String,
    source_sessions_path: PathBuf,
    target_sessions_path: PathBuf,
    group_move_source_path: String,
    group_move_target_path: String,
    group_move_subtree: bool,
    created_at_epoch_ms: u64,
}

#[derive(Deserialize)]
struct RowIdentity {
    id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    lifecycle_generation: u64,
    #[serde(default)]
    lifecycle_reservation: Option<crate::session::LifecycleReservation>,
}

#[derive(Deserialize)]
struct RecordedRow {
    #[serde(flatten)]
    identity: RowIdentity,
    project_path: String,
    worktree_info: Option<crate::session::WorktreeInfo>,
    pre_trash_project_path: Option<String>,
    workspace_info: Option<crate::session::WorkspaceInfo>,
}

struct HistoricalProfile {
    authority: ProfileAuthority,
    identity: (u64, u64),
}

pub fn run() -> Result<()> {
    use fs2::FileExt;
    let root = crate::session::storage::acquire_ownership_lock()?;
    let names = crate::session::list_profiles_for_worktree_inventory()?;
    let mut profiles = Vec::with_capacity(names.len());
    for profile in names {
        let directory = root.app_dir().join("profiles").join(&profile);
        let metadata = fs::metadata(&directory)?;
        profiles.push(HistoricalProfile {
            authority: ProfileAuthority {
                profile,
                sessions_path: crate::session::deletion::resolve_claim_path(
                    &directory.join("sessions.json"),
                )
                .context("migration profile route cannot be resolved")?,
            },
            identity: (metadata.dev(), metadata.ino()),
        });
    }
    let directory = root.app_dir().join(".move-journal");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                matches!(fs::symlink_metadata(&directory), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "journal directory is an unreadable alias"
            );
            return Ok(());
        }
        Err(error) => return Err(error).context("listing journal migration inventory"),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            anyhow::ensure!(
                entry.file_type()?.is_file(),
                "journal is not a regular file"
            );
            paths.push(entry.path());
        }
    }
    paths.sort();
    for path in paths {
        let bytes = fs::read(&path)?;
        let header: Header = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding journal header {}", path.display()))?;
        if header.version == 3 {
            let _: MoveJournalEntry = serde_json::from_slice(&bytes)?;
            continue;
        }
        anyhow::ensure!(
            header.version == 2,
            "unsupported historical journal version {}",
            header.version
        );
        let lease = open_lease(&path)?;
        lease.try_lock_exclusive().with_context(|| {
            format!(
                "journal {} still has a live producer; retry after it finishes",
                path.display()
            )
        })?;
        // The Root fence excludes journal writers while the historical lease is checked.
        anyhow::ensure!(
            fs::read(&path)? == bytes,
            "journal changed during migration"
        );
        let legacy: Value = serde_json::from_slice(&bytes)?;
        let (scope, recorded) = legacy_scope(&header, &legacy, &profiles)?;
        crate::session::backup_before_migration(&path)?;
        associate_reservations(&path, &scope, &recorded, &profiles)?;
        let migrated = MoveJournalEntry {
            version: 3,
            ids: header.ids,
            source_profile: header.source_profile,
            target_profile: header.target_profile,
            source_sessions_path: header.source_sessions_path,
            target_sessions_path: header.target_sessions_path,
            group_move_source_path: header.group_move_source_path,
            group_move_target_path: header.group_move_target_path,
            group_move_subtree: header.group_move_subtree,
            created_at_epoch_ms: header.created_at_epoch_ms,
            authority: MoveAuthority::LegacyConflict {
                legacy,
                reason: "v2 physical authority cannot authorize logical recovery; known resource scope is retained for inspection".into(),
                scope,
            },
        };
        crate::session::storage::atomic_write_verified(
            &path,
            &serde_json::to_vec_pretty(&migrated)?,
        )?;
        crate::session::storage::sync_parent_directory(&path)?;
        tracing::info!(journal = %path.display(), "v042: preserved physical journal as a non-authoritative logical conflict");
    }
    Ok(())
}

fn open_lease(path: &Path) -> Result<fs::File> {
    let lock = path.with_extension("lock");
    let open = |create| {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock)
    };
    let file = match open(false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => open(true)?,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "journal lease is not a regular file"
    );
    Ok(file)
}

fn add_profile(scope: &mut LegacyScope, profile: ProfileAuthority) {
    if !scope.profiles.contains(&profile) {
        scope.profiles.push(profile);
    }
}

fn add_path(scope: &mut LegacyScope, path: &str, recursive: bool) -> Result<()> {
    let key = ResourceKey::path(Path::new(path), recursive)?;
    if !scope.resources.contains(&key) {
        scope.resources.push(key);
    }
    Ok(())
}

fn add_branch(
    scope: &mut LegacyScope,
    repository: &str,
    branch: &str,
    hint: Option<&Path>,
) -> Result<()> {
    let common = if let Some(hint) = hint {
        hint.to_path_buf()
    } else {
        match crate::git::open_repo_at(Path::new(repository)) {
            Ok(repo) => fs::canonicalize(repo.commondir())?,
            // Missing historical repositories provide no common-directory authority.
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    };
    let key = ResourceKey::Branch {
        repository: common,
        reference: if branch.starts_with("refs/") {
            branch.to_owned()
        } else {
            format!("refs/heads/{branch}")
        },
    };
    if !scope.resources.contains(&key) {
        scope.resources.push(key);
    }
    Ok(())
}

fn legacy_scope(
    header: &Header,
    legacy: &Value,
    profiles: &[HistoricalProfile],
) -> Result<(LegacyScope, Vec<RowIdentity>)> {
    let mut scope = LegacyScope::default();
    for (profile, sessions_path) in [
        (&header.source_profile, &header.source_sessions_path),
        (&header.target_profile, &header.target_sessions_path),
    ] {
        add_profile(
            &mut scope,
            ProfileAuthority {
                profile: profile.clone(),
                sessions_path: crate::session::deletion::resolve_claim_path(sessions_path)
                    .context("historical profile route cannot be resolved")?,
            },
        );
    }
    for pointer in [
        "/authority/source",
        "/authority/target",
        "/authority/intent/source",
        "/authority/intent/target",
    ] {
        if let Some(identity) = legacy
            .pointer(pointer)
            .and_then(|profile| profile.get("identity"))
            .and_then(Value::as_array)
        {
            let identity = identity
                .first()
                .and_then(Value::as_u64)
                .zip(identity.get(1).and_then(Value::as_u64));
            for profile in profiles
                .iter()
                .filter(|profile| Some(profile.identity) == identity)
            {
                // Old identities identify caution scope only, never permission for an effect.
                add_profile(&mut scope, profile.authority.clone());
            }
        }
    }
    let common = legacy
        .pointer("/authority/intent/branch/common/resolved")
        .and_then(Value::as_str)
        .map(Path::new);
    let mut snapshots = Vec::new();
    if let Some(rows) = legacy.pointer("/authority/rows").and_then(Value::as_array) {
        for row in rows {
            snapshots.push(
                row.get("before")
                    .context("historical move has no before snapshot")?,
            );
            snapshots.push(
                row.get("after")
                    .context("historical move has no after snapshot")?,
            );
        }
    }
    if let Some(row) = legacy.pointer("/authority/intent/row") {
        snapshots.push(
            row.get("before")
                .context("historical rename has no before snapshot")?,
        );
        snapshots.push(
            row.get("after")
                .context("historical rename has no after snapshot")?,
        );
    }
    let mut recorded = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        let row =
            RecordedRow::deserialize(snapshot).context("decoding historical resource scope")?;
        add_path(&mut scope, &row.project_path, row.worktree_info.is_some())?;
        if let Some(path) = &row.pre_trash_project_path {
            add_path(&mut scope, path, row.worktree_info.is_some())?;
        }
        if let Some(worktree) = &row.worktree_info {
            add_branch(
                &mut scope,
                &worktree.main_repo_path,
                &worktree.branch,
                common,
            )?;
        }
        if let Some(workspace) = &row.workspace_info {
            add_path(&mut scope, &workspace.workspace_dir, true)?;
            for repo in &workspace.repos {
                add_path(&mut scope, &repo.worktree_path, true)?;
                add_branch(&mut scope, &repo.main_repo_path, &repo.branch, None)?;
            }
        }
        recorded.push(row.identity);
    }
    Ok((scope, recorded))
}

fn associate_reservations(
    journal: &Path,
    scope: &LegacyScope,
    recorded: &[RowIdentity],
    profiles: &[HistoricalProfile],
) -> Result<()> {
    let id = journal
        .file_name()
        .and_then(|name| name.to_str())
        .context("journal identifier is not UTF-8")?;
    let mut visited = std::collections::HashSet::new();
    for profile in profiles {
        let path = &profile.authority.sessions_path;
        if !scope
            .profiles
            .iter()
            .any(|known| known.sessions_path == *path)
            || !visited.insert(path)
        {
            continue;
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut document: Value = serde_json::from_slice(&bytes)?;
        let rows = document
            .as_array_mut()
            .context("profile sessions are not an array")?;
        let mut changed = false;
        for row in rows {
            let current = RowIdentity::deserialize(&*row)?;
            let Some(reservation) = &current.lifecycle_reservation else {
                continue;
            };
            if reservation.resource_intent.is_some() {
                continue;
            }
            let founded = recorded.iter().any(|original| {
                original.id == current.id
                    && original.created_at == current.created_at
                    && original.lifecycle_generation == current.lifecycle_generation
                    && original
                        .lifecycle_reservation
                        .as_ref()
                        .is_some_and(|original| {
                            original.op == reservation.op
                                && original.generation == reservation.generation
                                && reservation.generation == current.lifecycle_generation
                        })
            });
            if founded {
                row["lifecycle_reservation"]["resource_intent"] = Value::String(id.to_owned());
                changed = true;
            }
        }
        if changed {
            crate::session::backup_before_migration(path)?;
            crate::session::storage::atomic_write_verified(
                path,
                &serde_json::to_vec_pretty(&document)?,
            )?;
            crate::session::storage::sync_parent_directory(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, LifecycleOperation, Storage, WorktreeInfo};
    use fs2::FileExt;

    fn pending_row(storage: &Storage, project: &Path, branch: &str) -> Result<Instance> {
        let mut row = Instance::new("original", project.to_str().unwrap());
        row.worktree_info = Some(WorktreeInfo {
            branch: branch.to_owned(),
            main_repo_path: project.to_string_lossy().into_owned(),
            managed_by_aoe: false,
            created_at: row.created_at,
            base_branch: None,
        });
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Rename,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now() - chrono::Duration::days(1),
        )?;
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        Ok(row)
    }

    fn write_v2(storage: &Storage, before: &Instance) -> Result<(PathBuf, Value)> {
        let directory = crate::session::get_app_dir()?.join(".move-journal");
        fs::create_dir_all(&directory)?;
        let path = directory.join("move-123-historical.json");
        let lease = open_lease(&path)?;
        let profile = fs::metadata(storage.sessions_path().parent().unwrap())?;
        let profile = serde_json::json!({
            "profile": storage.profile(),
            "sessions_path": storage.sessions_path(),
            "identity": [profile.dev(), profile.ino()],
        });
        let mut after = before.clone();
        after.title = "renamed".into();
        after.lifecycle_reservation = None;
        let metadata = lease.metadata()?;
        let value = serde_json::json!({
            "version": 2,
            "ids": [before.id],
            "source_profile": storage.profile(),
            "target_profile": storage.profile(),
            "source_sessions_path": storage.sessions_path(),
            "target_sessions_path": storage.sessions_path(),
            "group_move_source_path": "",
            "group_move_target_path": "",
            "group_move_subtree": false,
            "created_at_epoch_ms": 123,
            "live_lock_identity": [metadata.dev(), metadata.ino()],
            "authority": {"kind": "rename", "intent": {
                "source": profile, "target": profile,
                "row": {"before": before, "after": after},
                "checkout": null, "branch": null, "phase": "prepared",
                "observation": null, "branch_receipt": null,
                "checkout_receipt": null, "compensation_receipt": null, "conflict": null,
            }},
        });
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
        Ok((path, value))
    }

    #[test]
    #[serial_test::serial]
    fn physical_evidence_becomes_scoped_conflict_with_nonexpiring_reservation() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("owner")?;
        let (project, repo) = crate::git::test_support::init_repo();
        let branch = repo.head()?.shorthand()?.to_owned();
        let original_tip = repo.head()?.target();
        let row = pending_row(&storage, project.path(), &branch)?;
        let (path, old) = write_v2(&storage, &row)?;
        let original_bytes = fs::read(&path)?;
        let mut sessions: Value = serde_json::from_slice(&fs::read(storage.sessions_path())?)?;
        sessions[0]["unrecognized_user_annotation"] = serde_json::json!({"keep": true});
        fs::write(
            storage.sessions_path(),
            serde_json::to_vec_pretty(&sessions)?,
        )?;

        run()?;

        let entry: MoveJournalEntry = serde_json::from_slice(&fs::read(&path)?)?;
        let MoveAuthority::LegacyConflict { legacy, scope, .. } = entry.authority else {
            panic!("historical evidence must not authorize recovery")
        };
        assert_eq!(legacy, old);
        assert!(scope
            .profiles
            .iter()
            .any(|profile| profile.profile == "owner"));
        let expected = ResourceKey::branch(project.path(), &branch)?;
        assert!(scope.resources.contains(&expected));
        let backups = crate::session::storage::migration_backups(&path)?;
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0].1)?, original_bytes);
        let current = storage.load()?.remove(0);
        assert_eq!(current.id, row.id);
        assert_eq!(current.created_at, row.created_at);
        assert_eq!(current.lifecycle_generation, row.lifecycle_generation);
        assert!(current.has_fresh_lifecycle_reservation(chrono::Utc::now()));
        assert_eq!(
            current
                .lifecycle_reservation
                .as_ref()
                .unwrap()
                .resource_intent
                .as_deref(),
            path.file_name().and_then(|name| name.to_str()),
        );
        let sessions_after: Value = serde_json::from_slice(&fs::read(storage.sessions_path())?)?;
        assert_eq!(
            sessions_after[0]["unrecognized_user_annotation"],
            sessions[0]["unrecognized_user_annotation"]
        );
        assert_eq!(repo.head()?.target(), original_tip);
        assert_eq!(
            repo.find_branch(&branch, git2::BranchType::Local)?
                .get()
                .target(),
            original_tip
        );
        let once = fs::read(&path)?;
        let rows_once = fs::read(storage.sessions_path())?;
        run()?;
        assert_eq!(fs::read(&path)?, once);
        assert_eq!(fs::read(storage.sessions_path())?, rows_once);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn live_historical_producer_is_not_converted_or_consumed() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("owner")?;
        let (project, repo) = crate::git::test_support::init_repo();
        let branch = repo.head()?.shorthand()?.to_owned();
        let row = pending_row(&storage, project.path(), &branch)?;
        let (path, _) = write_v2(&storage, &row)?;
        let lease = open_lease(&path)?;
        lease.lock_exclusive()?;
        let bytes = fs::read(&path)?;
        let rows = fs::read(storage.sessions_path())?;

        assert!(run().is_err());
        assert_eq!(fs::read(&path)?, bytes);
        assert_eq!(fs::read(storage.sessions_path())?, rows);
        assert!(crate::session::storage::migration_backups(&path)?.is_empty());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn displaced_incarnation_is_preserved_without_associating_old_lease() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("owner")?;
        let (project, repo) = crate::git::test_support::init_repo();
        let branch = repo.head()?.shorthand()?.to_owned();
        let row = pending_row(&storage, project.path(), &branch)?;
        let (path, old) = write_v2(&storage, &row)?;
        let mut replacement = row.clone();
        replacement.created_at += chrono::Duration::seconds(1);
        replacement.title = "replacement owner".into();
        fs::write(
            storage.sessions_path(),
            serde_json::to_vec_pretty(&vec![&replacement])?,
        )?;
        let rows = fs::read(storage.sessions_path())?;

        run()?;

        assert_eq!(fs::read(storage.sessions_path())?, rows);
        let entry: MoveJournalEntry = serde_json::from_slice(&fs::read(path)?)?;
        assert!(
            matches!(entry.authority, MoveAuthority::LegacyConflict { legacy, .. } if legacy == old)
        );
        Ok(())
    }
}
