//! Rename authority survives unlocked container preflight through the lifecycle generation.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use super::builder::CheckoutWitness;
use super::claim::ReservationCleanup;
use super::native_hooks::BranchOrigin;
use super::storage::{self, OwnershipGuard, StorageFlock};
use super::{Instance, LifecycleOperation, Storage};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RenameRejected {
    #[error("Cannot edit the workdir name or branch while the session is active; stop it first")]
    Active,
    #[error("Session not found")]
    Missing,
    #[error("Session does not use a worktree")]
    NotWorktree,
    #[error("Renaming is unified while session.tie_workdir_to_name is on; use 'aoe session rename --title <name>' instead")]
    Tied,
    #[error("Could not stop the structured view worker before renaming; retry in a moment")]
    WorkerShutdown,
    #[error(transparent)]
    Duplicate(anyhow::Error),
}

pub(crate) enum Edit {
    Session {
        title: Option<String>,
        group: Option<String>,
        profile: Option<String>,
        rename_branch: bool,
        branch: Option<String>,
    },
    Workdir {
        name: String,
        rename_branch: bool,
    },
}

pub(crate) struct Request {
    pub baseline: Instance,
    pub edit: Edit,
    pub file_watch: Arc<crate::file_watch::FileWatchService>,
}

pub(crate) struct Committed {
    pub baseline: Instance,
    pub original: Instance,
    pub instance: Instance,
    pub warning: Option<String>,
    pub storage: Storage,
}

struct Guards {
    root: OwnershipGuard,
    identity: StorageFlock,
    title: StorageFlock,
    lifecycle: StorageFlock,
}

fn lock(storage: &Storage, id: &str) -> Result<Guards> {
    let root = storage::acquire_ownership_lock()?;
    let identity = storage::acquire_session_identity_lock_with_ownership(&root)?;
    let title = storage::acquire_session_title_lock_with_ownership(&root, id)?;
    let lifecycle = storage.acquire_instance_lifecycle_lock_with_ownership(&root, id)?;
    Ok(Guards {
        root,
        identity,
        title,
        lifecycle,
    })
}

fn same_authority(current: &Instance, original: &Instance) -> bool {
    current.id == original.id
        && current.created_at == original.created_at
        && current.title == original.title
        && current.project_path == original.project_path
        && current.worktree_info == original.worktree_info
        && match (&current.workspace_info, &original.workspace_info) {
            (None, None) => true,
            (Some(current), Some(original)) => {
                current.workspace_dir == original.workspace_dir
                    && current.branch == original.branch
                    && current.repos == original.repos
                    && current.created_at == original.created_at
                    && current.cleanup_on_delete == original.cleanup_on_delete
            }
            _ => false,
        }
        && current.is_archived() == original.is_archived()
        && current.tool == original.tool
        && current.sandbox_info.as_ref().map(|sandbox| {
            (
                sandbox.enabled,
                &sandbox.container_id,
                &sandbox.image,
                &sandbox.container_name,
                &sandbox.extra_env,
                &sandbox.custom_instruction,
                &sandbox.container_workdir,
                &sandbox.provider,
            )
        }) == original.sandbox_info.as_ref().map(|sandbox| {
            (
                sandbox.enabled,
                &sandbox.container_id,
                &sandbox.image,
                &sandbox.container_name,
                &sandbox.extra_env,
                &sandbox.custom_instruction,
                &sandbox.container_workdir,
                &sandbox.provider,
            )
        })
        && current.is_structured() == original.is_structured()
        && current.is_trashed() == original.is_trashed()
        && current.lifecycle_reservation_is_owned(
            LifecycleOperation::Rename,
            original.lifecycle_generation,
        )
}

fn duplicate(rows: &[Instance], candidate: &Instance, exclude: Option<&str>) -> Result<()> {
    if super::is_duplicate_session(
        rows.iter(),
        &candidate.title,
        &candidate.project_path,
        exclude,
    ) {
        return Err(
            RenameRejected::Duplicate(super::duplicate_session_error(&candidate.title)).into(),
        );
    }
    Ok(())
}

