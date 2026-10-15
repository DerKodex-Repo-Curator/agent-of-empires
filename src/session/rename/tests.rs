use super::*;

fn fixture() -> (tempfile::TempDir, Storage, Instance) {
    let root = tempfile::tempdir().unwrap();
    let repo_path = root.path().join("repo");
    let repo = git2::Repository::init(&repo_path).unwrap();
    std::fs::write(repo_path.join("tracked"), "base").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked")).unwrap();
    let tree = index.write_tree().unwrap();
    let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "base",
        &repo.find_tree(tree).unwrap(),
        &[],
    )
    .unwrap();
    let base = repo.head().unwrap().shorthand().unwrap().to_string();
    let path = root.path().join("old");
    crate::git::GitWorktree::new(repo_path.clone())
        .unwrap()
        .with_init_submodules(false)
        .create_worktree("topic", &path, true, Some(&base))
        .unwrap();
    let storage = Storage::new_unwatched("owner").unwrap();
    let mut row = Instance::new("old", &path.to_string_lossy());
    row.source_profile = "owner".into();
    row.status = crate::session::Status::Stopped;
    row.view = crate::session::View::Structured;
    row.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "topic".into(),
        main_repo_path: repo_path.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: Some(base),
    });
    storage
        .update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    (root, storage, row)
}

fn request(row: &Instance, kind: &str) -> Request {
    Request {
        baseline: row.clone(),
        edit: if kind == "workdir" {
            Edit::Workdir {
                name: "new".into(),
                rename_branch: true,
            }
        } else {
            Edit::Session {
                title: Some("new".into()),
                group: None,
                profile: (kind == "profile").then(|| "target".into()),
                rename_branch: true,
                branch: None,
            }
        },
        file_watch: crate::file_watch::FileWatchService::noop(),
    }
}

#[test]
#[serial_test::serial]
fn preflight_releases_all_flocks_and_preserves_unsubmitted_peer_fields() {
    for kind in ["workdir", "same", "profile"] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(kind != "workdir");
        let (root, storage, row) = fixture();
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let id = row.id.clone();
        let source = storage.clone();
        let competing = request(&row, kind);
        let mut writer = None;
        let committed = perform(request(&row, kind), |reserved, moves| {
            assert!(moves);
            assert!(reserved.lifecycle_reservation_is_owned(
                LifecycleOperation::Rename,
                reserved.lifecycle_generation
            ));
            let (done, completed) = std::sync::mpsc::channel();
            writer = Some(std::thread::spawn(move || {
                let refused = perform(competing, |_, _| {
                    panic!("reserved rename must not enter preflight")
                })
                .err()
                .expect("another rename must reject the persisted reservation");
                assert!(matches!(
                    refused.downcast_ref::<crate::session::LifecycleReservationError>(),
                    Some(crate::session::LifecycleReservationError::Busy(
                        LifecycleOperation::Rename
                    ))
                ));
                let root = storage::acquire_ownership_read().unwrap();
                let _identity =
                    storage::acquire_session_identity_lock_with_ownership(&root).unwrap();
                let _title =
                    storage::acquire_session_title_lock_with_ownership(&root, &id).unwrap();
                let _lifecycle = source
                    .acquire_instance_lifecycle_lock_with_ownership(&root, &id)
                    .unwrap();
                peer.update_with_ownership(&root, |rows, _| {
                    rows.push(Instance::new("peer identity", "/unrelated"));
                    Ok(())
                })
                .unwrap();
                source
                    .update_with_ownership(&root, |rows, _| {
                        rows.iter_mut().find(|row| row.id == id).unwrap().group_path =
                            "peer-group".into();
                        Ok(())
                    })
                    .unwrap();
                let _ = done.send(());
            }));
            completed
                .recv_timeout(std::time::Duration::from_millis(500))
                .context("identity/lifecycle writer stayed blocked during preflight")
        });
        writer.unwrap().join().unwrap();
        let committed = committed.unwrap();
        let durable = if kind == "profile" {
            target.load().unwrap()
        } else {
            storage.load().unwrap()
        };
        let durable = durable.iter().find(|current| current.id == row.id).unwrap();
        assert_eq!(durable.created_at, row.created_at);
        assert_eq!(durable.group_path, "peer-group");
        assert!(durable.lifecycle_reservation.is_none());
        assert_eq!(
            durable.project_path,
            root.path().join("new").to_string_lossy()
        );
        assert_eq!(durable.worktree_info.as_ref().unwrap().branch, "new");
        assert_eq!(committed.instance.project_path, durable.project_path);
        assert!(root.path().join("new/tracked").exists());
        assert!(!root.path().join("old").exists());
        if kind == "profile" {
            assert!(storage.load().unwrap().is_empty());
        }
    }
}

