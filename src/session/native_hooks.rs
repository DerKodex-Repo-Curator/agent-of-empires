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
    recursive: bool,
    branch: Option<(PathBuf, String)>,
}

impl Claim {
    pub(crate) fn from_instance(profile: (u64, u64), row: &super::Instance) -> Result<Self> {
        Self::for_path(
            profile,
            row,
            &row.project_path,
            row.worktree_info.is_some(),
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
        recursive: bool,
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
            recursive,
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
            let mut add =
                |path: &str, recursive: bool, branch: Option<(&str, &str)>| -> Result<()> {
                    claims.insert(Claim::for_path(physical, &row, path, recursive, branch)?);
                    Ok(())
                };
            if let Some(path) = &row.pre_trash_project_path {
                add(path, row.worktree_info.is_some(), None)?;
            }
            if let Some(workspace) = &row.workspace_info {
                add(&workspace.workspace_dir, true, None)?;
                for repo in &workspace.repos {
                    add(
                        &repo.worktree_path,
                        true,
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
        branch: Option<&str>,
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
    branch: Option<&str>,
    provisional: Option<&ProvisionalCreation>,
) -> Result<()> {
    let (claims, _pins) = inventory(root, provisional)?;
    let target =
        super::deletion::resolve_claim_path(target).context("unresolved native checkout")?;
    let main_repo = branch
        .map(|_| {
            super::deletion::resolve_claim_path(main_repo).context("unresolved native repository")
        })
        .transpose()?;
    for claim in claims.difference(baseline) {
        anyhow::ensure!(
            !(claim.path.starts_with(&target)
                || (claim.recursive && target.starts_with(&claim.path)))
                && !claim
                    .branch
                    .as_ref()
                    .is_some_and(|(repo, name)| main_repo.as_ref() == Some(repo)
                        && branch == Some(name.as_str())),
            "Checkout was adopted during a native Git callback; canceling operation"
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

pub(crate) struct PathSnapshot {
    path: PathBuf,
    resolved: PathBuf,
    pin: Option<EntryPin>,
}

impl std::fmt::Debug for PathSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PathSnapshot")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl PathSnapshot {
    pub(crate) fn capture(path: PathBuf) -> Result<Self> {
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

    pub(crate) fn verify(&self) -> Result<()> {
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
/// The caller's concrete lock order. Git effects stay fenced except while a
/// native callback runs; retained Arc holders share the same open description.
#[derive(Clone, Copy)]
pub(crate) struct BranchHandoff<'a> {
    pub(crate) root: &'a OwnershipGuard,
    pub(crate) nested: &'a [&'a storage::StorageFlock],
    pub(crate) owner: Option<(&'a str, &'a str)>,
    pub(crate) protected_path: Option<&'a Path>,
}

impl<'a> BranchHandoff<'a> {
    pub(crate) fn unpublished(root: &'a OwnershipGuard) -> Self {
        Self {
            root,
            nested: &[],
            owner: None,
            protected_path: None,
        }
    }
}

pub(crate) struct BranchOrigin {
    repo: PathBuf,
    branch: String,
    reference: String,
    routes: Vec<PathSnapshot>,
    tip: git2::Oid,
    loose: PathSnapshot,
    packed: PathSnapshot,
    git: DirectoryWitness,
    common: DirectoryWitness,
    directory: DirectoryWitness,
}

impl std::fmt::Debug for BranchOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BranchOrigin")
            .field("repo", &self.repo)
            .field("branch", &self.branch)
            .field("tip", &self.tip)
            .finish_non_exhaustive()
    }
}

impl BranchOrigin {
    pub(crate) fn repository(&self) -> &Path {
        &self.repo
    }
    pub(crate) fn branch_name(&self) -> &str {
        &self.branch
    }

    pub(crate) fn capture(repo: &Path, branch: &str) -> Result<Self> {
        let source = git2::Repository::open(repo)?;
        let reference = source.find_reference(&format!("refs/heads/{branch}"))?;
        Self::from_reference(repo, &source, branch, &reference)
    }

    fn reference_path(repo: &git2::Repository, name: &str) -> PathBuf {
        if name.starts_with("refs/") {
            repo.commondir().join(name)
        } else {
            repo.path().join(name)
        }
    }

    fn from_reference(
        path: &Path,
        repo: &git2::Repository,
        branch: &str,
        reference: &git2::Reference<'_>,
    ) -> Result<Self> {
        let name = reference
            .name()
            .context("source reference name is not UTF-8")?;
        Ok(Self {
            repo: path.to_path_buf(),
            branch: branch.to_owned(),
            reference: name.to_owned(),
            routes: Vec::new(),
            tip: reference
                .target()
                .context("source is not a direct reference")?,
            loose: PathSnapshot::capture(Self::reference_path(repo, name))?,
            packed: PathSnapshot::capture(repo.commondir().join("packed-refs"))?,
            git: DirectoryWitness::capture(repo.path())?,
            common: DirectoryWitness::capture(repo.commondir())?,
            directory: DirectoryWitness::capture(path)?,
        })
    }

    fn capture_source(
        path: &Path,
        repo: &git2::Repository,
        requested: &str,
        resolved: (git2::Object<'_>, Option<git2::Reference<'_>>),
    ) -> Result<(git2::Oid, Option<std::sync::Arc<Self>>)> {
        let (object, reference) = resolved;
        let tip = object.peel_to_commit()?.id();
        let origin = if let Some(mut reference) = reference {
            let mut routes = Vec::new();
            let mut visited = HashSet::new();
            while let Some(target) = reference.symbolic_target()? {
                let name = reference
                    .name()
                    .context("source reference name is not UTF-8")?;
                anyhow::ensure!(visited.insert(name.to_owned()), "cyclic source reference");
                routes.push(PathSnapshot::capture(Self::reference_path(repo, name))?);
                let next = repo.find_reference(target)?;
                reference = next;
            }
            let mut origin = Self::from_reference(path, repo, requested, &reference)?;
            origin.routes = routes;
            origin.verify_source()?;
            Some(std::sync::Arc::new(origin))
        } else {
            None
        };
        Ok((tip, origin))
    }

    pub(crate) fn verify_source(&self) -> Result<()> {
        let repo = git2::Repository::open(&self.repo)?;
        self.directory.verify()?;
        self.git.verify_at(repo.path())?;
        self.common.verify_at(repo.commondir())?;
        anyhow::ensure!(
            repo.find_reference(&self.reference)?.target() == Some(self.tip),
            "Detached checkout source changed during native Git"
        );
        for route in &self.routes {
            route.verify()?;
        }
        self.loose.verify()?;
        self.packed.verify()?;
        Ok(())
    }

    pub(crate) fn verify(&self, destination: Option<&str>) -> Result<()> {
        let repo = git2::Repository::open(&self.repo)?;
        self.directory.verify()?;
        self.git.verify_at(repo.path())?;
        self.common.verify_at(repo.commondir())?;
        if let Some(tip) = local_branch_tip(&repo, &self.branch)? {
            anyhow::ensure!(tip == self.tip, "Original branch changed during native Git");
            self.loose.verify()?;
            self.packed.verify()?;
        }
        if let Some(destination) = destination {
            if let Some(tip) = local_branch_tip(&repo, destination)? {
                anyhow::ensure!(tip == self.tip, "Renamed branch changed during native Git");
            }
        }
        Ok(())
    }
}

fn local_branch_tip(repo: &git2::Repository, branch: &str) -> Result<Option<git2::Oid>> {
    match repo.find_reference(&format!("refs/heads/{branch}")) {
        Ok(reference) => Ok(Some(
            reference
                .target()
                .context("branch is not a direct reference")?,
        )),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn branch_checkout_paths(origin: &BranchOrigin, destination: Option<&str>) -> Result<Vec<PathBuf>> {
    crate::git::GitWorktree::new(origin.repo.clone())?
        .list_worktrees()?
        .into_iter()
        .filter(|entry| {
            entry
                .branch
                .as_deref()
                .is_some_and(|branch| branch == origin.branch || Some(branch) == destination)
        })
        .map(|entry| {
            super::deletion::resolve_claim_path(&entry.path)
                .context("unresolved Git branch checkout")
        })
        .collect()
}

struct BranchLease<'a> {
    handoff: BranchHandoff<'a>,
    origin: &'a BranchOrigin,
    destination: Option<&'a str>,
    path: &'a Path,
    checkout: CheckoutBoundary,
    checkout_head: Option<PathBuf>,
    claims: HashSet<Claim>,
    _profiles: Vec<DirectoryWitness>,
    app: DirectoryWitness,
    protected_path: Option<CheckoutBoundary>,
    owner: Option<(Storage, Option<super::Instance>)>,
    boundary: Vec<PathSnapshot>,
    branch_checkouts: Vec<PathBuf>,
    root_paused: bool,
    nested_released: usize,
}

impl<'a> BranchLease<'a> {
    fn new(
        handoff: BranchHandoff<'a>,
        origin: &'a BranchOrigin,
        path: &'a Path,
        destination: Option<&'a str>,
    ) -> Result<Self> {
        handoff.root.require_exclusive()?;
        origin.verify(destination)?;
        let (claims, profiles) = inventory(handoff.root, None)?;
        let owner = handoff
            .owner
            .map(|(profile, id)| -> Result<_> {
                let storage = Storage::open_unwatched_with_ownership(profile, handoff.root)?;
                let row = storage
                    .load_strict_for_worktree_ownership()?
                    .into_iter()
                    .find(|row| row.id == id);
                Ok((storage, row))
            })
            .transpose()?;
        let checkout = CheckoutBoundary::capture(path, None)?;
        let checkout_head = if destination.is_some() {
            let checkout_repo = git2::Repository::open(path)?;
            origin.common.verify_at(checkout_repo.commondir())?;
            anyhow::ensure!(
                crate::git::GitWorktree::get_current_branch(path)? == origin.branch,
                "Original checkout branch no longer matches the rename"
            );
            Some(checkout_repo.path().join("HEAD"))
        } else {
            None
        };
        let app = DirectoryWitness::capture(handoff.root.app_dir())?;
        let protected_path = handoff
            .protected_path
            .map(|path| CheckoutBoundary::capture(path, None))
            .transpose()?;
        let branch_checkouts = branch_checkout_paths(origin, destination)?;
        Ok(Self {
            handoff,
            origin,
            destination,
            path,
            checkout,
            checkout_head,
            branch_checkouts,
            claims,
            _profiles: profiles,
            app,
            protected_path,
            owner,
            boundary: Vec::new(),
            root_paused: false,
            nested_released: 0,
        })
    }

    fn verify(&self) -> Result<()> {
        self.handoff.root.require_exclusive()?;
        self.origin.verify(self.destination)?;
        self.checkout.verify()?;
        self.app.verify()?;
        if let Some(path) = &self.protected_path {
            path.verify()?;
        }
        for path in branch_checkout_paths(self.origin, self.destination)? {
            anyhow::ensure!(
                self.branch_checkouts.contains(&path),
                "Branch acquired a foreign Git checkout during a native callback"
            );
        }
        if let Some((storage, original)) = &self.owner {
            storage.verify_profile_identity()?;
            let id = self.handoff.owner.context("native owner missing")?.1;
            let current = storage
                .load_strict_for_worktree_ownership()?
                .into_iter()
                .find(|row| row.id == id);
            match (original, current) {
                (Some(before), Some(after)) => anyhow::ensure!(
                    before.created_at == after.created_at
                        && before.project_path == after.project_path
                        && before.worktree_info == after.worktree_info
                        && before.workspace_info.as_ref().map(|w| (
                            &w.workspace_dir,
                            &w.branch,
                            &w.repos,
                            w.cleanup_on_delete
                        )) == after.workspace_info.as_ref().map(|w| (
                            &w.workspace_dir,
                            &w.branch,
                            &w.repos,
                            w.cleanup_on_delete
                        ))
                        && before.lifecycle_generation == after.lifecycle_generation
                        && before.lifecycle_reservation == after.lifecycle_reservation
                        && before.trashed_at == after.trashed_at
                        && before.pre_trash_project_path == after.pre_trash_project_path
                        && before.status.blocks_worktree_edit()
                            == after.status.blocks_worktree_edit(),
                    "Session authority changed during a native branch callback"
                ),
                (None, None) => {}
                _ => anyhow::bail!("Session identity changed during a native branch callback"),
            }
        }
        verify_claims(
            self.handoff.root,
            &self.claims,
            &self.origin.repo,
            self.path,
            Some(&self.origin.branch),
            None,
        )?;
        if let Some(destination) = self.destination {
            verify_claims(
                self.handoff.root,
                &self.claims,
                &self.origin.repo,
                self.path,
                Some(destination),
                None,
            )?;
        }
        if let Some(path) = self.handoff.protected_path {
            verify_claims(
                self.handoff.root,
                &self.claims,
                &self.origin.repo,
                path,
                Some(self.destination.unwrap_or(&self.origin.branch)),
                None,
            )?;
        }
        Ok(())
    }

    fn restore_locks(&mut self) -> Result<()> {
        let mut result = Ok(());
        if self.root_paused {
            result = self.handoff.root.resume_native();
            self.root_paused = false;
        }
        let first = self.handoff.nested.len() - self.nested_released;
        for lock in &self.handoff.nested[first..] {
            let restored = lock.resume_native();
            if result.is_ok() {
                result = restored;
            }
        }
        self.nested_released = 0;
        result
    }

    fn finish(&mut self) -> Result<()> {
        self.restore_locks()?;
        for path in &self.boundary {
            path.verify()?;
        }
        self.verify()
    }
}

impl crate::git::native_hooks::HookLease for BranchLease<'_> {
    fn runtime_dir(&self) -> &Path {
        self.handoff.root.app_dir()
    }
    fn prepare_checkout(&mut self, _: bool) -> io::Result<()> {
        self.checkpoint()
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        self.verify()
            .map_err(|error| io::Error::other(format!("{error:#}")))
    }
    fn pause(&mut self, _: &str, _: &Path, _: bool, _: Option<&Path>) -> io::Result<()> {
        if self.root_paused {
            self.resume(None)?;
            return self.pause_retained();
        }
        let mut pause = || -> Result<()> {
            self.verify()?;
            let repo = git2::Repository::open(&self.origin.repo)?;
            let mut paths = vec![
                repo.commondir()
                    .join(format!("refs/heads/{}", self.origin.branch)),
                repo.commondir().join("packed-refs"),
                repo.path().join("HEAD"),
            ];
            if let Some(destination) = self.destination {
                paths.push(repo.commondir().join(format!("refs/heads/{destination}")));
            }
            if let Some(head) = &self.checkout_head {
                paths.push(head.clone());
            }
            self.boundary = paths
                .into_iter()
                .map(PathSnapshot::capture)
                .collect::<Result<_>>()?;
            self.pause_retained().map_err(anyhow::Error::from)
        };
        pause().map_err(|error| io::Error::other(format!("{error:#}")))
    }
    fn resume(&mut self, _: Option<&str>) -> io::Result<()> {
        let mut resume = || -> Result<()> {
            self.restore_locks()?;
            for path in &self.boundary {
                path.verify()?;
            }
            self.verify()?;
            Ok(())
        };
        resume().map_err(|error| io::Error::other(format!("{error:#}")))
    }
    fn pause_retained(&mut self) -> io::Result<()> {
        let mut pause = || -> Result<()> {
            for lock in self.handoff.nested.iter().rev() {
                lock.pause_native()?;
                self.nested_released += 1;
            }
            self.handoff.root.pause_native()?;
            self.root_paused = true;
            Ok(())
        };
        pause().map_err(|error| io::Error::other(format!("{error:#}")))
    }
}

impl Drop for BranchLease<'_> {
    fn drop(&mut self) {
        let _ = self.restore_locks();
    }
}

pub(crate) fn rename_branch(
    handoff: BranchHandoff<'_>,
    repo: &Path,
    path: &Path,
    old: &str,
    new: &str,
) -> Result<()> {
    let origin = BranchOrigin::capture(repo, old)?;
    let mut lease = BranchLease::new(handoff, &origin, path, Some(new))?;
    let git = crate::git::GitWorktree::new(repo.to_path_buf())?;
    let result = git.rename_branch_with_hooks(old, new, Some(&mut lease));
    lease.finish()?;
    result?;
    let repo = git2::Repository::open(repo)?;
    anyhow::ensure!(
        local_branch_tip(&repo, old)?.is_none()
            && local_branch_tip(&repo, new)? == Some(origin.tip)
            && crate::git::GitWorktree::get_current_branch(path)? == new,
        "Native branch rename did not complete against its original reference"
    );
    Ok(())
}

pub(crate) fn delete_branch(
    handoff: BranchHandoff<'_>,
    origin: &BranchOrigin,
    removed_path: &Path,
) -> Result<()> {
    anyhow::ensure!(
        !removed_path.try_exists()?,
        "Deleted checkout was replaced before branch cleanup"
    );
    let mut lease = BranchLease::new(handoff, origin, removed_path, None)?;
    let git = crate::git::GitWorktree::new(origin.repo.clone())?;
    let result = git.delete_branch_with_hooks(&origin.branch, Some(&mut lease));
    lease.finish()?;
    result?;
    anyhow::ensure!(
        local_branch_tip(&git2::Repository::open(&origin.repo)?, &origin.branch)?.is_none(),
        "Native branch deletion did not complete"
    );
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
    provisional: Option<&'a ProvisionalCreation>,
    main_repo: PathBuf,
    path: PathBuf,
    branch: String,
    detached: bool,
    original_tip: Option<git2::Oid>,
    detached_source: Option<std::sync::Arc<BranchOrigin>>,
    owned_branch_origin: Option<std::sync::Arc<BranchOrigin>>,
    boundary: Option<CheckoutBoundary>,
    child_boundaries: Vec<(String, CheckoutBoundary)>,
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
            detached_source: None,
            owned_branch_origin: None,
            boundary: None,
            child_boundaries: Vec::new(),
            partial_directories: Vec::new(),
            checkout: None,
            children: Vec::new(),
        })
    }

    fn pin_checkout(&mut self) -> Result<()> {
        self.verify()?;
        if self.checkout.is_none() {
            let mut candidate = CheckoutWitness::capture_for_branch(
                &self.path,
                &self.branch,
                self.detached,
                self.original_tip,
                self.detached_source.clone(),
            )?;
            candidate.record_created_branch(self.owned_branch_origin.clone());
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
        if let Some(source) = &self.detached_source {
            source.verify_source()?;
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
            (!self.detached).then_some(self.branch.as_str()),
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
        self.checkout
            .as_mut()
            .context("native checkout witness missing")?
            .finalize()?;
        Ok((
            warnings,
            self.checkout
                .clone()
                .context("native checkout witness missing")?,
        ))
    }

    pub(crate) fn take_branch_origin(&mut self) -> Option<std::sync::Arc<BranchOrigin>> {
        self.owned_branch_origin.take()
    }

    pub(crate) fn receipt(&self, create_branch: bool) -> Option<CreatedWorktree> {
        self.checkout.clone().map(|witness| CreatedWorktree {
            path: self.path.clone(),
            main_repo_path: self.main_repo.clone(),
            owned_branch: (create_branch && !witness.is_detached()).then(|| self.branch.clone()),
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
        let prepare = || -> Result<(bool, Option<git2::Oid>, Option<std::sync::Arc<BranchOrigin>>)> {
            self.verify()?;
            let repo = git2::Repository::open(&self.main_repo)?;
            let local = repo.find_branch(&self.branch, git2::BranchType::Local).ok();
            if local.is_none() {
                let resolution = repo.revparse_ext(&self.branch);
                match resolution {
                    Ok(resolved) => {
                        let (tip, source) =
                            BranchOrigin::capture_source(&self.main_repo, &repo, &self.branch, resolved)?;
                        Ok((true, Some(tip), source))
                    }
                    Err(error) if !owned_branch && error.code() == git2::ErrorCode::NotFound => {
                        // Existing admission permits remote suffixes that Git DWIMs to a local branch.
                        Ok((false, None, None))
                    }
                    Err(error) => Err(error.into()),
                }
            } else {
                let tip = if owned_branch {
                    Some(local.context("native checkout branch is missing")?.get().target()
                        .context("native checkout branch has no target")?)
                } else { None };
                Ok((false, tip, None))
            }
        };
        (self.detached, self.original_tip, self.detached_source) =
            prepare().map_err(|error| io::Error::other(format!("{error:#}")))?;
        self.owned_branch_origin = owned_branch
            .then(|| BranchOrigin::capture(&self.main_repo, &self.branch).map(std::sync::Arc::new))
            .transpose()
            .map_err(|error| io::Error::other(format!("{error:#}")))?;
        Ok(())
    }

    fn pause(
        &mut self,
        callback: &str,
        cwd: &Path,
        checkout_ready: bool,
        git_dir: Option<&Path>,
    ) -> io::Result<()> {
        let overlapping = self.ownership.is_none();
        if overlapping {
            self.resume(None)?;
        }
        let mut prepare = || -> Result<()> {
            self.verify()?;
            let admin = git_dir.map(|directory| cwd.join(directory));
            if !overlapping {
                self.boundary = Some(CheckoutBoundary::capture(&self.path, admin.as_deref())?);
                self.child_boundaries.clear();
            }
            if cwd != self.path && cwd != self.main_repo {
                self.child_boundaries.push((
                    callback.to_owned(),
                    CheckoutBoundary::capture(cwd, admin.as_deref())?,
                ));
            }
            for boundary in self
                .boundary
                .iter()
                .chain(self.child_boundaries.iter().map(|(_, boundary)| boundary))
            {
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
            self.pause_retained().map_err(anyhow::Error::from)
        };
        prepare().map_err(|error| io::Error::other(format!("{error:#}")))
    }

    fn resume(&mut self, completed: Option<&str>) -> io::Result<()> {
        let mut resume = || -> Result<()> {
            *self.ownership = Some(if self.exclusive {
                storage::acquire_ownership_lock()?
            } else {
                storage::acquire_ownership_read()?
            });
            for boundary in self
                .boundary
                .iter()
                .chain(self.child_boundaries.iter().map(|(_, boundary)| boundary))
            {
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
            if let Some(completed) = completed {
                self.child_boundaries
                    .retain(|(callback, _)| callback != completed);
            }
            Ok(())
        };
        resume().map_err(|error| io::Error::other(format!("{error:#}")))
    }
    fn pause_retained(&mut self) -> io::Result<()> {
        let mut release = || -> Result<()> {
            let root = self
                .ownership
                .as_ref()
                .context("native Git ownership lease missing")?;
            if let Some(owner) = &mut self.owner {
                owner.release(root)?;
            }
            drop(self.ownership.take());
            Ok(())
        };
        release().map_err(|error| io::Error::other(format!("{error:#}")))
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
    let branch = if result.is_err() && receipt.is_none() {
        lease.take_branch_origin()
    } else {
        None
    };
    drop(lease);
    if result.is_err() {
        drop(ownership.take());
        cleanup(receipt.as_ref(), branch.as_deref(), checkout.1);
    }
    result
}

pub(crate) fn cleanup(
    receipt: Option<&CreatedWorktree>,
    branch: Option<&BranchOrigin>,
    path: &Path,
) {
    if receipt.is_none() && branch.is_none() {
        return;
    }
    let Ok(root) = storage::acquire_ownership_lock() else {
        return;
    };
    if let Some(receipt) = receipt {
        cleanup_with_ownership(receipt, BranchHandoff::unpublished(&root), None);
    } else if let Some(branch) = branch {
        if let Err(error) =
            cleanup_branch_with_ownership(branch, path, BranchHandoff::unpublished(&root), None)
        {
            tracing::warn!(target: "session.create", "Preserving failed-creation branch: {error:#}");
        }
    }
}

pub(crate) fn cleanup_branch_with_ownership(
    origin: &BranchOrigin,
    path: &Path,
    handoff: BranchHandoff<'_>,
    workspace_owner: Option<(&Storage, &super::Instance)>,
) -> Result<()> {
    handoff.root.require_exclusive()?;
    origin.verify(None)?;
    anyhow::ensure!(
        !path.try_exists()?,
        "unwitnessed checkout exists; branch cleanup authority is uncertain"
    );
    anyhow::ensure!(
        branch_checkout_paths(origin, None)?.is_empty(),
        "created branch acquired a Git checkout"
    );
    let owners = super::builder::cleanup_owners(handoff.root)?;
    let protection = CleanupProtection {
        owner: None,
        owners,
        workspace_owner,
    };
    anyhow::ensure!(
        !protection.references_path(path)
            && !protection.references_branch(&origin.repo, &origin.branch),
        "created branch or checkout was adopted"
    );
    if local_branch_tip(&git2::Repository::open(&origin.repo)?, &origin.branch)?.is_none() {
        return Ok(());
    }
    delete_branch(handoff, origin, path)
}

pub(crate) fn cleanup_with_ownership(
    receipt: &CreatedWorktree,
    handoff: BranchHandoff<'_>,
    workspace_owner: Option<(&Storage, &super::Instance)>,
) -> bool {
    super::builder::cleanup_created_worktree(
        receipt,
        "native Git creation",
        &CleanupProtection {
            workspace_owner,
            ..CleanupProtection::default()
        },
        handoff,
    )
}

#[cfg(test)]
mod tests {
    use super::CheckoutBoundary;
    use std::fs;

    #[test]
    #[serial_test::serial]
    fn branch_handoff_restores_original_authority_before_continuation() {
        use super::{BranchHandoff, BranchLease, BranchOrigin};
        use crate::git::native_hooks::HookLease;
        use crate::session::storage;
        use crate::session::{Instance, Storage, WorktreeInfo};
        for action in [
            "read",
            "peer-rename",
            "native-progress",
            "delete-read",
            "delete-adopt",
            "adopt-source",
            "adopt-target",
            "adopt-branch",
            "foreign-checkout",
            "delete-foreign-checkout",
            "replace-ref",
            "replace-packed",
            "replace-destination",
            "switch-head",
            "owner-lifecycle",
            "owner-profile",
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(fixture.path());
            let main = fixture.path().join("main");
            let repo = git2::Repository::init(&main).unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
            let oid = repo
                .commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
            let checkout = fixture.path().join("original");
            let target = fixture.path().join("renamed");
            let git = crate::git::GitWorktree::new(main.clone()).unwrap();
            git.create_worktree("original", &checkout, true, Some("main"))
                .unwrap();
            let owner = Storage::new_unwatched("owner").unwrap();
            let peer = Storage::new_unwatched("peer").unwrap();
            let mut row = Instance::new("row", checkout.to_str().unwrap());
            row.worktree_info = Some(WorktreeInfo {
                branch: "original".into(),
                main_repo_path: main.to_str().unwrap().into(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            let id = row.id.clone();
            owner
                .update(|rows, _| {
                    rows.push(row);
                    Ok(())
                })
                .unwrap();
            let root = storage::acquire_ownership_lock().unwrap();
            let identity = storage::acquire_session_identity_lock_with_ownership(&root).unwrap();
            let title = storage::acquire_session_title_lock_with_ownership(&root, &id).unwrap();
            let lifecycle = owner
                .acquire_instance_lifecycle_lock_with_ownership(&root, &id)
                .unwrap();
            if action == "replace-packed" {
                assert!(std::process::Command::new("git")
                    .args(["pack-refs", "--all"])
                    .current_dir(&main)
                    .status()
                    .unwrap()
                    .success());
            }
            let origin = BranchOrigin::capture(&main, "original").unwrap();
            let deleting = action.starts_with("delete-");
            if deleting {
                git.remove_worktree(&checkout, false).unwrap();
            }
            let nested = [&identity, &title, &lifecycle];
            let mut lease = BranchLease::new(
                BranchHandoff {
                    root: &root,
                    nested: &nested,
                    owner: Some(("owner", &id)),
                    protected_path: (!deleting).then_some(target.as_path()),
                },
                &origin,
                &checkout,
                (!deleting).then_some("renamed"),
            )
            .unwrap();
            if action == "native-progress" {
                git.rename_branch("original", "renamed").unwrap();
            }
            lease.pause("callback", &main, false, None).unwrap();
            assert!(
                root.require_exclusive().is_err(),
                "suspended root must reject authority: {action}"
            );
            let callback_root =
                storage::try_acquire_storage_flock(root.app_dir(), ".workspace-claim.lock")
                    .unwrap()
                    .expect("root was not released");
            let callback_identity =
                storage::try_acquire_storage_flock(root.app_dir(), ".title-mutation.lock")
                    .unwrap()
                    .expect("identity was not released");
            let callback_title = storage::try_acquire_storage_flock(
                root.app_dir(),
                &format!(".session-title-{id}.lock"),
            )
            .unwrap()
            .expect("title was not released");
            let callback_lifecycle = storage::try_acquire_storage_flock(
                owner.sessions_path().parent().unwrap(),
                &format!(".instance-lifecycle-{id}.lock"),
            )
            .unwrap()
            .expect("lifecycle was not released");
            drop((
                callback_lifecycle,
                callback_title,
                callback_identity,
                callback_root,
            ));
            // The callback uses its own root, not the invalid suspended authority.
            match action {
                "read" | "native-progress" | "delete-read" => {
                    assert!(Storage::open_unwatched("peer")
                        .unwrap()
                        .load()
                        .unwrap()
                        .is_empty());
                }
                "peer-rename" => crate::session::rename_profile("peer", "peer-renamed").unwrap(),
                "adopt-source" | "adopt-target" | "adopt-branch" | "delete-adopt" => {
                    let path = if action == "adopt-target" {
                        &target
                    } else if action == "adopt-branch" {
                        fixture.path()
                    } else {
                        &checkout
                    };
                    let mut adopter = Instance::new("adopter", path.to_str().unwrap());
                    if action == "adopt-branch" {
                        adopter.worktree_info = Some(WorktreeInfo {
                            branch: "original".into(),
                            main_repo_path: main.to_str().unwrap().into(),
                            managed_by_aoe: false,
                            created_at: chrono::Utc::now(),
                            base_branch: None,
                        });
                    }
                    peer.update(|rows, _| {
                        rows.push(adopter);
                        Ok(())
                    })
                    .unwrap();
                }
                "replace-ref" => {
                    let path = repo.path().join("refs/heads/original");
                    let bytes = fs::read(&path).unwrap();
                    fs::rename(&path, fixture.path().join("pinned-original-ref")).unwrap();
                    fs::write(&path, bytes).unwrap();
                }
                "replace-packed" => {
                    let path = repo.path().join("packed-refs");
                    let bytes = fs::read(&path).unwrap();
                    fs::rename(&path, fixture.path().join("pinned-packed-refs")).unwrap();
                    fs::write(&path, bytes).unwrap();
                }
                "replace-destination" => {
                    repo.reference("refs/heads/renamed", oid, false, "callback")
                        .unwrap();
                }
                "foreign-checkout" | "delete-foreign-checkout" => {
                    let other = fixture.path().join("foreign-checkout");
                    assert!(std::process::Command::new("git")
                        .args(["worktree", "add", "--force"])
                        .arg(&other)
                        .arg("original")
                        .current_dir(&main)
                        .status()
                        .unwrap()
                        .success());
                }
                "switch-head" => {
                    repo.reference("refs/heads/callback-other", oid, false, "callback")
                        .unwrap();
                    git2::Repository::open(&checkout)
                        .unwrap()
                        .set_head("refs/heads/callback-other")
                        .unwrap();
                }
                "owner-lifecycle" => {
                    owner
                        .update(|rows, _| {
                            rows[0].lifecycle_generation += 1;
                            Ok(())
                        })
                        .unwrap();
                }
                "owner-profile" => {
                    let profile = owner.sessions_path().parent().unwrap();
                    fs::rename(profile, fixture.path().join("pinned-owner-profile")).unwrap();
                    fs::create_dir(profile).unwrap();
                    fs::write(profile.join("sessions.json"), "[]").unwrap();
                }
                _ => unreachable!(),
            }
            let accepted = lease.resume(Some("callback")).is_ok();
            assert_eq!(
                accepted,
                matches!(
                    action,
                    "read" | "peer-rename" | "native-progress" | "delete-read"
                ),
                "callback authority: {action}"
            );
            assert!(
                root.require_exclusive().is_ok(),
                "root must be restored even after rejection"
            );
            assert!(
                storage::try_acquire_storage_flock(root.app_dir(), ".workspace-claim.lock")
                    .unwrap()
                    .is_none(),
                "root fence escaped callback"
            );
            if action == "replace-ref" {
                assert_eq!(
                    repo.find_reference("refs/heads/original").unwrap().target(),
                    Some(oid)
                );
            }
            if action == "replace-destination" {
                assert_eq!(
                    repo.find_reference("refs/heads/renamed").unwrap().target(),
                    Some(oid)
                );
            }
            if action.starts_with("adopt-") || action == "delete-adopt" {
                assert_eq!(peer.load().unwrap().len(), 1);
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn branch_handoff_restores_partial_nested_release() {
        use super::{BranchHandoff, BranchLease, BranchOrigin};
        use crate::git::native_hooks::HookLease;
        use crate::session::storage;
        let fixture = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(fixture.path());
        let main = fixture.path().join("main");
        let repo = git2::Repository::init(&main).unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let checkout = fixture.path().join("original");
        crate::git::GitWorktree::new(main.clone())
            .unwrap()
            .create_worktree("original", &checkout, true, Some("main"))
            .unwrap();
        let root = storage::acquire_ownership_lock().unwrap();
        let first = storage::acquire_storage_flock(root.app_dir(), "first.lock").unwrap();
        let second = storage::acquire_storage_flock(root.app_dir(), "second.lock").unwrap();
        let origin = BranchOrigin::capture(&main, "original").unwrap();
        let nested = [&first, &second];
        let mut lease = BranchLease::new(
            BranchHandoff {
                root: &root,
                nested: &nested,
                owner: None,
                protected_path: None,
            },
            &origin,
            &checkout,
            Some("renamed"),
        )
        .unwrap();
        fs::rename(
            root.app_dir().join("first.lock"),
            root.app_dir().join("original-first.lock"),
        )
        .unwrap();
        fs::write(root.app_dir().join("first.lock"), "").unwrap();
        assert!(lease.pause("callback", &main, false, None).is_err());
        assert_eq!(lease.nested_released, 1);
        drop(lease);
        assert!(root.require_exclusive().is_ok());
        assert!(
            storage::try_acquire_storage_flock(root.app_dir(), "second.lock")
                .unwrap()
                .is_none(),
            "released lifecycle remained unlocked after partial pause"
        );
    }

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

    #[test]
    #[serial_test::serial]
    fn branch_only_creation_receipts_preserve_replacements_adoptions_and_user_branches() {
        use super::{cleanup_branch_with_ownership, BranchHandoff, NativeLease};
        use crate::git::native_hooks::HookLease;
        for case in [
            "owned",
            "workspace-owner",
            "preexisting",
            "replaced-ref",
            "checkout",
            "adopted",
            "unreadable",
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let _home =
                crate::session::test_support::isolate_app_dir_at(&fixture.path().join("home"));
            let storage = crate::session::Storage::new_unwatched("owner").unwrap();
            let peer = crate::session::Storage::new_unwatched("peer").unwrap();
            let main = fixture.path().join("repo");
            let repo = git2::Repository::init(&main).unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let signature = git2::Signature::now("fixture", "fixture@example.com").unwrap();
            let tip = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    "fixture",
                    &repo.find_tree(tree_id).unwrap(),
                    &[],
                )
                .unwrap();
            let path = fixture.path().join("checkout");
            let mut row =
                crate::session::Instance::new("workspace owner", fixture.path().to_str().unwrap());
            row.source_profile = "owner".into();
            if case == "workspace-owner" {
                row.workspace_info = Some(crate::session::WorkspaceInfo {
                    branch: "main".into(),
                    workspace_dir: row.project_path.clone(),
                    repos: Vec::new(),
                    created_at: chrono::Utc::now(),
                    cleanup_on_delete: true,
                });
                storage
                    .update(|rows, _| {
                        rows.push(row.clone());
                        Ok(())
                    })
                    .unwrap();
            }
            if case == "preexisting" {
                repo.branch("topic", &repo.find_commit(tip).unwrap(), false)
                    .unwrap();
            }
            let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
            let mut lease = NativeLease::new(
                &mut ownership,
                false,
                Some(&storage),
                None,
                (&main, &path, "topic"),
                None,
            )
            .unwrap();
            if case != "preexisting" {
                repo.branch("topic", &repo.find_commit(tip).unwrap(), false)
                    .unwrap();
            }
            lease.prepare_checkout(case != "preexisting").unwrap();
            assert!(lease.receipt(case != "preexisting").is_none());
            let origin = lease.take_branch_origin();
            drop(lease);
            if case == "replaced-ref" {
                let reference = repo.path().join("refs/heads/topic");
                let replacement = fixture.path().join("replacement-ref");
                std::fs::write(&replacement, std::fs::read(&reference).unwrap()).unwrap();
                std::fs::rename(replacement, reference).unwrap();
            } else if case == "checkout" {
                crate::git::GitWorktree::new(main.clone())
                    .unwrap()
                    .with_init_submodules(false)
                    .create_worktree("topic", &fixture.path().join("foreign"), false, None)
                    .unwrap();
            } else if case == "adopted" {
                let mut adopter = crate::session::Instance::new("adopter", path.to_str().unwrap());
                adopter.worktree_info = Some(crate::session::WorktreeInfo {
                    branch: "topic".into(),
                    main_repo_path: main.to_string_lossy().into_owned(),
                    managed_by_aoe: true,
                    created_at: chrono::Utc::now(),
                    base_branch: None,
                });
                peer.update(|rows, _| {
                    rows.push(adopter);
                    Ok(())
                })
                .unwrap();
            } else if case == "unreadable" {
                std::fs::write(peer.sessions_path(), "invalid rows").unwrap();
            }
            drop(ownership.take());
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            let removable = matches!(case, "owned" | "workspace-owner");
            if let Some(origin) = origin {
                let owner = (case == "workspace-owner").then_some((&storage, &row));
                let result = cleanup_branch_with_ownership(
                    &origin,
                    &path,
                    BranchHandoff::unpublished(&root),
                    owner,
                );
                assert_eq!(result.is_ok(), removable, "{case}: {result:?}");
            } else {
                assert_eq!(case, "preexisting");
            }
            assert_eq!(
                repo.find_branch("topic", git2::BranchType::Local).is_ok(),
                !removable,
                "{case}: wrong branch cleanup"
            );
            assert!(!path.exists());
            if case == "unreadable" {
                assert_eq!(
                    std::fs::read(peer.sessions_path()).unwrap(),
                    b"invalid rows"
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn detached_native_claims_ignore_local_label_aliases_but_refuse_path_adoption() {
        use crate::git::native_hooks::HookLease;
        use crate::session::{Instance, Storage, WorktreeInfo};
        let fixture = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(&fixture.path().join("home"));
        let peer = Storage::new_unwatched("peer").unwrap();
        let main = fixture.path().join("repo");
        let repo = git2::Repository::init(&main).unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let signature = git2::Signature::now("fixture", "fixture@example.com").unwrap();
        let tip = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "fixture",
                &repo.find_tree(tree_id).unwrap(),
                &[],
            )
            .unwrap();
        repo.reference("refs/remotes/origin/main", tip, true, "fixture")
            .unwrap();
        let target = fixture.path().join("detached-target");
        let peer_path = fixture.path().join("attached-peer");
        let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
        let mut lease = super::NativeLease::new(
            &mut ownership,
            false,
            None,
            None,
            (&main, &target, "origin/main"),
            None,
        )
        .unwrap();
        lease.prepare_checkout(false).unwrap();
        drop(lease.ownership.take());
        repo.reference(
            "refs/heads/origin/main",
            tip,
            true,
            "independent local branch",
        )
        .unwrap();
        crate::git::GitWorktree::new(main.clone())
            .unwrap()
            .with_init_submodules(false)
            .create_worktree("origin/main", &peer_path, false, None)
            .unwrap();
        let mut adopted = Instance::new("peer", &peer_path.to_string_lossy());
        adopted.worktree_info = Some(WorktreeInfo {
            branch: "origin/main".into(),
            main_repo_path: main.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        peer.update(|rows, _| {
            rows.push(adopted);
            Ok(())
        })
        .unwrap();
        *lease.ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
        lease.verify().unwrap();
        drop(lease.ownership.take());
        peer.update(|rows, _| {
            rows[0].project_path = target.to_string_lossy().into_owned();
            Ok(())
        })
        .unwrap();
        *lease.ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
        assert!(
            lease.verify().is_err(),
            "detached mode must retain target-path adoption checks"
        );
        assert_eq!(
            repo.find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            Some(tip)
        );
        assert!(peer_path.exists());
        assert!(!target.exists());
    }
}
