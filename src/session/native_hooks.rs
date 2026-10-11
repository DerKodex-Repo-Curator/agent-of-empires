use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::builder::{
    CheckoutWitness, CleanupProtection, CreatedWorktree, DirectoryWitness, ProvisionalCreation,
};
use super::storage::{self, OwnershipGuard};
use super::Storage;

pub(crate) trait NativeOwner {
    fn release(&mut self, ownership: &OwnershipGuard) -> Result<()>;
    fn restore(&mut self, ownership: &OwnershipGuard) -> Result<()>;
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct Claim {
    pub(crate) profile: (u64, u64),
    id: String,
    born: chrono::DateTime<chrono::Utc>,
    path: PathBuf,
    branch: Option<(PathBuf, String)>,
}

impl Claim {
    pub(crate) fn from_instance(profile: (u64, u64), row: &super::Instance) -> Result<Self> {
        Self::for_path(
            profile,
            row,
            &row.project_path,
            row.worktree_info
                .as_ref()
                .map(|wt| (wt.main_repo_path.as_str(), wt.branch.as_str())),
        )
    }

    pub(crate) fn matches_instance(&self, row: &super::Instance) -> Result<bool> {
        if self.id != row.id || self.born != row.created_at {
            return Ok(false);
        }
        let path = super::deletion::resolve_claim_path(Path::new(&row.project_path))
            .context("unresolved checkout claim")?;
        if path != self.path {
            return Ok(false);
        }
        match (&self.branch, &row.worktree_info) {
            (None, None) => Ok(true),
            (Some((repo, branch)), Some(wt)) if branch == &wt.branch => {
                if wt.main_repo_path == row.project_path {
                    Ok(repo == &path)
                } else {
                    Ok(repo
                        == &super::deletion::resolve_claim_path(Path::new(&wt.main_repo_path))
                            .context("unresolved repository claim")?)
                }
            }
            _ => Ok(false),
        }
    }

    fn for_path(
        profile: (u64, u64),
        row: &super::Instance,
        path: &str,
        branch: Option<(&str, &str)>,
    ) -> Result<Self> {
        let resolved = super::deletion::resolve_claim_path(Path::new(path))
            .context("unresolved checkout claim")?;
        let branch = branch
            .map(|(repo, branch)| -> Result<_> {
                let repo = if repo == path {
                    resolved.clone()
                } else {
                    super::deletion::resolve_claim_path(Path::new(repo))
                        .context("unresolved repository claim")?
                };
                Ok((repo, branch.to_owned()))
            })
            .transpose()?;
        Ok(Self {
            profile,
            id: row.id.clone(),
            born: row.created_at,
            path: resolved,
            branch,
        })
    }
}

fn inventory(
    ownership: &OwnershipGuard,
    provisional: Option<&ProvisionalCreation>,
) -> Result<(HashSet<Claim>, Vec<DirectoryWitness>)> {
    if let Some(provisional) = provisional {
        provisional.profile_directory.verify()?;
    }
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
            let primary = Claim::from_instance(physical, &row)?;
            if let Some(provisional) =
                provisional.filter(|p| p.claim.profile == physical && p.claim.id == row.id)
            {
                anyhow::ensure!(
                    provisional.matches_claim(&row, &primary),
                    "Creating placeholder changed during native Git creation"
                );
                continue;
            }
            claims.insert(primary);
            let mut add = |path: &str, branch: Option<(&str, &str)>| -> Result<()> {
                claims.insert(Claim::for_path(physical, &row, path, branch)?);
                Ok(())
            };
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
    pub(crate) fn capture(
        root: &OwnershipGuard,
        provisional: Option<&ProvisionalCreation>,
    ) -> Result<Self> {
        let (claims, pins) = inventory(root, provisional)?;
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
        provisional: Option<&ProvisionalCreation>,
    ) -> Result<()> {
        verify_claims(root, &self.claims, main_repo, target, branch, provisional)
    }
}

fn verify_claims(
    root: &OwnershipGuard,
    baseline: &HashSet<Claim>,
    main_repo: &Path,
    target: &Path,
    branch: &str,
    provisional: Option<&ProvisionalCreation>,
) -> Result<()> {
    let (claims, _pins) = inventory(root, provisional)?;
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

enum EntryPin {
    Directory(DirectoryWitness),
    File {
        handle: std::fs::File,
        bytes: Vec<u8>,
    },
}

struct PathSnapshot {
    path: PathBuf,
    resolved: PathBuf,
    pin: Option<EntryPin>,
}

impl PathSnapshot {
    fn capture(path: PathBuf) -> Result<Self> {
        let resolved = super::deletion::resolve_claim_path(&path)
            .context("unresolved native checkout path")?;
        let pin = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let metadata = std::fs::metadata(&path)?;
                if metadata.is_dir() {
                    Some(EntryPin::Directory(DirectoryWitness::capture(&path)?))
                } else {
                    anyhow::ensure!(metadata.is_file(), "Native Git route is not a regular file");
                    use std::io::Read;
                    let mut handle = std::fs::File::open(&path)?;
                    let mut bytes = Vec::new();
                    handle.read_to_end(&mut bytes)?;
                    Some(EntryPin::File { handle, bytes })
                }
            }
        };
        Ok(Self {
            path,
            resolved,
            pin,
        })
    }