#[test]
#[serial_test::serial]
fn superseded_generations_profiles_and_resource_claims_defeat_rename_before_git() {
    for change in [
        "generation",
        "incarnation",
        "source-profile",
        "target-profile",
        "destination",
        "source",
        "branch",
        "same-oid-ref",
    ] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, storage, row) = fixture();
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let result = perform(request(&row, "profile"), |reserved, _| {
            match change {
                "generation" | "incarnation" => storage.update(|rows, _| {
                    let current = rows
                        .iter_mut()
                        .find(|current| current.id == row.id)
                        .unwrap();
                    current.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Rename,
                        reserved.lifecycle_generation,
                    );
                    current.try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )?;
                    if change == "incarnation" {
                        current.created_at += chrono::Duration::seconds(1);
                    }
                    Ok(())
                })?,
                "source-profile" | "target-profile" => {
                    let replaced = if change == "source-profile" {
                        &storage
                    } else {
                        &target
                    };
                    let path = replaced.sessions_path().parent().unwrap();
                    std::fs::rename(path, root.path().join("retired-profile"))?;
                    std::fs::create_dir(path)?;
                    Storage::new_unwatched(replaced.profile())?;
                }
                "destination" | "source" => peer.update(|rows, _| {
                    let path = if change == "source" {
                        root.path().join("old")
                    } else {
                        root.path().join("new")
                    };
                    rows.push(Instance::new("adopter", &path.to_string_lossy()));
                    Ok(())
                })?,
                "branch" => {
                    let repo = git2::Repository::open(root.path().join("repo"))?;
                    repo.branch("new", &repo.head()?.peel_to_commit()?, false)?;
                }
                "same-oid-ref" => {
                    let path = root.path().join("repo/.git/refs/heads/topic");
                    let bytes = std::fs::read(&path)?;
                    std::fs::rename(&path, path.with_extension("retired"))?;
                    std::fs::write(path, bytes)?;
                }
                _ => unreachable!(),
            }
            Ok(())
        });
        assert!(
            result.is_err(),
            "{change} must defeat stale rename authority"
        );
        assert!(root.path().join("old/tracked").exists(), "{change}");
        assert!(!root.path().join("new").exists(), "{change}");
        if change == "generation" {
            let current = storage.load().unwrap().remove(0);
            assert_eq!(
                current.lifecycle_reservation.unwrap().op,
                LifecycleOperation::Launch
            );
            assert!(current.lifecycle_generation > row.lifecycle_generation + 1);
        }
    }
}

#[test]
#[serial_test::serial]
fn metadata_rename_uses_canonical_profile_for_an_unstamped_request() {
    let _app = crate::session::test_support::isolate_app_dir();
    let storage = Storage::new_unwatched("default").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut row = Instance::new("before", &directory.path().to_string_lossy());
    row.view = crate::session::View::Structured;
    row.group_path = "kept".into();
    storage
        .update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    let mut raw: Vec<Instance> =
        serde_json::from_slice(&std::fs::read(storage.sessions_path()).unwrap()).unwrap();
    let baseline = raw.remove(0);
    assert!(baseline.source_profile.is_empty());
    assert!(baseline.worktree_info.is_none());
    let committed = perform(
        Request {
            baseline,
            edit: Edit::Session {
                title: Some("after".into()),
                group: None,
                profile: None,
                rename_branch: false,
                branch: None,
            },
            file_watch: crate::file_watch::FileWatchService::noop(),
        },
        |reserved, moves| {
            assert_eq!(reserved.source_profile, "default");
            assert!(!moves);
            Ok(())
        },
    )
    .unwrap();
    for instance in [
        &committed.baseline,
        &committed.original,
        &committed.instance,
    ] {
        assert_eq!(instance.source_profile, "default");
        assert_eq!(instance.id, row.id);
        assert_eq!(instance.created_at, row.created_at);
    }
    let durable = storage.load().unwrap().remove(0);
    assert_eq!(durable.title, "after");
    assert_eq!(durable.project_path, row.project_path);
    assert_eq!(durable.group_path, "kept");
    assert_eq!(durable.lifecycle_generation, row.lifecycle_generation + 1);
    assert!(durable.lifecycle_reservation.is_none());
    assert_eq!(committed.instance.title, durable.title);
    assert_eq!(
        committed.instance.lifecycle_generation,
        durable.lifecycle_generation
    );
}

