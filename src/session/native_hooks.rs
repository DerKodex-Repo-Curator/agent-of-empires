use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::builder::{CheckoutWitness, CleanupProtection, CreatedWorktree, DirectoryWitness};
use super::storage::{self, OwnershipGuard};
use super::Storage;

pub(crate) trait NativeOwner {
    fn release(&mut self, ownership: &OwnershipGuard) -> Result<()>;
    fn restore(&mut self, ownership: &OwnershipGuard) -> Result<()>;
}

#[derive(Hash, PartialEq, Eq)]
struct Claim {
    profile: (u64, u64),
    id: String,
    born: chrono::DateTime<chrono::Utc>,
    path: PathBuf,
    branch: Option<(PathBuf, String)>,
}

fn inventory(ownership: &OwnershipGuard) -> Result<(HashSet<Claim>, Vec<DirectoryWitness>)> {
    let mut claims = HashSet::new();
    let mut directories = Vec::new();
    let mut seen = HashSet::new();
    for profile in super::list_profiles_for_worktree_inventory()? {
        let storage = Storage::open_unwatched_with_ownership(&profile, ownership)?;
        let physical = storage.physical_profile_identity()?;
        if !seen.insert(physical) {
            continue;
        }
        directories.push(DirectoryWitness::capture(
            storage
                .sessions_path()
                .parent()
                .context("profile directory missing")?,
        )?);
        for row in storage.load_strict_for_worktree_ownership()? {
            let mut add = |path: &str, branch: Option<(&str, &str)>| -> Result<()> {
                claims.insert(Claim {
                    profile: physical,
                    id: row.id.clone(),
                    born: row.created_at,
                    path: super::deletion::resolve_claim_path(Path::new(path))
                        .context("unresolved checkout claim")?,
                    branch: branch
                        .map(|(repo, branch)| -> Result<_> {
                            Ok((
                                super::deletion::resolve_claim_path(Path::new(repo))
                                    .context("unresolved repository claim")?,
                                branch.to_owned(),
                            ))
                        })
                        .transpose()?,
                });
                Ok(())
            };
            add(
                &row.project_path,
                row.worktree_info
                    .as_ref()
                    .map(|wt| (wt.main_repo_path.as_str(), wt.branch.as_str())),
            )?;
            if let Some(path) = &row.pre_trash_project_path {
                add(path, None)?;
            }
            if let Some(workspace) = &row.workspace_info {
                add(&workspace.workspace_dir, None)?;
                for repo in &workspace.repos {
                    add(
                        &repo.worktree_path,
                        Some((&repo.main_repo_path, &repo.branch)),
                    )?;
                }
            }
        }
    }
    Ok((claims, directories))
}

pub(crate) struct ClaimSnapshot {
    claims: HashSet<Claim>,
    _pins: Vec<DirectoryWitness>,
}

impl ClaimSnapshot {
    pub(crate) fn capture(root: &OwnershipGuard) -> Result<Self> {
        let (claims, pins) = inventory(root)?;
        Ok(Self {
            claims,
            _pins: pins,
        })
    }

    pub(crate) fn verify(
        &self,
        root: &OwnershipGuard,
        main_repo: &Path,
        target: &Path,
        branch: &str,
    ) -> Result<()> {
        verify_claims(root, &self.claims, main_repo, target, branch)
    }
}

fn verify_claims(
    root: &OwnershipGuard,
    baseline: &HashSet<Claim>,
    main_repo: &Path,
    target: &Path,
    branch: &str,
) -> Result<()> {
    let (claims, _pins) = inventory(root)?;
    let target =
        super::deletion::resolve_claim_path(target).context("unresolved native checkout")?;
    let main_repo =
        super::deletion::resolve_claim_path(main_repo).context("unresolved native repository")?;
    for claim in claims.difference(baseline) {
        anyhow::ensure!(
            !(claim.path.starts_with(&target) || target.starts_with(&claim.path))
                && !claim
                    .branch
                    .as_ref()
                    .is_some_and(|(repo, name)| repo == &main_repo && name == branch),
            "Checkout was adopted during a native Git callback; canceling creation"
        );
    }
    Ok(())
}

pub(crate) struct NativeLease<'a> {
    ownership: &'a mut Option<OwnershipGuard>,
    exclusive: bool,
    app_dir: PathBuf,
    profile: Option<&'a Storage>,
    owner: Option<&'a mut dyn NativeOwner>,
    directories: Vec<DirectoryWitness>,
    source_git: DirectoryWitness,
    source_common: DirectoryWitness,
    profile_pins: Vec<DirectoryWitness>,
    claims: HashSet<Claim>,
    main_repo: PathBuf,
    path: PathBuf,
    branch: String,
    checkout: Option<CheckoutWitness>,
    children: Vec<(PathBuf, CheckoutWitness)>,
}