    fn verify(&self) -> Result<()> {
        anyhow::ensure!(
            super::deletion::resolve_claim_path(&self.path).as_ref() == Some(&self.resolved),
            "Native checkout path was redirected during a callback"
        );
        match &self.pin {
            None => anyhow::ensure!(
                matches!(std::fs::symlink_metadata(&self.path), Err(error) if error.kind() == io::ErrorKind::NotFound),
                "Native checkout resource appeared during a callback"
            ),
            Some(EntryPin::Directory(directory)) => directory.verify()?,
            Some(EntryPin::File { handle, bytes }) => {
                anyhow::ensure!(
                    super::storage::filesystem_identity(&std::fs::metadata(&self.path)?)
                        == super::storage::filesystem_identity(&handle.metadata()?)
                        && std::fs::read(&self.path)? == *bytes,
                    "Native Git route changed during a callback"
                );
            }
        }
        Ok(())
    }

    fn route(&self, prefix: &[u8], base: &Path) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let EntryPin::File { bytes, .. } = self.pin.as_ref()? else {
            return None;
        };
        let route = bytes.strip_prefix(prefix)?;
        let route = route.strip_suffix(b"\n").unwrap_or(route);
        let route = route.strip_suffix(b"\r").unwrap_or(route);
        (!route.is_empty()).then(|| base.join(std::ffi::OsStr::from_bytes(route)))
    }
}

struct CheckoutBoundary {
    checkout: PathSnapshot,
    parent: PathSnapshot,
    route: PathSnapshot,
    admin: Option<PathSnapshot>,
    common_route: Option<PathSnapshot>,
    common: Option<PathSnapshot>,
}

impl CheckoutBoundary {
    fn capture(path: &Path, git_dir: Option<&Path>) -> Result<Self> {
        let checkout = PathSnapshot::capture(path.to_path_buf())?;
        let mut parent = path.parent().context("checkout parent missing")?;
        while !parent.exists() {
            parent = parent.parent().context("checkout ancestor missing")?;
        }
        let parent = PathSnapshot::capture(parent.to_path_buf())?;
        let route = PathSnapshot::capture(path.join(".git"))?;
        let admin_path = if matches!(route.pin, Some(EntryPin::Directory(_))) {
            Some(route.path.clone())
        } else {
            route
                .route(b"gitdir: ", path)
                .or_else(|| git_dir.map(|directory| path.join(directory)))
        };
        let (admin, common_route, common) = if let Some(admin_path) = admin_path {
            let admin = if admin_path == route.path {
                None
            } else {
                Some(PathSnapshot::capture(admin_path.clone())?)
            };
            let common_route = PathSnapshot::capture(admin_path.join("commondir"))?;
            let common = common_route
                .route(b"", &admin_path)
                .map(PathSnapshot::capture)
                .transpose()?;
            (admin, Some(common_route), common)
        } else {
            (None, None, None)
        };
        Ok(Self {
            checkout,
            parent,
            route,
            admin,
            common_route,
            common,
        })
    }