#[test]
#[serial_test::serial]
fn unchanged_tied_title_still_refuses_a_claimed_derived_workdir() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, storage, mut row) = fixture();
    row.title = "new".into();
    let claimed = root.path().join("new");
    storage
        .update(|rows, _| {
            rows[0].title.clone_from(&row.title);
            rows.push(Instance::new("new", &claimed.to_string_lossy()));
            Ok(())
        })
        .unwrap();
    let error = perform(
        Request {
            baseline: row.clone(),
            edit: Edit::Session {
                title: Some("new".into()),
                group: None,
                profile: None,
                rename_branch: false,
                branch: None,
            },
            file_watch: crate::file_watch::FileWatchService::noop(),
        },
        |_, _| panic!("duplicate destination must refuse before unlocked preflight"),
    )
    .err()
    .expect("unchanged title must still validate its derived path");
    assert!(matches!(
        error.downcast_ref::<RenameRejected>(),
        Some(RenameRejected::Duplicate(_))
    ));
    let durable = storage.load().unwrap();
    let original = durable.iter().find(|current| current.id == row.id).unwrap();
    assert_eq!(original.project_path, row.project_path);
    assert_eq!(original.lifecycle_generation, row.lifecycle_generation);
    assert!(original.lifecycle_reservation.is_none());
    assert!(root.path().join("old/tracked").exists());
    assert!(!claimed.exists());
}

fn edit_request(row: &Instance, kind: &str, cross_profile: bool) -> Request {
    Request {
        baseline: row.clone(),
        edit: Edit::Session {
            title: (kind != "branch").then(|| "new".into()),
            group: None,
            profile: cross_profile.then(|| "target".into()),
            rename_branch: kind == "both",
            branch: (kind == "branch").then(|| "exact/Requested-Branch".into()),
        },
        file_watch: crate::file_watch::FileWatchService::noop(),
    }
}

