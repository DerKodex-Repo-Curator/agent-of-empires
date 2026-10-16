//! Creating a session: the request, its pending stub, and the cleanup a
//! cancel or a quit has to do.

use super::*;

pub(in crate::tui) struct SessionMutationGuards {
    pub(super) _session_title: crate::session::StorageFlock,
    pub(super) _lifecycle: crate::session::StorageFlock,
}

fn retire_provisional_stub(
    profile: &str,
    id: &str,
    provisional: &crate::session::builder::ProvisionalCreation,
    provisional_groups: &HashSet<String>,
) -> anyhow::Result<()> {
    let ownership = crate::session::storage::acquire_ownership_lock()?;
    let _identity =
        crate::session::storage::acquire_session_identity_lock_with_ownership(&ownership)?;
    let storage = Storage::open_unwatched_with_ownership(profile, &ownership)?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock_with_ownership(&ownership, id)?;
    provisional.verify_profile(&storage)?;
    storage.update_with_ownership(&ownership, |rows, groups| {
        if let Some(index) = rows.iter().position(|row| row.id == id) {
            if !provisional.matches_instance(&rows[index])?
                || rows[index]
                    .lifecycle_reservation
                    .as_ref()
                    .is_some_and(|lease| lease.resource_intent.is_some())
            {
                return Ok(());
            }
            rows.remove(index);
        }
        groups.retain(|group| !provisional_groups.contains(&group.path));
        *groups = GroupTree::new_with_groups(rows, groups).get_all_groups();
        Ok(())
    })
}

impl HomeView {
    fn forget_creation_stub(
        &mut self,
        id: &str,
        provisional: Option<&crate::session::builder::ProvisionalCreation>,
    ) {
        if provisional.is_none() {
            self.remove_instance(id);
            return;
        }
        if let Some(instance) = self.instances.shift_remove(id) {
            if let Some(deletions) = self.pending_deletions.get_mut(&instance.source_profile) {
                deletions.remove(id);
            }
            if let Some(added) = self.pending_added.get_mut(&instance.source_profile) {
                added.remove(id);
            }
        }
    }