    fn paths(&self) -> impl Iterator<Item = &PathSnapshot> {
        [
            Some(&self.checkout),
            Some(&self.parent),
            Some(&self.route),
            self.admin.as_ref(),
            self.common_route.as_ref(),
            self.common.as_ref(),
        ]
        .into_iter()
        .flatten()
    }

    fn verify(&self) -> Result<()> {
        for path in self.paths() {
            path.verify()?;
        }
        Ok(())
    }
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
    provisional: Option<&'a ProvisionalCreation>,
    main_repo: PathBuf,
    path: PathBuf,
    branch: String,
    detached: bool,
    original_tip: Option<git2::Oid>,
    boundary: Option<CheckoutBoundary>,
    child_boundary: Option<CheckoutBoundary>,
    partial_directories: Vec<(PathBuf, DirectoryWitness)>,
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
        provisional: Option<&'a ProvisionalCreation>,
    ) -> Result<Self> {
        let (main_repo, path, branch) = checkout;
        let root = ownership
            .as_ref()
            .context("native Git ownership lease missing")?;
        let app_dir = root.app_dir().to_path_buf();
        let (claims, profile_pins) = inventory(root, provisional)?;
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
            if let Some(provisional) = provisional {
                anyhow::ensure!(
                    profile.physical_profile_identity()? == provisional.claim.profile,
                    "Creation profile changed before native Git"
                );
            }
        }
        anyhow::ensure!(
            provisional.is_none() || profile.is_some(),
            "Provisional creation profile missing"
        );
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
            provisional,
            main_repo: main_repo.to_path_buf(),
            path: path.to_path_buf(),
            branch: branch.to_owned(),
            detached: false,
            original_tip: None,
            boundary: None,
            child_boundary: None,
            partial_directories: Vec::new(),
            checkout: None,
            children: Vec::new(),
        })
    }

    fn pin_checkout(&mut self) -> Result<()> {
        self.verify()?;
        if self.checkout.is_none() {
            let candidate = CheckoutWitness::capture_for_branch(
                &self.path,
                &self.branch,
                self.detached,
                self.original_tip,
            )?;
            self.verify()?;
            candidate.verify()?;
            self.checkout = Some(candidate);
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        for directory in &self.directories {
            directory.verify()?;
        }
        for (_, directory) in &self.partial_directories {
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
            self.provisional,
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

    fn prepare_checkout(&mut self, owned_branch: bool) -> io::Result<()> {
        let prepare = || -> Result<(bool, Option<git2::Oid>)> {
            self.verify()?;
            let repo = git2::Repository::open(&self.main_repo)?;
            let local = repo.find_branch(&self.branch, git2::BranchType::Local).ok();
            let detached = local.is_none() && repo.revparse_single(&self.branch).is_ok();
            let tip = if detached {
                Some(repo.revparse_single(&self.branch)?.peel_to_commit()?.id())
            } else if owned_branch {
                Some(
                    local
                        .context("native checkout branch is missing")?
                        .get()
                        .target()
                        .context("native checkout branch has no target")?,
                )
            } else {
                None
            };
            Ok((detached, tip))
        };
        (self.detached, self.original_tip) =
            prepare().map_err(|error| io::Error::other(format!("{error:#}")))?;
        Ok(())
    }

    fn pause(
        &mut self,
        cwd: &Path,
        checkout_ready: bool,
        git_dir: Option<&Path>,
    ) -> io::Result<()> {
        let mut prepare = || -> Result<()> {
            self.verify()?;
            let admin = git_dir.map(|directory| cwd.join(directory));
            self.boundary = Some(CheckoutBoundary::capture(&self.path, admin.as_deref())?);
            self.child_boundary = if cwd != self.path && cwd != self.main_repo {
                Some(CheckoutBoundary::capture(cwd, admin.as_deref())?)
            } else {
                None
            };
            for boundary in [&self.boundary, &self.child_boundary].into_iter().flatten() {
                for path in boundary.paths() {
                    if let Some(EntryPin::Directory(directory)) = &path.pin {
                        if !self
                            .partial_directories
                            .iter()
                            .any(|(known, _)| known == &path.path)
                        {
                            self.partial_directories
                                .push((path.path.clone(), directory.clone()));
                        }
                    }
                }
            }
            if checkout_ready {
                self.pin_checkout()?;
                if cwd != self.path
                    && cwd != self.main_repo
                    && !self.children.iter().any(|(path, _)| path == cwd)
                {
                    self.children
                        .push((cwd.to_path_buf(), CheckoutWitness::capture(cwd)?));
                }
            }
            self.verify()?;
            let root = self
                .ownership
                .as_ref()
                .context("native Git ownership lease missing")?;
            let (_, pins) = inventory(root, self.provisional)?;
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
            for boundary in [&self.boundary, &self.child_boundary].into_iter().flatten() {
                boundary.verify()?;
            }
            self.verify()?;
            if let Some(owner) = &mut self.owner {
                owner.restore(
                    self.ownership
                        .as_ref()
                        .context("native Git ownership lease missing")?,
                )?;
            }
            self.boundary = None;
            self.child_boundary = None;
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
    provisional: Option<&ProvisionalCreation>,
) -> Result<(Vec<String>, CheckoutWitness)> {
    let mut lease = NativeLease::new(ownership, false, profile, None, checkout, provisional)?;
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

#[cfg(test)]
mod tests {
    use super::CheckoutBoundary;
    use std::fs;

    #[test]
    fn partial_checkout_rejects_callback_replacement() {
        for change in ["checkout", "git-file", "route", "admin", "common", "parent"] {
            let fixture = tempfile::tempdir().unwrap();
            let target = fixture.path().join("checkout");
            let admin = fixture.path().join("admin");
            let common = fixture.path().join("common");
            fs::create_dir(&admin).unwrap();
            fs::create_dir(&common).unwrap();
            fs::write(admin.join("commondir"), "../common\n").unwrap();
            if change != "parent" {
                fs::create_dir(&target).unwrap();
            }
            if !matches!(change, "git-file" | "parent") {
                fs::write(target.join(".git"), "gitdir: ../admin\n").unwrap();
            }
            let target = if change == "parent" {
                target.join("nested")
            } else {
                target
            };
            // No HEAD or index exists: only the paused structural boundary matters.
            let boundary = CheckoutBoundary::capture(&target, None).unwrap();
            match change {
                "checkout" => {
                    fs::rename(&target, fixture.path().join("original")).unwrap();
                    fs::create_dir(&target).unwrap();
                }
                "git-file" => fs::write(target.join(".git"), "gitdir: ../admin\n").unwrap(),
                "route" => fs::write(target.join(".git"), "gitdir: ../common\n").unwrap(),
                "admin" | "common" => {
                    let path = if change == "admin" { &admin } else { &common };
                    fs::rename(path, fixture.path().join("original")).unwrap();
                    fs::create_dir(path).unwrap();
                }
                "parent" => std::os::unix::fs::symlink(&common, target.parent().unwrap()).unwrap(),
                _ => unreachable!(),
            }
            assert!(boundary.verify().is_err(), "callback replacement: {change}");
        }
    }
}