#[test]
#[serial_test::serial]
fn real_git_effects_recover_after_same_and_cross_profile_publication_failure() {
    for cross_profile in [false, true] {
        for kind in ["branch", "move", "both"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
            let (root, source, before) = fixture();
            let target = Storage::new_unwatched("target").unwrap();
            target.update(|_, _| Ok(())).unwrap();
            let failed = if cross_profile {
                target.clone()
            } else {
                source.clone()
            };
            let file = failed.sessions_path().to_path_buf();
            let saved = root.path().join("original-sessions.json");
            let restore_file = file.clone();
            let restore_saved = saved.clone();
            let result = with_after_effects(
                move |_, _, candidate| {
                    assert!(Path::new(&candidate.project_path).join("tracked").exists());
                    let repo = git2::Repository::open(&candidate.project_path).unwrap();
                    assert_eq!(
                        repo.head().unwrap().shorthand().unwrap(),
                        candidate.worktree_info.as_ref().unwrap().branch.as_str()
                    );
                    std::fs::rename(&file, &saved).unwrap();
                    std::fs::create_dir(&file).unwrap();
                },
                || perform(edit_request(&before, kind, cross_profile), |_, _| Ok(())),
            );
            assert!(
                result.is_err(),
                "{kind}/{cross_profile}: real publication failure must surface"
            );
            let pending = move_journal::scan();
            assert_eq!(pending.entries.len(), 1);
            assert!(matches!(&pending.entries[0].1.as_ref().unwrap().authority,
                MoveAuthority::Rename { intent } if intent.phase == EffectPhase::Conflict));
            std::fs::remove_dir(&restore_file).unwrap();
            std::fs::rename(&restore_saved, &restore_file).unwrap();
            assert!(
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                    .unwrap()
                    .is_empty()
            );
            assert!(move_journal::scan().entries.is_empty());
            let durable = source.load().unwrap();
            assert_eq!(durable.len(), 1);
            assert_eq!(durable[0].id, before.id);
            assert_eq!(durable[0].created_at, before.created_at);
            assert_eq!(durable[0].project_path, before.project_path);
            assert_eq!(durable[0].worktree_info, before.worktree_info);
            assert!(durable[0].lifecycle_reservation.is_none());
            assert!(target.load().unwrap().is_empty());
            let repo = git2::Repository::open(&before.project_path).unwrap();
            assert_eq!(repo.head().unwrap().shorthand().unwrap(), "topic");
            let common =
                git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path)
                    .unwrap();
            assert!(common.find_reference("refs/heads/topic").is_ok());
            assert!(common.find_reference("refs/heads/new").is_err());
            assert!(common
                .find_reference("refs/heads/exact/Requested-Branch")
                .is_err());
            assert!(!root.path().join("new").exists());
            assert!(
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

fn reference_hook(
    root: &Path,
    row: &Instance,
    phase: &str,
    action: &str,
    profile: &str,
) -> std::path::PathBuf {
    use crate::session::environment::shell_escape;
    use std::os::unix::fs::PermissionsExt;
    let entered = root.join("callback-entered");
    let completed = root.join("callback-completed");
    let program = crate::process::running_image().unwrap();
    let hook = Path::new(&row.worktree_info.as_ref().unwrap().main_repo_path)
        .join(".git/hooks/reference-transaction");
    let gate = if action == "replace-result-ref" {
        let repo =
            git2::Repository::open(&row.worktree_info.as_ref().unwrap().main_repo_path).unwrap();
        format!(
            "[ -f {} ] || exit 0\n",
            shell_escape(&repo.commondir().join("refs/heads/new").to_string_lossy())
        )
    } else {
        String::new()
    };
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = {phase} ] || exit 0\n{gate}[ ! -e {entered} ] || exit 0\n: > {entered}\nAOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE={profile} AOE_TEST_NATIVE_ACTION={action} AOE_TEST_NATIVE_TARGET={target} AOE_TEST_NATIVE_MARKER={completed} {program} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture >/dev/null || exit $?\nprintf complete > {completed}\n",
        phase = shell_escape(phase), entered = shell_escape(&entered.to_string_lossy()),
        profile = shell_escape(profile), action = shell_escape(action),
        target = shell_escape(&row.project_path), completed = shell_escape(&completed.to_string_lossy()),
        program = shell_escape(&program.to_string_lossy()),
    );
    std::fs::write(&hook, script).unwrap();
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    completed
}

#[test]
#[serial_test::serial]
fn committed_old_ref_deletion_repairs_from_original_oid_after_profile_rename() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, source, before) = fixture();
    let old_identity = source.physical_profile_identity().unwrap();
    let repo =
        git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path).unwrap();
    let original_tip = repo
        .find_reference("refs/heads/topic")
        .unwrap()
        .target()
        .unwrap();
    let completed = reference_hook(root.path(), &before, "committed", "rename", "owner");
    let result = perform(edit_request(&before, "both", false), |_, _| Ok(()));
    assert!(result.is_err());
    assert!(
        completed.exists(),
        "the real committed callback must finish its profile rename"
    );
    assert!(Storage::open_unwatched("owner").is_err());
    let moved = Storage::open_unwatched("owner-renamed").unwrap();
    assert_eq!(moved.physical_profile_identity().unwrap(), old_identity);
    let durable = moved.load().unwrap();
    assert_eq!(durable[0].created_at, before.created_at);
    assert_eq!(durable[0].project_path, before.project_path);
    assert_eq!(durable[0].worktree_info, before.worktree_info);
    assert!(durable[0].lifecycle_reservation.is_none());
    assert_eq!(
        repo.find_reference("refs/heads/topic").unwrap().target(),
        Some(original_tip)
    );
    assert!(repo.find_reference("refs/heads/new").is_err());
    assert_eq!(
        git2::Repository::open(&before.project_path)
            .unwrap()
            .head()
            .unwrap()
            .shorthand()
            .unwrap(),
        "topic"
    );
    assert!(move_journal::scan().entries.is_empty());
}

