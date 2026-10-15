//! Rename authority survives unlocked container preflight through the lifecycle generation.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use super::builder::CheckoutWitness;
use super::claim::ReservationCleanup;
use super::move_journal::{
    self, EffectObservation, EffectPhase, JournalTransaction, MoveAuthority, MoveJournalEntry,
    PathAuthority, ProfileAuthority, RenameIntent, RowDelta,
};
use super::native_hooks::BranchOrigin;
use super::storage::{self, OwnershipGuard, StorageFlock};
use super::{Instance, LifecycleOperation, Storage};

#[cfg(test)]
type AfterEffectsHook = Box<dyn FnOnce(&OwnershipGuard, &Instance, &Instance)>;
#[cfg(test)]
thread_local! {
    static AFTER_EFFECTS: std::cell::RefCell<Option<AfterEffectsHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_after_effects<T>(
    hook: impl FnOnce(&OwnershipGuard, &Instance, &Instance) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            AFTER_EFFECTS.with(|slot| slot.borrow_mut().take());
        }
    }
    AFTER_EFFECTS.with(|slot| assert!(slot.borrow_mut().replace(Box::new(hook)).is_none()));
    let _reset = Reset;
    run()
}

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

fn record_intent(
    source: &Storage,
    target: &Storage,
    before: &Instance,
    after: &Instance,
    checkout: Option<&CheckoutWitness>,
    branch: Option<&BranchOrigin>,
) -> Result<(JournalTransaction, MoveJournalEntry)> {
    let mut requested = after.clone();
    requested.release_lifecycle_reservation_if_owned(
        LifecycleOperation::Rename,
        before.lifecycle_generation,
    );
    requested.source_profile = target.profile().to_owned();
    let entry = MoveJournalEntry {
        version: move_journal::MOVE_JOURNAL_VERSION,
        ids: vec![before.id.clone()],
        source_profile: source.profile().to_owned(),
        target_profile: target.profile().to_owned(),
        source_sessions_path: source.sessions_path().to_path_buf(),
        target_sessions_path: target.sessions_path().to_path_buf(),
        group_move_source_path: before.group_path.clone(),
        group_move_target_path: after.group_path.clone(),
        group_move_subtree: false,
        created_at_epoch_ms: Utc::now().timestamp_millis().try_into()?,
        authority: MoveAuthority::Rename {
            intent: Box::new(RenameIntent {
                source: ProfileAuthority::capture(source)?,
                target: ProfileAuthority::capture(target)?,
                row: RowDelta {
                    before: before.clone(),
                    after: requested,
                },
                checkout: checkout
                    .map(CheckoutWitness::durable_authority)
                    .transpose()?,
                branch: branch.map(BranchOrigin::durable_authority).transpose()?,
                phase: EffectPhase::Prepared,
                observation: None,
                branch_receipt: None,
                checkout_receipt: None,
                compensation_receipt: None,
                conflict: None,
            }),
        },
        live_lock_identity: None,
    };
    Ok((move_journal::record_live(&entry)?, entry))
}

fn phase(
    entry: &mut MoveJournalEntry,
    journal: &JournalTransaction,
    phase: EffectPhase,
) -> Result<()> {
    let MoveAuthority::Rename { intent } = &mut entry.authority else {
        anyhow::bail!("rename transaction has no rename authority");
    };
    intent.phase = phase;
    journal.rewrite(entry)
}