pub(crate) fn perform(
    mut request: Request,
    preflight: impl FnOnce(&Instance, bool) -> Result<()>,
) -> Result<Committed> {
    let storage = Storage::open(
        &request.baseline.source_profile,
        Arc::clone(&request.file_watch),
    )?;
    let guards = lock(&storage, &request.baseline.id)?;
    let rows = storage.load_strict_for_worktree_ownership()?;
    let mut original = rows
        .iter()
        .find(|row| row.id == request.baseline.id)
        .cloned()
        .ok_or(RenameRejected::Missing)?;
    anyhow::ensure!(
        original.created_at == request.baseline.created_at,
        "Session incarnation changed before rename"
    );
    if original.has_fresh_lifecycle_reservation(Utc::now()) {
        return Err(super::LifecycleReservationError::Busy(
            original.lifecycle_reservation.as_ref().unwrap().op,
        )
        .into());
    }
    anyhow::ensure!(!original.is_trashed(), "Cannot rename a trashed session");
    request
        .baseline
        .source_profile
        .clone_from(&original.source_profile);
    if let Err(error) = super::worktree_reconcile::reconcile_and_persist_with_ownership(
        &storage,
        &guards.root,
        &mut original,
        &mut Default::default(),
    ) {
        tracing::warn!(target: "session.rename", session = %original.id, "worktree path reconciliation skipped: {error}");
    }
    let mut candidate = original.clone();
    let mut name = None;
    let mut branch_only = None;
    let mut rename_branch = false;
    let mut target_profile = None;
    let standalone = matches!(&request.edit, Edit::Workdir { .. });
    match request.edit {
        Edit::Session {
            title,
            group,
            profile,
            rename_branch: rename,
            branch,
        } => {
            if let Some(title) = title {
                candidate.title = title;
            }
            if let Some(group) = group {
                candidate.group_path = group;
            }
            target_profile = profile.filter(|profile| *profile != original.source_profile);
            let tied = original.tie_workdir_applies(
                super::config::profile_config::resolve_config_or_warn(&original.source_profile)
                    .session
                    .tie_workdir_to_name,
            );
            if let Some(branch) = branch {
                branch_only = Some(branch);
            } else if tied && (original.title != candidate.title || rename) {
                name = Some(super::worktree_edit::worktree_leaf_from_title(
                    &candidate.title,
                ));
                rename_branch = rename;
            } else {
                anyhow::ensure!(!rename || tied, "--rename-branch only applies to a tied aoe-managed worktree session (session.tie_workdir_to_name)");
            }
        }
        Edit::Workdir {
            name: requested,
            rename_branch: rename,
        } => {
            if original.worktree_info.is_none() {
                return Err(RenameRejected::NotWorktree.into());
            }
            if original.tie_workdir_applies(
                super::config::profile_config::resolve_config_or_warn(&original.source_profile)
                    .session
                    .tie_workdir_to_name,
            ) {
                return Err(RenameRejected::Tied.into());
            }
            name = Some(requested);
            rename_branch = rename;
        }
    }
    let target = target_profile
        .as_ref()
        .map(|profile| {
            Storage::open_with_ownership(profile, Arc::clone(&request.file_watch), &guards.root)
        })
        .transpose()?;
    if let Some(name) = &name {
        candidate.project_path =
            super::worktree_edit::target_worktree_path(Path::new(&original.project_path), name)
                .context("Worktree has no parent directory")?
                .to_string_lossy()
                .into_owned();
        if rename_branch {
            if let Some(info) = candidate.worktree_info.as_mut() {
                info.branch = super::builder::git_sanitize_branch_name(name);
            }
        }
    }
    if let Some(branch) = &branch_only {
        candidate
            .worktree_info
            .as_mut()
            .context("Branch-only rename requires a managed worktree")?
            .branch
            .clone_from(branch);
    }
    let moves = original.project_path != candidate.project_path;
    let changes_branch = original.worktree_info.as_ref().map(|info| &info.branch)
        != candidate.worktree_info.as_ref().map(|info| &info.branch);
    let edits_git = moves || changes_branch;
    let requires_quiescence = edits_git && branch_only.is_none();
    if standalone && !edits_git {
        return Err(super::worktree_edit::WorktreeEditError::Unchanged.into());
    }
    if let Some(target) = &target {
        let target_rows = target.load_strict_for_worktree_ownership()?;
        anyhow::ensure!(
            !target_rows.iter().any(|row| row.id == original.id),
            "Session already exists in target profile"
        );
        duplicate(&target_rows, &candidate, None)?;
    } else {
        duplicate(&rows, &candidate, Some(&original.id))?;
    }
    drop(rows);
    if edits_git {
        if requires_quiescence
            && (original.status.blocks_worktree_edit()
                || request.baseline.status.blocks_worktree_edit())
        {
            return Err(RenameRejected::Active.into());
        }
        let info = original
            .worktree_info
            .as_ref()
            .context("Session does not use a worktree")?;
        super::worktree_edit::ensure_worktree_unshared(
            &guards.root,
            &original.source_profile,
            &original.id,
            Path::new(&original.project_path),
            info,
            moves.then_some(Path::new(&candidate.project_path)),
            changes_branch,
        )?;
    }
    if let Some(branch) = &branch_only {
        let _ = super::worktree_edit::prepare_worktree_branch_rename(
            original
                .worktree_info
                .as_ref()
                .context("Branch-only rename requires a managed worktree")?,
            Path::new(&original.project_path),
            branch,
        )?;
    }
    let witness = edits_git
        .then(|| CheckoutWitness::capture(Path::new(&original.project_path)))
        .transpose()?;
    let branch_origin = if changes_branch {
        let info = original.worktree_info.as_ref().unwrap();
        Some(BranchOrigin::capture(
            Path::new(&info.main_repo_path),
            &info.branch,
        )?)
    } else {
        None
    };
    let generation = storage.update_with_ownership(&guards.root, |rows, _| {
        let current = rows
            .iter_mut()
            .find(|row| row.id == original.id && row.created_at == original.created_at)
            .context("Session disappeared before rename reservation")?;
        current
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Rename,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .map_err(anyhow::Error::from)
    })?;
    original.lifecycle_generation = generation;
    original.lifecycle_reservation = Some(super::LifecycleReservation {
        op: LifecycleOperation::Rename,
        generation,
        at: Utc::now(),
    });
    drop(guards);
    let mut cleanup =
        ReservationCleanup::new(&storage, &original, LifecycleOperation::Rename, generation);
    preflight(&original, moves)?;
    // Removing a stopped container here also removes its baked-in old mount.
    anyhow::ensure!(
        !moves
            || !super::worktree_edit::ensure_sandbox_container_released(
                &original.id,
                original.is_sandboxed()
            ),
        "Stop the session before renaming: its sandbox container still holds the worktree"
    );
    let guards = lock(&storage, &original.id)?;
    storage.verify_profile_identity()?;
    if let Some(target) = &target {
        target.verify_profile_identity()?;
    }
    let current = storage
        .load_strict_for_worktree_ownership()?
        .into_iter()
        .find(|row| row.id == original.id)
        .context("Session disappeared during rename")?;
    anyhow::ensure!(
        same_authority(&current, &original),
        "Session changed while preparing rename; retry against the current session"
    );
    if requires_quiescence && current.status.blocks_worktree_edit() {
        return Err(RenameRejected::Active.into());
    }
    if let Some(witness) = &witness {
        witness.verify()?;
    }
    if let Some(origin) = &branch_origin {
        origin.verify(None)?;
    }
    let mut candidate = {
        let mut fresh = current.clone();
        if candidate.title != original.title {
            fresh.title = candidate.title;
        }
        if candidate.group_path != original.group_path {
            fresh.group_path = candidate.group_path;
        }
        if candidate.project_path != original.project_path {
            fresh.project_path = candidate.project_path;
        }
        if candidate.worktree_info != original.worktree_info {
            fresh.worktree_info = candidate.worktree_info;
        }
        fresh.source_profile.clone_from(&original.source_profile);
        fresh
    };
    let effect = |_: &Instance| -> Result<()> {
        if let Some(name) = &name {
            let info = original
                .worktree_info
                .as_ref()
                .context("Session does not use a worktree")?;
            match super::worktree_edit::edit_worktree_workdir_with_ownership(
                &guards.root,
                &[&guards.identity, &guards.title, &guards.lifecycle],
                &original.source_profile,
                &original.id,
                super::worktree_edit::WorktreeEditRequest {
                    worktree_info: info,
                    current_path: Path::new(&original.project_path),
                    new_name: name,
                    rename_branch,
                },
            ) {
                Ok(_) | Err(super::worktree_edit::WorktreeEditError::Unchanged) => {}
                Err(error) => return Err(error.into()),
            }
        } else if let Some(branch) = &branch_only {
            super::worktree_edit::rename_worktree_branch_with_ownership(
                &guards.root,
                &[&guards.identity, &guards.title, &guards.lifecycle],
                &original.source_profile,
                &original.id,
                original
                    .worktree_info
                    .as_ref()
                    .context("Session does not use a worktree")?,
                Path::new(&original.project_path),
                branch,
            )?;
        }
        Ok(())
    };
    if let Some(target) = &target {
        candidate = storage.move_instance_to_with_effect_with_ownership(
            &guards.root,
            target,
            (&current, &candidate),
            false,
            |rows, candidate| duplicate(rows, candidate, None),
            (edits_git, effect),
        )?;
        anyhow::ensure!(
            candidate.lifecycle_reservation.is_none(),
            "Moved rename reservation was not released"
        );
        candidate
            .source_profile
            .clone_from(target_profile.as_ref().unwrap());
    } else {
        duplicate(
            &storage.load_strict_for_worktree_ownership()?,
            &candidate,
            Some(&original.id),
        )?;
        effect(&candidate)?;
        let persisted = storage.update_with_ownership(&guards.root, |rows, groups| {
            duplicate(rows, &candidate, Some(&original.id))?;
            let row = rows
                .iter_mut()
                .find(|row| row.id == original.id && row.created_at == original.created_at)
                .context("Session disappeared before rename publication")?;
            anyhow::ensure!(
                same_authority(row, &original),
                "Rename reservation or checkout superseded"
            );
            row.merge_user_action_diff(&current, &candidate);
            row.release_lifecycle_reservation_if_owned(LifecycleOperation::Rename, generation);
            let committed = row.clone();
            let mut tree = super::GroupTree::new_with_groups(rows, groups);
            tree.create_group(&committed.group_path);
            *groups = tree.get_all_groups();
            Ok(committed)
        });
        candidate = match persisted {
            Ok(committed) => committed,
            Err(error) => {
                if let Some(branch) = branch_only.as_deref().filter(|_| changes_branch) {
                    if storage
                        .load_strict_for_worktree_ownership()?
                        .iter()
                        .any(|row| same_authority(row, &original))
                    {
                        super::worktree_edit::rollback_worktree_branch_with_ownership(
                            &guards.root,
                            &[&guards.identity, &guards.title, &guards.lifecycle],
                            &original.source_profile,
                            &original.id,
                            original.worktree_info.as_ref().unwrap(),
                            Path::new(&original.project_path),
                            branch,
                        )
                        .with_context(|| {
                            format!(
                                "Session metadata failed ({error}); branch rollback also failed"
                            )
                        })?;
                    }
                }
                return Err(error.context(
                    "Could not persist renamed session; inspect the checkout before retrying",
                ));
            }
        };
        candidate
            .source_profile
            .clone_from(&original.source_profile);
    }
    cleanup.disarm();
    drop(cleanup);
    drop(guards);
    let warning = if original.title != candidate.title && !candidate.is_structured() {
        crate::tmux::rekey_session(&candidate.id, &original.title, &candidate.title).err()
            .map(|error| format!("Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"))
    } else {
        None
    };
    Ok(Committed {
        baseline: request.baseline,
        original,
        instance: candidate,
        warning,
        storage: target.unwrap_or(storage),
    })
}

#[cfg(test)]
mod tests;