#[test]
#[serial_test::serial]
fn recovery_in_a_real_native_callback_cannot_consume_the_live_intent() {
    for cross_profile in [false, true] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, source, before) = fixture();
        let target = Storage::new_unwatched("target").unwrap();
        let completed = reference_hook(root.path(), &before, "prepared", "recover", "owner");
        let committed =
            perform(edit_request(&before, "both", cross_profile), |_, _| Ok(())).unwrap();
        assert!(completed.exists());
        assert_eq!(
            committed.instance.worktree_info.as_ref().unwrap().branch,
            "new"
        );
        assert!(root.path().join("new/tracked").exists());
        assert!(move_journal::scan().entries.is_empty());
        if cross_profile {
            assert!(source.load().unwrap().is_empty());
            assert_eq!(target.load().unwrap()[0].created_at, before.created_at);
        } else {
            assert_eq!(source.load().unwrap()[0].created_at, before.created_at);
        }
    }
}

#[test]
#[serial_test::serial]
fn native_adoption_after_partial_git_effect_keeps_an_explicit_conflict() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, source, before) = fixture();
    let peer = Storage::new_unwatched("peer").unwrap();
    let completed = reference_hook(root.path(), &before, "committed", "adopt-rename", "peer");
    assert!(perform(edit_request(&before, "both", false), |_, _| Ok(())).is_err());
    assert!(completed.exists());
    let adopter = peer.load().unwrap();
    assert_eq!(adopter.len(), 1);
    assert_eq!(adopter[0].project_path, before.project_path);
    assert!(Path::new(&before.project_path).join("tracked").exists());
    assert!(!root.path().join("new").exists());
    let pending = move_journal::scan();
    assert_eq!(pending.entries.len(), 1);
    assert!(matches!(&pending.entries[0].1.as_ref().unwrap().authority,
        MoveAuthority::Rename { intent } if intent.phase == EffectPhase::Conflict));
    let original = source.load().unwrap();
    assert_eq!(original[0].created_at, before.created_at);
    let bytes = std::fs::read(peer.sessions_path()).unwrap();
    assert!(
        !Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), bytes);
    assert_eq!(move_journal::scan().entries.len(), 1);
}

