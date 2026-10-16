//! Legacy intents lack incarnation authority and must never choose a winner.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

#[derive(Deserialize, Serialize)]
struct LegacyEntry {
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

// Version 41 always writes version 2, independently of the current runtime codec.
#[derive(Deserialize, Serialize)]
struct HistoricalEntry {
    #[serde(flatten)]
    header: LegacyEntry,
    authority: HistoricalAuthority,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HistoricalAuthority {
    LegacyConflict {
        legacy: serde_json::Value,
        reason: String,
    },
}

pub fn run() -> Result<()> {
    let ownership = crate::session::storage::acquire_ownership_lock()?;
    let profiles = crate::session::list_profiles_for_worktree_inventory()?;
    let central = ownership.app_dir().join(".move-journal");
    // Finish the strict inventory before moving any evidence.
    let mut journals = Vec::new();
    for profile in profiles {
        let directory = ownership
            .app_dir()
            .join("profiles")
            .join(profile)
            .join(".move-journal");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&directory) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspecting {}", directory.display()))
                    }
                    Ok(_) => anyhow::bail!(
                        "legacy journal directory is an unreadable alias: {}",
                        directory.display()
                    ),
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("listing {}", directory.display()))
            }
        };
        for entry in entries {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                anyhow::ensure!(
                    entry.file_type()?.is_file(),
                    "legacy journal is not a regular file"
                );
                journals.push(entry.path());
            }
        }
    }
    journals.sort();
    for path in journals {
        let bytes = fs::read(&path)?;
        let legacy: LegacyEntry = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decoding legacy intent {}; original evidence retained",
                path.display()
            )
        })?;
        anyhow::ensure!(
            legacy.version == 1,
            "unsupported legacy move-journal version {} at {}",
            legacy.version,
            path.display()
        );
        let mut hash = Sha256::new();
        hash.update(path.canonicalize()?.as_os_str().as_encoded_bytes());
        hash.update(&bytes);
        let digest = hash.finalize();
        use std::fmt::Write as _;
        let mut name = String::with_capacity("legacy-".len() + digest.len() * 2 + ".json".len());
        name.push_str("legacy-");
        for byte in digest {
            write!(&mut name, "{byte:02x}")?;
        }
        name.push_str(".json");
        let destination = central.join(name);
        let migrated = HistoricalEntry {
            header: LegacyEntry { version: 2, ..legacy },
            authority: HistoricalAuthority::LegacyConflict {
                legacy: serde_json::from_slice(&bytes)?,
                reason: "legacy v1 intent has no original incarnation, generation or physical-resource authority".into(),
            },
        };
        let migrated_bytes = serde_json::to_vec_pretty(&migrated)?;
        crate::session::backup_before_migration(&path)?;
        fs::create_dir_all(&central)?;
        crate::session::storage::sync_parent_directory(&central)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "migration destination is not an original regular journal"
                );
                anyhow::ensure!(
                    fs::read(&destination)? == migrated_bytes,
                    "migration destination contains different evidence"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::session::storage::atomic_write_verified(&destination, &migrated_bytes)?;
            }
            Err(error) => return Err(error.into()),
        }
        crate::session::storage::sync_parent_directory(&destination)?;
        anyhow::ensure!(
            fs::read(&destination)? == migrated_bytes,
            "central legacy conflict was not verified"
        );
        fs::remove_file(&path)?;
        crate::session::storage::sync_parent_directory(&path)?;
        tracing::info!(source = %path.display(), destination = %destination.display(), "v041: preserved legacy move intent as a non-authoritative conflict");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn legacy_intents_are_backed_up_central_conflicts_and_migration_is_idempotent() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("owner")?;
        let directory = storage
            .sessions_path()
            .parent()
            .unwrap()
            .join(".move-journal");
        fs::create_dir(&directory)?;
        let path = directory.join("move-1-2.json");
        let old = serde_json::json!({
            "version": 1, "ids": ["original-id"], "source_profile": "owner", "target_profile": "target",
            "source_sessions_path": storage.sessions_path(), "target_sessions_path": "/original/target/sessions.json",
            "group_move_source_path": "before", "group_move_target_path": "after", "group_move_subtree": false,
            "created_at_epoch_ms": 123,
        });
        let bytes = serde_json::to_vec_pretty(&old)?;
        fs::write(&path, &bytes)?;
        run()?;
        assert!(!path.exists());
        let scanned = crate::session::move_journal::scan();
        assert_eq!(scanned.entries.len(), 1);
        let (central, _) = &scanned.entries[0];
        let parsed: HistoricalEntry = serde_json::from_slice(&fs::read(central)?)?;
        assert_eq!(parsed.header.version, 2);
        let HistoricalAuthority::LegacyConflict { legacy, .. } = parsed.authority;
        assert_eq!(legacy, old);
        let backups = crate::session::storage::migration_backups(&path)?;
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0].1)?, bytes);
        let first = fs::read(central)?;
        run()?;
        assert_eq!(fs::read(central)?, first);
        assert_eq!(crate::session::move_journal::scan().entries.len(), 1);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn uncertain_profile_inventory_leaves_legacy_evidence_untouched() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let app = crate::session::get_app_dir()?;
        let storage = crate::session::Storage::new_unwatched("owner")?;
        let directory = storage
            .sessions_path()
            .parent()
            .unwrap()
            .join(".move-journal");
        fs::create_dir(&directory)?;
        let path = directory.join("move-1-2.json");
        fs::write(&path, b"original evidence")?;
        std::os::unix::fs::symlink(app.join("missing"), app.join("profiles/broken"))?;
        assert!(run().is_err());
        assert_eq!(fs::read(path)?, b"original evidence");
        assert!(!app.join(".move-journal").exists());
        Ok(())
    }
}