    /// Request background session creation, used for sandbox sessions so the UI does not
    /// block. A `Status::Creating` stub appears in the list, so progress shows in the
    /// preview pane while the TUI stays usable.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    ) {
        // Pre-resolve the title with the logic the builder will run, so the stub, the
        // background creation and the final instance agree; otherwise an empty title shows
        // as the path basename in the stub and a civilization name in the instance.
        if data.title.is_empty() {
            let existing_titles: Vec<&str> = self
                .instances()
                .filter(|i| i.source_profile == data.profile)
                .map(|i| i.title.as_str())
                .collect();
            let existing_branches: Vec<&str> = self
                .instances()
                .filter(|i| i.source_profile == data.profile)
                .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
                .collect();
            let taken_branches = crate::session::builder::collect_taken_branches_for_derived_dedupe(
                &existing_branches,
                &data.path,
                &data.extra_repo_paths,
                data.worktree_enabled,
                data.create_new_branch,
                data.scratch,
            );
            if let Ok(title) = crate::session::builder::resolve_title(
                &data.title,
                data.worktree_branch.as_deref(),
                data.worktree_enabled,
                &existing_titles,
                &taken_branches,
            ) {
                data.title = title;
            }
        }
        let stub_title = data.title.clone();
        let mut stub = Instance::new(&stub_title, &data.path);
        stub.tool = if data.tool.is_empty() {
            "claude".to_string()
        } else {
            data.tool.clone()
        };
        stub.group_path = data.group.clone();
        stub.status = crate::session::Status::Creating;
        stub.yolo_mode = data.yolo_mode;
        stub.source_profile = data.profile.clone();
        if let Err(error) = stub.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        ) {
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Creation Failed",
                &error.to_string(),
            ));
            self.new_dialog = None;
            return;
        }

        // Set stub worktree_info so project-mode grouping works during creation; the real
        // one, with a resolved main_repo_path, replaces it once build_instance completes.
        let stub_branch = data
            .worktree_branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                data.worktree_enabled
                    .then(|| crate::session::builder::branch_name_from_title(&stub_title))
            });
        if let Some(branch) = stub_branch {
            stub.worktree_info = Some(crate::session::WorktreeInfo {
                branch,
                main_repo_path: data.path.clone(),
                managed_by_aoe: false,
                created_at: chrono::Utc::now(),
                base_branch: data.base_branch.clone(),
            });
        }

        let provisional_creation = match (|| -> anyhow::Result<_> {
            let storage = Storage::open(&data.profile, self.file_watch.clone())?;
            let provisional =
                crate::session::builder::ProvisionalCreation::capture(&storage, &stub)?;
            let saved = (|| {
                let root = crate::session::storage::acquire_ownership_lock()?;
                let _identity =
                    crate::session::storage::acquire_session_identity_lock_with_ownership(&root)?;
                let _lifecycle =
                    storage.acquire_instance_lifecycle_lock_with_ownership(&root, &stub.id)?;
                storage.update_with_ownership(&root, |rows, _| {
                    anyhow::ensure!(
                        !rows.iter().any(|row| row.id == stub.id),
                        "Creating identity already exists"
                    );
                    rows.push(stub.clone());
                    Ok(())
                })
            })();
            if let Err(error) = saved {
                let cleanup =
                    retire_provisional_stub(&data.profile, &stub.id, &provisional, &HashSet::new());
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => {
                        error.context(format!("Creating placeholder cleanup failed: {cleanup:#}"))
                    }
                });
            }
            Ok(std::sync::Arc::new(provisional))
        })() {
            Ok(provisional) => Some(provisional),
            Err(error) => {
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Creation Failed",
                    &format!("{error:#}"),
                ));
                self.new_dialog = None;
                return;
            }
        };
        let stub_id = stub.id.clone();
        let target_profile = data.profile.clone();
        let existing_group_paths: HashSet<String> = self
            .group_trees
            .get(&target_profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .map(|group| group.path)
                    .collect()
            })
            .unwrap_or_default();

        // Add stub to instance list
        self.publish_persisted_instance(stub);
        self.rebuild_group_trees();
        if !data.group.is_empty() {
            if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                tree.create_group(&data.group);
            }
        }
        self.creating_provisional_group_paths = self
            .group_trees
            .get(&target_profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .map(|group| group.path)
                    .filter(|path| !existing_group_paths.contains(path))
                    .collect()
            })
            .unwrap_or_default();

        // Initialize progress tracking and select the stub
        self.creating_hook_progress.insert(
            stub_id.clone(),
            CreatingHookProgress {
                hook_output: Vec::new(),
                current_hook: None,
            },
        );
        self.creating_stub_id = Some(stub_id.clone());
        self.creating_provisional_creation = provisional_creation.clone();
        self.rebuild_flat_items();

        // Move cursor to the new stub
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &stub_id))
        {
            self.cursor = pos;
            self.update_selected();
        }

        // Close the dialog
        self.new_dialog = None;

        let cancel = tokio_util::sync::CancellationToken::new();
        self.creation_cancel = Some(cancel.clone());
        // Filter out the stub from existing instances so the builder doesn't
        // treat its placeholder title as a duplicate to auto-increment.
        let existing_instances: Vec<Instance> = self
            .instances
            .values()
            .filter(|i| i.id != stub_id)
            .cloned()
            .collect();
        let request = CreationRequest {
            data,
            existing_instances,
            hooks,
            cancel,
            provisional_creation,
        };
        if let Err(error) = self.creation_poller.request_creation(request) {
            self.cancel_creation();
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Creation Failed",
                &format!("{error:#}"),
            ));
        }
    }

    /// Cancel the current creation; the worker stops at its next step and rolls back.
    pub fn cancel_creation(&mut self) {
        if let Some(cancel) = self.creation_cancel.take() {
            cancel.cancel();
        }
        if let Some(stub_id) = self.creating_stub_id.take() {
            let provisional = self.creating_provisional_creation.take();
            let groups = std::mem::take(&mut self.creating_provisional_group_paths);
            let profile = self
                .get_instance(&stub_id)
                .map(|row| row.source_profile.clone());
            self.forget_creation_stub(&stub_id, provisional.as_deref());
            if let (Some(provisional), Some(profile)) = (provisional, profile) {
                if let Err(error) =
                    retire_provisional_stub(&profile, &stub_id, &provisional, &groups)
                {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Cancellation cleanup failed",
                        &format!("{error:#}"),
                    ));
                }
                if let Err(error) = self.reload() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Cancellation reload failed",
                        &format!("{error:#}"),
                    ));
                }
            }
            self.creating_hook_progress.remove(&stub_id);
            self.rebuild_group_trees();
            self.rebuild_flat_items();
            self.update_selected();
        }
        self.new_dialog = None;
    }

    /// Apply any pending creation results from the background poller.
    /// Returns Some(session_id) if creation succeeded and we should attach.
    pub fn apply_creation_results(&mut self) -> Option<String> {
        use crate::tui::creation_poller::CreationResult;

        let outcome = match self.creation_poller.try_recv_result() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if self.creating_stub_id.is_some() {
                    self.cancel_creation();
                    let recovery = Storage::recover_pending_moves(self.file_watch.clone());
                    let message = match recovery {
                        Ok(notices) => {
                            format!("Creation worker disconnected.\n{}", notices.join("\n"))
                        }
                        Err(error) => {
                            format!("Creation worker disconnected; recovery failed: {error:#}")
                        }
                    };
                    if let Err(error) = self.reload() {
                        tracing::warn!(target: "tui.home", "Creation disconnect reload failed: {error:#}");
                    }
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &message));
                }
                return None;
            }
        };
        let result = outcome.result;

        // A cancelled request's stub is already gone; the fields below may belong to a
        // newer request, so leave them alone.
        if outcome.cancelled || matches!(result, CreationResult::Cancelled) {
            match result {
                CreationResult::Success(mut success) => {
                    if let Err(error) = success.build_result.rollback() {
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Cancellation cleanup failed",
                            &format!("{error:#}"),
                        ));
                    }
                }
                CreationResult::Error(error) => {
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                }
                CreationResult::Cancelled => {}
            }
            return None;
        }

        self.creation_cancel = None;
        let stub_id = self.creating_stub_id.take();
        let provisional = self.creating_provisional_creation.take();
        // Taken, not borrowed, so every early return leaves the field empty: the
        // provisional group paths belong to this stub alone and must not carry into the
        // next creation.
        let provisional_group_paths = std::mem::take(&mut self.creating_provisional_group_paths);
        if let Some(ref id) = stub_id {
            self.creating_hook_progress.remove(id);
        }

        match result {
            CreationResult::Success(success) => {
                let crate::tui::creation_poller::CreationSuccess {
                    mut build_result,
                    on_launch_hooks_ran,
                } = *success;
                if let Err(mut error) = build_result.publish() {
                    if let Err(cleanup) = build_result.rollback() {
                        error = error
                            .context(format!("Creation rollback could not finish: {cleanup:#}"));
                    }
                    if let (Some(id), Some(provisional)) =
                        (stub_id.as_deref(), provisional.as_deref())
                    {
                        if let Err(cleanup) = retire_provisional_stub(
                            build_result.profile_storage.profile(),
                            id,
                            provisional,
                            &provisional_group_paths,
                        ) {
                            error =
                                error.context(format!("Placeholder cleanup failed: {cleanup:#}"));
                        }
                    }
                    if let Err(reload) = self.reload() {
                        error = error.context(format!("Authoritative reload failed: {reload:#}"));
                    }
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Creation Failed",
                        &format!("{error:#}"),
                    ));
                    self.new_dialog = None;
                    return None;
                }
                let mut instance = build_result.instance;
                let session_id = instance.id.clone();
                let target_profile = build_result.profile_storage.profile().to_owned();
                self.storages
                    .insert(target_profile, build_result.profile_storage);
                crate::tips::record_session_creations(1);
                self.publish_persisted_instance(instance.clone());
                self.rebuild_group_trees();
                if on_launch_hooks_ran {
                    self.on_launch_hooks_ran.insert(session_id.clone());
                }
                if let Err(error) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {error:#}");
                }
                let minted = instance
                    .sandbox_info
                    .as_mut()
                    .map(|sandbox| std::mem::take(&mut sandbox.before_start_env))
                    .unwrap_or_default();
                if !minted.is_empty() {
                    self.mutate_instance(&session_id, |instance| {
                        if let Some(sandbox) = instance.sandbox_info.as_mut() {
                            sandbox.before_start_env = minted.clone();
                        }
                    });
                }
                self.select_and_reveal_session(&session_id);
                self.new_dialog = None;
                if !build_result.warnings.is_empty() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Session warnings",
                        &format!(
                            "Session was created, but setup emitted warnings:\n\n{}",
                            build_result.warnings.join("\n\n")
                        ),
                    ));
                }
                Some(session_id)
            }
            CreationResult::Error(mut error) => {
                if let Some(id) = &stub_id {
                    let profile = self
                        .get_instance(id)
                        .map(|instance| instance.source_profile.clone());
                    self.forget_creation_stub(id, provisional.as_deref());
                    if let Some(profile) = profile {
                        if let Some(provisional) = provisional.as_deref() {
                            if let Err(retire_error) = retire_provisional_stub(
                                &profile,
                                id,
                                provisional,
                                &provisional_group_paths,
                            ) {
                                error.push_str(&format!(
                                    "\nFailed to retire creation placeholder: {retire_error:#}"
                                ));
                            }
                        }
                        let instances = self.cloned_instances_for_profile(&profile);
                        if let Some(tree) = self.group_trees.get_mut(&profile) {
                            let mut groups = tree.get_all_groups();
                            groups.retain(|group| !provisional_group_paths.contains(&group.path));
                            *tree = GroupTree::new_with_groups(&instances, &groups);
                        }
                    }
                    if provisional.is_some() {
                        if let Err(reload_error) = self.reload() {
                            error.push_str(&format!(
                                "\nFailed to reload authoritative state: {reload_error:#}"
                            ));
                        }
                    }
                    self.rebuild_flat_items();
                    self.update_selected();
                    // Hook failures carry multi-line output; size to fit so
                    // the actual error isn't clipped at the default 50x9.
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                } else if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_loading(false);
                    dialog.set_error(error);
                }
                None
            }
            // Returned early above.
            CreationResult::Cancelled => None,
        }
    }

    /// Check if on_launch hooks already ran for this session (and consume the flag).
    pub fn take_on_launch_hooks_ran(&mut self, session_id: &str) -> bool {
        self.on_launch_hooks_ran.remove(session_id)
    }

    /// Check if there's a pending creation operation
    pub fn is_creation_pending(&self) -> bool {
        self.creation_poller.is_pending()
    }

    /// Check if the currently selected session is the in-flight creating stub
    pub fn is_creating_stub_selected(&self) -> bool {
        match (&self.creating_stub_id, &self.selected_session) {
            (Some(stub_id), Some(selected)) => stub_id == selected,
            _ => false,
        }
    }

    /// Show a confirmation dialog warning that a session is being created.
    pub fn show_quit_during_creation_confirm(&mut self) {
        self.confirm_dialog = Some(ConfirmDialog::new(
            "Session Creating",
            "A session is still being created. Quit anyway? The hook will be cancelled.",
            "quit_during_creation",
        ));
    }

    /// Whether `q` on the home screen should confirm before quitting.
    pub fn confirm_before_quit(&self) -> bool {
        self.confirm_before_quit
    }

    /// Show the "quit aoe?" confirmation, with a "don't warn me again"
    /// checkbox that flips `confirm_before_quit` off when ticked (#1569).
    pub fn show_quit_confirm(&mut self) {
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Quit Agent of Empires",
                "Quit?\nYour sessions persist in the background.",
                "quit",
            )
            .neutral()
            .offering_dont_ask_again(),
        );
    }

    /// Persist `confirm_before_quit = false` and update the cached flag, when the user
    /// ticks "don't warn me again" in the quit dialog.
    pub(in crate::tui) fn disable_confirm_before_quit(&mut self) {
        self.confirm_before_quit = false;
        if let Err(e) = update_config(|config| {
            config.session.confirm_before_quit = false;
        }) {
            tracing::warn!(target: "tui.home", "Failed to save config: {e}");
        }
    }

    /// Persist the "don't warn me again" opt-out for whichever confirm offered the
    /// checkbox. Both call sites route through this, so keyboard and click cannot disagree
    /// about which confirms are opt-out-able; actions without a checkbox never reach it.
    pub(in crate::tui) fn apply_confirm_dont_ask_again(&mut self, action: &str) {
        match action {
            "quit" => self.disable_confirm_before_quit(),
            // Written globally, matching the quit opt-out. A profile that overrides
            // confirm_delete = true keeps prompting; that override is cleared from the
            // settings pane.
            "trash_session" => {
                if let Err(e) = update_config(|config| {
                    config.session.confirm_delete = false;
                }) {
                    tracing::warn!(target: "tui.home", "Failed to save config: {e}");
                }
            }
            _ => {}
        }
    }

    /// Cancel pending creation and drain rollback results until the shutdown deadline.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.creation_poller.is_pending() {
            return;
        }
        self.cancel_creation();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.creation_poller.is_pending() {
            let Some(outcome) = self
                .creation_poller
                .recv_result_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                break;
            };
            if let crate::tui::creation_poller::CreationResult::Success(mut success) =
                outcome.result
            {
                if let Err(error) = success.build_result.rollback() {
                    tracing::error!(target: "tui.home", "Cancelled creation exit cleanup failed: {error:#}");
                }
            }
        }
    }
}