#[test]
#[serial_test::serial]
fn post_effect_supersession_never_rolls_back_or_overwrites_new_owner_data() {
    for change in [
        "generation",
        "incarnation",
        "profile",
        "checkout",
        "destination-owner",
    ] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, source, before) = fixture();
        let peer = Storage::new_unwatched("peer").unwrap();
        let edited = source.clone();
        let adopter = peer.clone();
        let retired_profile = root.path().join("retired-profile");
        let retired_checkout = root.path().join("retired-checkout");
        let result = with_after_effects(
            move |ownership, original, candidate| {
                if change == "destination-owner" {
                    adopter
                        .update_with_ownership(ownership, |rows, _| {
                            rows.push(Instance::new("new owner", &candidate.project_path));
                            Ok(())
                        })
                        .unwrap();
                    return;
                }
                if change == "profile" {
                    let directory = edited.sessions_path().parent().unwrap();
                    std::fs::rename(directory, &retired_profile).unwrap();
                    std::fs::create_dir(directory).unwrap();
                    let replacement =
                        Storage::open_unwatched_with_ownership("owner", ownership).unwrap();
                    replacement
                        .update_with_ownership(ownership, |rows, _| {
                            let mut row = candidate.clone();
                            row.created_at += chrono::Duration::seconds(1);
                            row.title = "replacement owner".into();
                            rows.push(row);
                            Ok(())
                        })
                        .unwrap();
                    return;
                }
                if change == "checkout" {
                    std::fs::rename(&candidate.project_path, &retired_checkout).unwrap();
                    std::fs::create_dir(&candidate.project_path).unwrap();
                    std::fs::write(
                        Path::new(&candidate.project_path).join("replacement"),
                        "keep",
                    )
                    .unwrap();
                }
                edited
                    .update_with_ownership(ownership, |rows, _| {
                        let row = rows.iter_mut().find(|row| row.id == original.id).unwrap();
                        row.project_path.clone_from(&candidate.project_path);
                        row.worktree_info.clone_from(&candidate.worktree_info);
                        row.title = "replacement owner".into();
                        if change == "generation" {
                            row.release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Rename,
                                original.lifecycle_generation,
                            );
                            row.try_acquire_lifecycle_reservation(
                                LifecycleOperation::Trash,
                                Instance::LIFECYCLE_RESERVATION_TTL,
                                Utc::now(),
                            )?;
                        } else {
                            row.created_at += chrono::Duration::seconds(1);
                        }
                        if change == "checkout" {
                            row.worktree_info = None;
                        }
                        Ok(())
                    })
                    .unwrap();
            },
            || perform(edit_request(&before, "both", false), |_, _| Ok(())),
        );
        assert!(result.is_err(), "{change}");
        let current = Storage::open_unwatched("owner").unwrap();
        let source_bytes = std::fs::read(current.sessions_path()).unwrap();
        let peer_bytes = peer
            .sessions_path()
            .try_exists()
            .unwrap()
            .then(|| std::fs::read(peer.sessions_path()).unwrap());
        let pending = move_journal::scan();
        assert_eq!(pending.entries.len(), 1, "{change}");
        assert!(
            matches!(&pending.entries[0].1.as_ref().unwrap().authority,
            MoveAuthority::Rename { intent } if intent.phase == EffectPhase::Conflict),
            "{change}"
        );
        assert!(
            !Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read(current.sessions_path()).unwrap(),
            source_bytes,
            "{change}"
        );
        if let Some(bytes) = peer_bytes {
            assert_eq!(
                std::fs::read(peer.sessions_path()).unwrap(),
                bytes,
                "{change}"
            );
        }
        assert!(
            root.path().join("new").exists(),
            "{change}: adopted output must not be moved"
        );
        if change == "checkout" {
            assert_eq!(
                std::fs::read(root.path().join("new/replacement")).unwrap(),
                b"keep"
            );
            assert!(root.path().join("retired-checkout/tracked").exists());
        } else {
            assert!(root.path().join("new/tracked").exists(), "{change}");
        }
        let repo =
            git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path).unwrap();
        assert!(
            repo.find_reference("refs/heads/new").is_ok(),
            "{change}: never delete an adopted resulting ref"
        );
    }
}

#[test]
#[serial_test::serial]
fn same_oid_reference_replacement_in_committed_hook_is_not_reauthorized() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, _, before) = fixture();
    let completed = reference_hook(
        root.path(),
        &before,
        "committed",
        "replace-result-ref",
        "owner",
    );
    assert!(perform(edit_request(&before, "both", false), |_, _| Ok(())).is_err());
    assert!(
        completed.exists(),
        "the real committed hook must replace the resulting reference"
    );
    let repo =
        git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path).unwrap();
    let reference = repo.commondir().join("refs/heads/new");
    let original = completed.with_extension("original-ref");
    let replacement_identity =
        storage::filesystem_identity(&std::fs::metadata(&reference).unwrap());
    assert_ne!(
        replacement_identity,
        storage::filesystem_identity(&std::fs::metadata(&original).unwrap())
    );
    let bytes = std::fs::read(&reference).unwrap();
    assert_eq!(bytes, std::fs::read(&original).unwrap());
    assert!(
        !Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        storage::filesystem_identity(&std::fs::metadata(&reference).unwrap()),
        replacement_identity
    );
    assert_eq!(std::fs::read(&reference).unwrap(), bytes);
    let pending = move_journal::scan();
    assert_eq!(pending.entries.len(), 1);
    assert!(matches!(&pending.entries[0].1.as_ref().unwrap().authority,
        MoveAuthority::Rename { intent } if intent.phase == EffectPhase::Conflict));
}