fn ref_tip(repo: &git2::Repository, branch: &str) -> Result<Option<String>> {
    match repo.find_reference(&format!("refs/heads/{branch}")) {
        Ok(reference) => Ok(Some(
            reference
                .target()
                .context("rename reference is symbolic")?
                .to_string(),
        )),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn observe_effect(entry: &mut MoveJournalEntry, error: Option<String>) -> Result<()> {
    let MoveAuthority::Rename { intent } = &mut entry.authority else {
        anyhow::bail!("rename transaction has no rename authority");
    };
    let mut old_ref = None;
    let mut new_ref = None;
    if let Some(branch) = &intent.branch {
        branch.verify_directories()?;
        let repo = git2::Repository::open(&branch.repository.path)?;
        old_ref = ref_tip(&repo, &branch.branch)?;
        let requested = &intent
            .row
            .after
            .worktree_info
            .as_ref()
            .context("rename branch metadata missing")?
            .branch;
        new_ref = ref_tip(&repo, requested)?;
        if old_ref.is_none() && new_ref.is_none() {
            intent.phase = EffectPhase::BranchRemoved;
        }
    }
    let checkout_path = if let Some(checkout) = &intent.checkout {
        [
            &intent.row.before.project_path,
            &intent.row.after.project_path,
        ]
        .into_iter()
        .find(|path| checkout.verify_directories_at(Path::new(path)).is_ok())
        .map(std::path::PathBuf::from)
    } else {
        None
    };
    let checkout_head = checkout_path
        .as_ref()
        .map(|path| -> Result<_> {
            let repo = git2::Repository::open(path)?;
            let head = repo.find_reference("HEAD")?;
            Ok(head
                .symbolic_target()?
                .map(str::to_owned)
                .or_else(|| head.target().map(|oid| oid.to_string())))
        })
        .transpose()?
        .flatten();
    intent.observation = Some(EffectObservation {
        old_ref,
        new_ref,
        checkout_path,
        checkout_head,
        error,
    });
    Ok(())
}

fn verify_effect_state(intent: &RenameIntent, published: bool) -> Result<()> {
    let row = if published {
        &intent.row.after
    } else {
        &intent.row.before
    };
    if let Some(checkout) = &intent.checkout {
        checkout.verify_directories_at(Path::new(&row.project_path))?;
        if published && intent.row.before.project_path != intent.row.after.project_path {
            let receipt = intent
                .checkout_receipt
                .as_ref()
                .context("moved checkout has no durable receipt")?;
            receipt.verify_directories_at(Path::new(&row.project_path))?;
            receipt.head.verify()?;
        }
    }
    if let Some(branch) = &intent.branch {
        branch.verify_directories()?;
        let repo = git2::Repository::open(&branch.repository.path)?;
        let new = &intent
            .row
            .after
            .worktree_info
            .as_ref()
            .context("requested branch metadata missing")?
            .branch;
        let old_tip = ref_tip(&repo, &branch.branch)?;
        let new_tip = ref_tip(&repo, new)?;
        if published {
            anyhow::ensure!(
                old_tip.is_none() && new_tip.as_deref() == Some(&branch.tip),
                "published rename does not match the original object"
            );
            intent
                .branch_receipt
                .as_ref()
                .context("renamed reference has no durable physical receipt")?
                .verify()?;
        } else {
            anyhow::ensure!(
                old_tip.as_deref() == Some(&branch.tip) && new_tip.is_none(),
                "original branch state has not been restored"
            );
            if let Some(receipt) = &intent.compensation_receipt {
                receipt.verify()?;
            } else {
                branch.loose.verify()?;
                branch.packed.verify()?;
            }
        }
        let checkout = git2::Repository::open(&row.project_path)?;
        let expected = format!("refs/heads/{}", row.worktree_info.as_ref().unwrap().branch);
        anyhow::ensure!(
            checkout.find_reference("HEAD")?.symbolic_target()? == Some(expected.as_str()),
            "checkout HEAD no longer names the recorded branch"
        );
    } else if let Some(checkout) = &intent.checkout {
        checkout.head.verify()?;
    }
    Ok(())
}

fn verify_intent_resources_unclaimed(
    root: &OwnershipGuard,
    source: &Storage,
    intent: &RenameIntent,
) -> Result<()> {
    let id = intent.row.before.id.as_str();
    let paths = super::deletion::paths_in_use_except_with_ownership(root, source.profile(), &[id]);
    anyhow::ensure!(
        !paths.covers(Path::new(&intent.row.before.project_path))
            && !paths.covers(Path::new(&intent.row.after.project_path)),
        "a peer owns a rename source or destination, or ownership is uncertain"
    );
    if let Some(branch) = &intent.branch {
        branch.verify_directories()?;
    }
    for (index, info) in [
        &intent.row.before.worktree_info,
        &intent.row.after.worktree_info,
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 && intent.row.before.worktree_info == intent.row.after.worktree_info {
            continue;
        }
        if let Some(info) = info {
            anyhow::ensure!(
                !super::deletion::branch_in_use_with_ownership(
                    root,
                    source.profile(),
                    &[id],
                    Path::new(&info.main_repo_path),
                    &info.branch,
                )?,
                "a peer adopted an original or resulting branch"
            );
        }
    }
    Ok(())
}

fn recover_intent(intent: &mut RenameIntent) -> Result<EffectPhase> {
    let root = storage::acquire_ownership_lock()?;
    let profiles = super::list_profiles_for_worktree_inventory()?;
    let storages = profiles
        .iter()
        .map(|profile| Storage::open_unwatched_with_ownership(profile, &root))
        .collect::<Result<Vec<_>>>()?;
    let mut source = None;
    let mut target = None;
    for store in &storages {
        store.load_strict_for_worktree_ownership()?;
        if intent.source.matches(store)? {
            source = Some(store);
        }
        if intent.target.matches(store)? {
            target = Some(store);
        }
    }
    let id = &intent.row.before.id;
    let identity = storage::acquire_session_identity_lock_with_ownership(&root)?;
    let title = storage::acquire_session_title_lock_with_ownership(&root, id)?;
    let (first, second) = match (source, target) {
        (Some(source), Some(target)) if intent.source.identity != intent.target.identity => {
            if source.sessions_path() < target.sessions_path() {
                (Some(source), Some(target))
            } else {
                (Some(target), Some(source))
            }
        }
        (Some(source), _) => (Some(source), None),
        (_, Some(target)) => (Some(target), None),
        _ => anyhow::bail!("both original physical profiles disappeared"),
    };
    let first_lifecycle = first
        .map(|store| store.acquire_instance_lifecycle_lock_with_ownership(&root, id))
        .transpose()?;
    let second_lifecycle = second
        .map(|store| store.acquire_instance_lifecycle_lock_with_ownership(&root, id))
        .transpose()?;
    let source_row = source
        .map(|store| -> Result<_> {
            Ok(store
                .load_strict_for_worktree_ownership()?
                .into_iter()
                .find(|row| &row.id == id))
        })
        .transpose()?
        .flatten();
    let target_row = if intent.source.identity == intent.target.identity {
        None
    } else {
        target
            .map(|store| -> Result<_> {
                Ok(store
                    .load_strict_for_worktree_ownership()?
                    .into_iter()
                    .find(|row| &row.id == id))
            })
            .transpose()?
            .flatten()
    };
    let published = if intent.source.identity == intent.target.identity {
        source_row
            .as_ref()
            .is_some_and(|row| move_journal::row_matches_delta(row, &intent.row, true))
    } else {
        target_row
            .as_ref()
            .is_some_and(|row| move_journal::row_matches_delta(row, &intent.row, true))
    };
    if published {
        verify_effect_state(intent, true)?;
        if intent.source.identity != intent.target.identity {
            if let Some(source_row) = source_row {
                anyhow::ensure!(
                    move_journal::row_matches_delta(&source_row, &intent.row, false),
                    "source copy was superseded; it must not be removed"
                );
                anyhow::ensure!(
                    move_journal::source_changes_preserved(
                        &intent.row.before,
                        &source_row,
                        target_row
                            .as_ref()
                            .context("published target row disappeared")?,
                    )?,
                    "source has a concurrent edit not present in the published target"
                );
                let source = source.context("original source profile disappeared")?;
                source.update_with_ownership(&root, |rows, groups| {
                    let before = rows
                        .iter()
                        .find(|row| &row.id == id)
                        .context("source copy disappeared")?;
                    anyhow::ensure!(
                        move_journal::row_matches_delta(before, &intent.row, false),
                        "source incarnation changed before removal"
                    );
                    rows.retain(|row| &row.id != id);
                    storage::reconcile_groups_after_repair(
                        rows,
                        groups,
                        &[],
                        &super::GroupMovePlan::single(
                            &intent.row.before.group_path,
                            &intent.row.after.group_path,
                        ),
                    );
                    Ok(())
                })?;
                storage::sync_parent_directory(source.sessions_path())?;
            }
        }
        let winner = target
            .or(source)
            .context("published physical profile disappeared")?;
        storage::sync_parent_directory(winner.sessions_path())?;
        return Ok(EffectPhase::Published);
    }
    anyhow::ensure!(
        target_row.is_none(),
        "target row has a different incarnation or generation"
    );
    let source = source.context("original physical source profile disappeared")?;
    let current = source_row.context("original session disappeared")?;
    anyhow::ensure!(
        move_journal::row_matches_delta(&current, &intent.row, false),
        "original session authority was superseded"
    );
    verify_intent_resources_unclaimed(&root, source, intent)?;
    if let Some(checkout) = &intent.checkout {
        let original = Path::new(&intent.row.before.project_path);
        let moved = Path::new(&intent.row.after.project_path);
        if checkout.verify_directories_at(original).is_err() {
            checkout.verify_directories_at(moved)?;
            anyhow::ensure!(
                matches!(std::fs::symlink_metadata(original), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "original checkout destination is occupied"
            );
            let info = intent
                .row
                .before
                .worktree_info
                .as_ref()
                .context("original worktree metadata missing")?;
            crate::git::GitWorktree::new(info.main_repo_path.clone().into())?
                .move_worktree(moved, original)?;
            checkout.verify_directories_at(original)?;
        }
    }
    if let Some(branch) = &intent.branch {
        let new = &intent
            .row
            .after
            .worktree_info
            .as_ref()
            .context("new branch metadata missing")?
            .branch;
        let repo = git2::Repository::open(&branch.repository.path)?;
        let old_tip = ref_tip(&repo, &branch.branch)?;
        let new_tip = ref_tip(&repo, new)?;
        let mut nested = [&identity, &title, &identity, &identity];
        let mut nested_len = 2;
        for lock in [&first_lifecycle, &second_lifecycle].into_iter().flatten() {
            nested[nested_len] = lock;
            nested_len += 1;
        }
        let handoff = super::native_hooks::BranchHandoff {
            root: &root,
            nested: &nested[..nested_len],
            owner: Some((source.profile(), id)),
            protected_path: Some(Path::new(&intent.row.after.project_path)),
        };
        match (old_tip.as_deref(), new_tip.as_deref()) {
            (None, None) => {
                intent.checkout.as_ref().context("missing original checkout authority")?.head.verify()?;
                if let Err(error) = super::native_hooks::restore_missing_branch(
                    handoff, branch, Path::new(&intent.row.before.project_path),
                    &mut intent.compensation_receipt,
                ) {
                    if intent.compensation_receipt.is_none() {
                        return Err(error);
                    }
                    // The pinned committed effect must pass every terminal check below.
                }
            }
            (None, Some(tip)) if tip == branch.tip => {
                intent.branch_receipt.as_ref().context("resulting reference has no trusted physical receipt")?.verify()?;
                let retained_output = BranchOrigin::capture(&branch.repository.path, new)?;
                super::native_hooks::rename_branch_from_origin(
                    handoff, &retained_output, Path::new(&intent.row.before.project_path), &branch.branch,
                )?;
                intent.compensation_receipt = Some(PathAuthority::capture(&branch.common.path.join(&branch.reference))?);
            }
            (Some(tip), None) if tip == branch.tip => {}
            _ => anyhow::bail!("branch effects are partial, replaced or adopted; no destructive compensation is authorized"),
        }
    }
    verify_effect_state(intent, false)?;
    verify_intent_resources_unclaimed(&root, source, intent)?;
    source.update_with_ownership(&root, |rows, _| {
        let current = rows
            .iter_mut()
            .find(|row| &row.id == id)
            .context("original row disappeared before compensation commit")?;
        anyhow::ensure!(
            move_journal::row_matches_delta(current, &intent.row, false),
            "original row changed during compensation"
        );
        current.release_lifecycle_reservation_if_owned(
            LifecycleOperation::Rename,
            intent.row.before.lifecycle_generation,
        );
        Ok(())
    })?;
    storage::sync_parent_directory(source.sessions_path())?;
    Ok(EffectPhase::Compensated)
}

pub(crate) fn recover_journal_entry(
    entry: &mut MoveJournalEntry,
    path: &Path,
    lease: &std::fs::File,
) -> Result<bool> {
    let MoveAuthority::Rename { intent } = &mut entry.authority else {
        anyhow::bail!("rename recovery received a different journal authority");
    };
    anyhow::ensure!(
        entry.ids.len() == 1
            && entry.ids[0] == intent.row.before.id
            && intent.row.before.id == intent.row.after.id
            && intent.row.before.created_at == intent.row.after.created_at
            && intent.source.profile == entry.source_profile
            && intent.target.profile == entry.target_profile
            && intent.source.sessions_path == entry.source_sessions_path
            && intent.target.sessions_path == entry.target_sessions_path,
        "rename journal declarations do not match their original authority"
    );
    intent.phase = EffectPhase::Compensating;
    move_journal::rewrite_with_lease(path, lease, entry)?;
    let MoveAuthority::Rename { intent } = &mut entry.authority else {
        unreachable!()
    };
    let result = recover_intent(intent);
    match result {
        Ok(terminal) => {
            intent.phase = terminal;
            intent.conflict = None;
            move_journal::rewrite_with_lease(path, lease, entry)?;
            move_journal::consume(path)?;
            Ok(true)
        }
        Err(error) => {
            intent.phase = EffectPhase::Conflict;
            intent.conflict = Some(format!("{error:#}"));
            tracing::error!(target: "session.rename", path = %path.display(), reason = %error, "rename intent remains an explicit conflict; concurrent data was preserved");
            move_journal::rewrite_with_lease(path, lease, entry)?;
            Ok(false)
        }
    }
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
            let title_requested = title.is_some();
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
            } else if tied && (title_requested || rename) {
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
    let candidate = {
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
    let (journal, mut entry) = record_intent(
        &storage,
        target.as_ref().unwrap_or(&storage),
        &current,
        &candidate,
        witness.as_ref(),
        branch_origin.as_ref(),
    )?;
    let execution = (|| -> Result<Instance> {
        let mut effect = |_: &Instance| -> Result<()> {
            if let Some(name) = &name {
                let info = original
                    .worktree_info
                    .as_ref()
                    .context("Session does not use a worktree")?;
                if !info.managed_by_aoe {
                    return Err(super::worktree_edit::WorktreeEditError::NotManaged.into());
                }
                if name.trim().is_empty() {
                    return Err(super::worktree_edit::WorktreeEditError::EmptyName.into());
                }
                if moves && Path::new(&candidate.project_path).try_exists()? {
                    return Err(super::worktree_edit::WorktreeEditError::TargetExists(
                        candidate.project_path.clone().into(),
                    )
                    .into());
                }
            }
            if changes_branch {
                phase(&mut entry, &journal, EffectPhase::BranchPending)?;
                let origin = branch_origin
                    .as_ref()
                    .context("original branch authority missing")?;
                let branch = &candidate
                    .worktree_info
                    .as_ref()
                    .context("new branch metadata missing")?
                    .branch;
                super::native_hooks::rename_branch_from_origin(
                    super::native_hooks::BranchHandoff {
                        root: &guards.root,
                        nested: &[&guards.identity, &guards.title, &guards.lifecycle],
                        owner: Some((&original.source_profile, &original.id)),
                        protected_path: moves.then_some(Path::new(&candidate.project_path)),
                    },
                    origin,
                    Path::new(&original.project_path),
                    branch,
                )?;
                observe_effect(&mut entry, None)?;
                let MoveAuthority::Rename { intent } = &mut entry.authority else {
                    unreachable!()
                };
                let common = &intent.branch.as_ref().unwrap().common.path;
                intent.branch_receipt = Some(PathAuthority::capture(
                    &common.join(format!("refs/heads/{branch}")),
                )?);
                phase(&mut entry, &journal, EffectPhase::BranchApplied)?;
            }
            if moves {
                phase(&mut entry, &journal, EffectPhase::MovePending)?;
                let mut info = original
                    .worktree_info
                    .as_ref()
                    .context("Session does not use a worktree")?
                    .clone();
                if changes_branch {
                    info.branch
                        .clone_from(&candidate.worktree_info.as_ref().unwrap().branch);
                }
                super::worktree_edit::edit_worktree_workdir_with_ownership(
                    &guards.root,
                    &[&guards.identity, &guards.title, &guards.lifecycle],
                    &original.source_profile,
                    &original.id,
                    super::worktree_edit::WorktreeEditRequest {
                        worktree_info: &info,
                        current_path: Path::new(&original.project_path),
                        new_name: name.as_ref().context("workdir rename name missing")?,
                        rename_branch: false,
                    },
                )?;
                let MoveAuthority::Rename { intent } = &mut entry.authority else {
                    unreachable!()
                };
                intent
                    .checkout
                    .as_ref()
                    .context("original checkout authority missing")?
                    .verify_directories_at(Path::new(&candidate.project_path))?;
                intent.checkout_receipt = Some(
                    CheckoutWitness::capture(Path::new(&candidate.project_path))?
                        .durable_authority()?,
                );
                observe_effect(&mut entry, None)?;
                phase(&mut entry, &journal, EffectPhase::MoveApplied)?;
            }
            #[cfg(test)]
            if let Some(hook) = AFTER_EFFECTS.with(|slot| slot.borrow_mut().take()) {
                hook(&guards.root, &original, &candidate);
            }
            if edits_git {
                storage.verify_profile_identity()?;
                if let Some(target) = &target {
                    target.verify_profile_identity()?;
                }
                let MoveAuthority::Rename { intent } = &entry.authority else {
                    unreachable!()
                };
                let row = storage
                    .load_strict_for_worktree_ownership()?
                    .into_iter()
                    .find(|row| row.id == original.id)
                    .context("original session disappeared before rename publication")?;
                anyhow::ensure!(
                    move_journal::row_matches_delta(&row, &intent.row, false),
                    "original rename authority changed before publication"
                );
                verify_intent_resources_unclaimed(&guards.root, &storage, intent)?;
            }
            Ok(())
        };
        if let Some(target) = &target {
            let committed = storage.move_instance_to_with_effect_with_ownership(
                &guards.root,
                target,
                (&current, &candidate),
                false,
                |rows, candidate| duplicate(rows, candidate, None),
                (edits_git, effect, Some(&journal)),
            )?;
            anyhow::ensure!(
                committed.lifecycle_reservation.is_none(),
                "Moved rename reservation was not released"
            );
            Ok(committed)
        } else {
            duplicate(
                &storage.load_strict_for_worktree_ownership()?,
                &candidate,
                Some(&original.id),
            )?;
            effect(&candidate)?;
            let committed = storage.update_with_ownership(&guards.root, |rows, groups| {
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
            })?;
            storage::sync_parent_directory(storage.sessions_path())?;
            phase(&mut entry, &journal, EffectPhase::Published)?;
            move_journal::consume(&journal.path)?;
            Ok(committed)
        }
    })();
    let candidate = match execution {
        Ok(committed) => committed,
        Err(error) => {
            let observation = observe_effect(&mut entry, Some(format!("{error:#}")))
                .and_then(|()| journal.rewrite(&entry));
            drop(guards);
            let recovery = journal.read().and_then(|mut recorded| {
                recover_journal_entry(&mut recorded, &journal.path, journal.lease())
            });
            let disposition = match recovery {
                Ok(true) => "Git effects and durable session state were reconciled".to_owned(),
                Ok(false) => format!(
                    "an explicit intent conflict remains at {}",
                    journal.path.display()
                ),
                Err(recovery_error) => format!(
                    "recovery failed ({recovery_error:#}); intent remains at {}",
                    journal.path.display()
                ),
            };
            let observation = observation
                .err()
                .map(|failure| format!("; effect observation could not be recorded: {failure:#}"))
                .unwrap_or_default();
            return Err(error.context(format!("{disposition}{observation}")));
        }
    };
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