impl<'a> NativeLease<'a> {
    pub(crate) fn new(
        ownership: &'a mut Option<OwnershipGuard>,
        exclusive: bool,
        profile: Option<&'a Storage>,
        owner: Option<&'a mut dyn NativeOwner>,
        checkout: (&Path, &Path, &str),
    ) -> Result<Self> {
        let (main_repo, path, branch) = checkout;
        let root = ownership
            .as_ref()
            .context("native Git ownership lease missing")?;
        let app_dir = root.app_dir().to_path_buf();
        let (claims, profile_pins) = inventory(root)?;
        let mut parent = path.parent().context("checkout parent missing")?;
        while !parent.exists() {
            parent = parent.parent().context("checkout ancestor missing")?;
        }
        let repo = git2::Repository::open(main_repo)?;
        let mut directories = vec![
            DirectoryWitness::capture(&app_dir)?,
            DirectoryWitness::capture(main_repo)?,
            DirectoryWitness::capture(repo.commondir())?,
            DirectoryWitness::capture(parent)?,
        ];
        if let Some(profile) = profile {
            profile.verify_profile_identity()?;
            directories.push(DirectoryWitness::capture(
                profile
                    .sessions_path()
                    .parent()
                    .context("profile directory missing")?,
            )?);
        }
        Ok(Self {
            ownership,
            exclusive,
            app_dir,
            profile,
            owner,
            directories,
            source_git: DirectoryWitness::capture(repo.path())?,
            source_common: DirectoryWitness::capture(repo.commondir())?,
            profile_pins,
            claims,
            main_repo: main_repo.to_path_buf(),
            path: path.to_path_buf(),
            branch: branch.to_owned(),
            checkout: None,
            children: Vec::new(),
        })
    }

    fn pin_checkout(&mut self) -> Result<()> {
        if self.checkout.is_none() {
            self.checkout = Some(CheckoutWitness::capture(&self.path)?);
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        for directory in &self.directories {
            directory.verify()?;
        }
        let source = git2::Repository::open(&self.main_repo)?;
        self.source_git.verify_at(source.path())?;
        self.source_common.verify_at(source.commondir())?;
        if let Some(profile) = self.profile {
            profile.verify_profile_identity()?;
        }
        if let Some(checkout) = &self.checkout {
            checkout.verify()?;
        }
        for (_, checkout) in &self.children {
            checkout.verify()?;
        }
        let root = self
            .ownership
            .as_ref()
            .context("native Git ownership lease missing")?;
        verify_claims(
            root,
            &self.claims,
            &self.main_repo,
            &self.path,
            &self.branch,
        )
    }

    pub(crate) fn create(
        &mut self,
        git: &crate::git::GitWorktree,
        create_branch: bool,
        base: Option<&str>,
    ) -> Result<(Vec<String>, CheckoutWitness)> {
        let branch = self.branch.clone();
        let path = self.path.clone();
        let warnings =
            git.create_worktree_with_hooks(&branch, &path, create_branch, base, Some(self))?;
        self.pin_checkout()?;
        self.verify()?;
        Ok((
            warnings,
            self.checkout
                .clone()
                .context("native checkout witness missing")?,
        ))
    }

    pub(crate) fn receipt(&self, create_branch: bool) -> Option<CreatedWorktree> {
        self.checkout.clone().map(|witness| CreatedWorktree {
            path: self.path.clone(),
            main_repo_path: self.main_repo.clone(),
            owned_branch: create_branch.then(|| self.branch.clone()),
            witness,
        })
    }
}

impl crate::git::native_hooks::HookLease for NativeLease<'_> {
    fn runtime_dir(&self) -> &Path {
        &self.app_dir
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        self.pin_checkout()
            .and_then(|_| self.verify())
            .map_err(|error| io::Error::other(format!("{error:#}")))
    }

    fn pause(&mut self, cwd: &Path) -> io::Result<()> {
        let mut prepare = || -> Result<()> {
            self.pin_checkout()?;
            if cwd != self.path && !self.children.iter().any(|(path, _)| path == cwd) {
                self.children
                    .push((cwd.to_path_buf(), CheckoutWitness::capture(cwd)?));
            }
            self.verify()?;
            let root = self
                .ownership
                .as_ref()
                .context("native Git ownership lease missing")?;
            let (_, pins) = inventory(root)?;
            self.profile_pins.extend(pins);
            if let Some(owner) = &mut self.owner {
                owner.release(root)?;
            }
            drop(self.ownership.take());
            Ok(())
        };
        prepare().map_err(|error| io::Error::other(format!("{error:#}")))
    }

    fn resume(&mut self) -> io::Result<()> {
        let mut resume = || -> Result<()> {
            *self.ownership = Some(if self.exclusive {
                storage::acquire_ownership_lock()?
            } else {
                storage::acquire_ownership_read()?
            });
            self.verify()?;
            if let Some(owner) = &mut self.owner {
                owner.restore(
                    self.ownership
                        .as_ref()
                        .context("native Git ownership lease missing")?,
                )?;
            }
            Ok(())
        };
        resume().map_err(|error| io::Error::other(format!("{error:#}")))
    }
}

pub(crate) fn create_worktree(
    ownership: &mut Option<OwnershipGuard>,
    profile: Option<&Storage>,
    git: &crate::git::GitWorktree,
    checkout: (&Path, &Path, &str),
    create_branch: bool,
    base: Option<&str>,
) -> Result<(Vec<String>, CheckoutWitness)> {
    let mut lease = NativeLease::new(ownership, false, profile, None, checkout)?;
    let result = lease.create(git, create_branch, base);
    let receipt = lease.receipt(create_branch);
    drop(lease);
    if result.is_err() {
        drop(ownership.take());
        cleanup(receipt.as_ref());
    }
    result
}

pub(crate) fn cleanup(receipt: Option<&CreatedWorktree>) {
    let Some(receipt) = receipt else {
        return;
    };
    let Ok(root) = storage::acquire_ownership_lock() else {
        return;
    };
    cleanup_with_ownership(receipt, &root);
}

pub(crate) fn cleanup_with_ownership(receipt: &CreatedWorktree, root: &OwnershipGuard) {
    let Ok(owners) = super::builder::cleanup_owners(root) else {
        return;
    };
    super::builder::cleanup_created_worktree(
        receipt,
        "native Git creation",
        &CleanupProtection {
            owner: None,
            owners,
        },
    );
}