#[test]
#[serial_test::serial]
fn branch_recreation_never_adopts_same_oid_refs_from_prepared_or_replaced_committed_hooks() {
    use crate::session::environment::shell_escape;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    for phase in ["prepared", "committed"] {
        let _app = crate::session::test_support::isolate_app_dir();
        let (fixture, source, before) = fixture();
        let info = before.worktree_info.as_ref().unwrap();
        let ownership = storage::acquire_ownership_lock().unwrap();
        let identity = storage::acquire_session_identity_lock_with_ownership(&ownership).unwrap();
        let title =
            storage::acquire_session_title_lock_with_ownership(&ownership, &before.id).unwrap();
        let lifecycle = source
            .acquire_instance_lifecycle_lock_with_ownership(&ownership, &before.id)
            .unwrap();
        let origin = BranchOrigin::capture(Path::new(&info.main_repo_path), &info.branch).unwrap();
        let authority = origin.durable_authority().unwrap();
        let reference = authority.common.path.join(&authority.reference);
        assert!(Command::new("git")
            .current_dir(&info.main_repo_path)
            .args(["update-ref", "-d", &authority.reference, &authority.tip])
            .status()
            .unwrap()
            .success());
        let entered = fixture.path().join("foreign-ref-written");
        let retired = fixture.path().join("original-created-ref");
        let effect = if phase == "prepared" {
            format!(
                "printf '%s\\n' {} > {}\n",
                shell_escape(&authority.tip),
                shell_escape(&reference.to_string_lossy())
            )
        } else {
            format!(
                "mv {} {}\ncp {} {}\n",
                shell_escape(&reference.to_string_lossy()),
                shell_escape(&retired.to_string_lossy()),
                shell_escape(&retired.to_string_lossy()),
                shell_escape(&reference.to_string_lossy())
            )
        };
        let hook = authority.common.path.join("hooks/reference-transaction");
        std::fs::write(&hook, format!(
            "#!/bin/sh\n[ \"$1\" = {} ] || exit 0\n[ ! -e {entered} ] || exit 0\n{effect}printf written > {entered}\n",
            shell_escape(phase), entered = shell_escape(&entered.to_string_lossy())
        )).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let row_bytes = std::fs::read(source.sessions_path()).unwrap();
        let mut receipt = None;
        let result = super::super::native_hooks::restore_missing_branch(
            super::super::native_hooks::BranchHandoff {
                root: &ownership,
                nested: &[&identity, &title, &lifecycle],
                owner: Some((source.profile(), &before.id)),
                protected_path: None,
            },
            &authority,
            Path::new(&before.project_path),
            &mut receipt,
        );
        assert!(
            result.is_err(),
            "{phase}: the foreign reference must refuse continuation"
        );
        assert!(
            entered.exists(),
            "{phase}: the real reference hook must execute"
        );
        let current = storage::filesystem_identity(&std::fs::metadata(&reference).unwrap());
        let repo = git2::Repository::open(&info.main_repo_path).unwrap();
        assert_eq!(
            repo.find_reference(&authority.reference)
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            authority.tip
        );
        if phase == "prepared" {
            assert!(
                receipt.is_none(),
                "a prepared hook has no committed owner receipt"
            );
            assert_ne!(Some(current), authority.loose.identity);
        } else {
            let receipt = receipt.expect("the committed owner receipt survives the later error");
            assert_eq!(
                receipt.identity,
                Some(storage::filesystem_identity(
                    &std::fs::metadata(&retired).unwrap()
                ))
            );
            assert_ne!(receipt.identity, Some(current));
            assert!(
                receipt.verify().is_err(),
                "never replace the original receipt with the same-OID foreign inode"
            );
            assert_eq!(
                std::fs::read(&reference).unwrap(),
                std::fs::read(&retired).unwrap()
            );
        }
        assert_eq!(std::fs::read(source.sessions_path()).unwrap(), row_bytes);
    }
}
