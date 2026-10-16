//! Add durable post-retirement cleanup without inventing historical store authority.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;

use crate::session::move_journal::{
    EffectPhase, EffectRecord, MoveAuthority, MoveJournalEntry, ResourceEffect,
    MOVE_JOURNAL_VERSION,
};

pub fn run() -> Result<()> {
    let root = crate::session::storage::acquire_ownership_lock()?;
    let directory = root.app_dir().join(".move-journal");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                matches!(fs::symlink_metadata(&directory),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "journal directory is an unreadable alias"
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    for item in entries {
        let item = item?;
        let path = item.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        anyhow::ensure!(item.file_type()?.is_file(), "journal is not a regular file");
        let bytes = fs::read(&path)?;
        let mut entry: MoveJournalEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding historical journal {}", path.display()))?;
        if entry.version == MOVE_JOURNAL_VERSION {
            continue;
        }
        anyhow::ensure!(
            entry.version == 3,
            "unsupported historical journal version {}",
            entry.version
        );
        let lock_path = path.with_extension("lock");
        let open = |create| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(create)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&lock_path)
        };
        let lease = match open(false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => open(true)?,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            lease.metadata()?.is_file(),
            "journal lease is not a regular file"
        );
        lease
            .try_lock_exclusive()
            .context("journal still has a live producer; retry after it finishes")?;
        anyhow::ensure!(
            fs::read(&path)? == bytes,
            "journal changed during migration"
        );
        upgrade(&mut entry, &root.app_dir().join("acp_events.db"));
        crate::session::backup_before_migration(&path)?;
        crate::session::storage::atomic_write_verified(&path, &serde_json::to_vec_pretty(&entry)?)?;
        crate::session::storage::sync_parent_directory(&path)?;
        tracing::info!(journal = %path.display(), "v043: recorded durable purge cleanup");
    }
    Ok(())
}

fn upgrade(entry: &mut MoveJournalEntry, database: &std::path::Path) {
    if entry.version == MOVE_JOURNAL_VERSION {
        return;
    }
    entry.version = MOVE_JOURNAL_VERSION;
    let MoveAuthority::Resource { intent } = &mut entry.authority else {
        return;
    };
    if intent.operation != crate::session::LifecycleOperation::Purge {
        return;
    }
    let unknown_callback = intent.effects.iter().any(|record| {
        matches!(&record.effect, ResourceEffect::Callback { name, .. }
            if !matches!(name.as_str(), "purge-hooks" | "purge-quiesce" | "purge-preclean"
                | "purge-empty-workspace" | "purge-sidecars"))
    });
    let needs_stores = intent
        .effects
        .iter()
        .any(|record| matches!(record.effect, ResourceEffect::ReleaseContainer));
    let mut missing_receipt = false;
    for record in &mut intent.effects {
        if matches!(&record.effect, ResourceEffect::Callback { name, .. } if name == "purge-sidecars")
        {
            let completed = record.phase == EffectPhase::Applied
                && record.error.is_none()
                && record.before.is_some()
                && record.after.is_some();
            if needs_stores && !completed {
                missing_receipt = true;
            } else if !needs_stores {
                record.effect = ResourceEffect::PurgeSidecars { stores: None };
            }
        }
    }
    if !intent
        .effects
        .iter()
        .any(|record| matches!(record.effect, ResourceEffect::PurgeAcpEvents { .. }))
    {
        intent.effects.insert(
            2.min(intent.effects.len()),
            EffectRecord {
                effect: ResourceEffect::PurgeAcpEvents {
                    database: database.to_path_buf(),
                },
                phase: EffectPhase::Prepared,
                before: None,
                after: None,
                error: None,
                compensation: false,
                retained: false,
            },
        );
        if intent.phase == EffectPhase::Committed {
            intent.phase = EffectPhase::Applied;
        }
    }
    if missing_receipt || unknown_callback {
        intent.phase = EffectPhase::Conflict;
        let reason = if missing_receipt {
            "historical purge lacks its captured sandbox-store removal receipt; current configuration cannot authorize deletion"
        } else {
            "historical purge contains an unknown callback; its cleanup authority cannot be reconstructed"
        };
        intent.conflict = Some(match intent.conflict.take() {
            Some(previous) => format!("{previous}; {reason}"),
            None => reason.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::move_journal::{IntentOwner, ProfileAuthority, ResourceIntent};

    #[test]
    fn v3_purge_upgrade_preserves_uncertainty_and_never_recaptures_stores() {
        for sandbox in [false, true] {
            for (phase, failed, committed) in [
                (EffectPhase::Prepared, false, false),
                (EffectPhase::Applied, false, false),
                (EffectPhase::Applied, false, true),
                (EffectPhase::Applied, true, false),
                (EffectPhase::Pending, false, false),
                (EffectPhase::Conflict, false, false),
            ] {
                let owner = crate::session::Instance::new("migration", "/absent");
                let profile = ProfileAuthority {
                    profile: "test".into(),
                    sessions_path: "/test/sessions.json".into(),
                };
                let mut effects = vec![ResourceEffect::Callback {
                    name: "purge-hooks".into(),
                    directory: "/absent".into(),
                    checkouts: vec![],
                }];
                if sandbox {
                    effects.push(ResourceEffect::ReleaseContainer);
                }
                effects.push(ResourceEffect::Callback {
                    name: "purge-sidecars".into(),
                    directory: "/absent".into(),
                    checkouts: vec![],
                });
                let mut intent = ResourceIntent::new(
                    IntentOwner {
                        profile: profile.clone(),
                        id: owner.id.clone(),
                        created_at: owner.created_at,
                        generation: owner.lifecycle_generation,
                    },
                    profile,
                    crate::session::LifecycleOperation::Purge,
                    Some(owner),
                    None,
                    effects,
                    vec![],
                )
                .unwrap();
                intent.effects.last_mut().unwrap().phase = phase;
                let sidecar = intent.effects.last_mut().unwrap();
                if phase == EffectPhase::Applied {
                    sidecar.before = Some(Default::default());
                    sidecar.after = Some(Default::default());
                }
                sidecar.error = failed.then(|| "cleanup failed".into());
                if committed {
                    intent.phase = EffectPhase::Committed;
                }
                let mut entry = MoveJournalEntry {
                    version: 3,
                    ids: vec![intent.owner.id.clone()],
                    source_profile: "test".into(),
                    target_profile: "test".into(),
                    source_sessions_path: "/test/sessions.json".into(),
                    target_sessions_path: "/test/sessions.json".into(),
                    group_move_source_path: String::new(),
                    group_move_target_path: String::new(),
                    group_move_subtree: false,
                    created_at_epoch_ms: 0,
                    authority: MoveAuthority::Resource {
                        intent: Box::new(intent),
                    },
                };
                upgrade(&mut entry, std::path::Path::new("/old/acp_events.db"));
                let once = serde_json::to_value(&entry).unwrap();
                upgrade(&mut entry, std::path::Path::new("/new/acp_events.db"));
                assert_eq!(serde_json::to_value(&entry).unwrap(), once);
                let MoveAuthority::Resource { intent } = &entry.authority else {
                    unreachable!()
                };
                let sidecar = intent.effects.iter().find(|record| matches!(&record.effect,
                    ResourceEffect::PurgeSidecars { .. } | ResourceEffect::Callback { .. })
                    && !matches!(&record.effect, ResourceEffect::Callback { name, .. } if name != "purge-sidecars")).unwrap();
                assert_eq!(sidecar.phase, phase);
                assert_eq!(
                    intent.phase == EffectPhase::Conflict,
                    sandbox && (phase != EffectPhase::Applied || failed)
                );
                if phase == EffectPhase::Applied && !failed {
                    assert_eq!(sidecar.before, Some(Default::default()));
                    assert_eq!(sidecar.after, Some(Default::default()));
                    assert!(sidecar.error.is_none());
                    assert_ne!(
                        intent.phase,
                        EffectPhase::Committed,
                        "new ACP cleanup must run even after an old committed purge"
                    );
                }
                assert_eq!(
                    intent
                        .effects
                        .iter()
                        .filter(|record| matches!(
                            record.effect,
                            ResourceEffect::PurgeAcpEvents { .. }
                        ))
                        .count(),
                    1
                );
                assert!(!intent.effects.iter().any(|record| matches!(
                    record.effect,
                    ResourceEffect::PurgeSidecars { stores: Some(_) }
                )));
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn v3_all_applied_purge_is_backed_up_and_replays_new_acp_obligation() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("migration-owner")?;
        storage.update(|_, _| Ok(()))?;
        let directory = tempfile::tempdir()?;
        let mut owner =
            crate::session::Instance::new("removed", &directory.path().to_string_lossy());
        owner.source_profile = storage.profile().into();
        let profile = ProfileAuthority::capture(&storage)?;
        let mut intent = ResourceIntent::new(
            IntentOwner {
                profile: profile.clone(),
                id: owner.id.clone(),
                created_at: owner.created_at,
                generation: owner.lifecycle_generation,
            },
            profile,
            crate::session::LifecycleOperation::Purge,
            Some(owner.clone()),
            None,
            ["purge-hooks", "purge-quiesce", "purge-sidecars"]
                .into_iter()
                .map(|name| ResourceEffect::Callback {
                    name: name.into(),
                    directory: directory.path().into(),
                    checkouts: vec![],
                })
                .collect(),
            vec![],
        )?;
        intent.effects.insert(
            2,
            EffectRecord {
                effect: ResourceEffect::ReleaseContainer,
                phase: EffectPhase::Prepared,
                before: None,
                after: None,
                error: None,
                compensation: false,
                retained: false,
            },
        );
        for record in &mut intent.effects {
            record.phase = EffectPhase::Applied;
            record.before = Some(record.effect.observe()?);
            record.after = record.before.clone();
            if matches!(record.effect, ResourceEffect::ReleaseContainer) {
                record.after.as_mut().unwrap().container_released = Some(true);
            }
        }
        intent.phase = EffectPhase::Committed;
        let entry = MoveJournalEntry {
            version: 3,
            ids: vec![owner.id.clone()],
            source_profile: storage.profile().into(),
            target_profile: storage.profile().into(),
            source_sessions_path: storage.sessions_path().into(),
            target_sessions_path: storage.sessions_path().into(),
            group_move_source_path: String::new(),
            group_move_target_path: String::new(),
            group_move_subtree: false,
            created_at_epoch_ms: 0,
            authority: MoveAuthority::Resource {
                intent: Box::new(intent),
            },
        };
        let app = crate::session::get_app_dir()?;
        let journal_dir = app.join(".move-journal");
        fs::create_dir_all(&journal_dir)?;
        let path = journal_dir.join("move-123-durable-purge.json");
        let bytes = serde_json::to_vec_pretty(&entry)?;
        fs::write(&path, &bytes)?;
        let conn = rusqlite::Connection::open(app.join("acp_events.db"))?;
        conn.execute_batch("CREATE TABLE acp_events (session_id TEXT)")?;
        conn.execute("INSERT INTO acp_events VALUES (?1)", [&owner.id])?;
        run()?;
        let upgraded = fs::read(&path)?;
        run()?;
        assert_eq!(fs::read(&path)?, upgraded);
        let backups = crate::session::storage::migration_backups(&path)?;
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0].1)?, bytes);
        for _ in 0..2 {
            assert!(crate::session::Storage::recover_pending_moves(
                crate::file_watch::FileWatchService::noop()
            )?
            .is_empty());
        }
        assert!(!path.exists());
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM acp_events", [], |row| row.get(0))?;
        assert_eq!(count, 0);
        Ok(())
    }
}
