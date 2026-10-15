# Worktrees Reference

A worktree session gets a separate checkout, created when the session is created and cleaned up when it is deleted.

## Creating one

```bash
aoe add . -w feat/my-feature -b                          # new branch off the repo default
aoe add . -w hotfix-1 -b --base-branch release-1.2       # new branch off a specific base
aoe add . -w feat/my-feature                             # attach: re-use or check out that branch
aoe worktree list                                        # every worktree
aoe worktree info <session>
aoe worktree cleanup                                     # find orphaned worktrees
aoe remove <session> [--delete-worktree]
```

`-b` is what switches between creating a branch and attaching to an existing one. `--base-branch` only matters with `-b`, and is resolved against the remotes first, then a local branch, so a teammate's not-yet-fetched branch works without a manual `git fetch`. Remote selection scores every configured remote, not just `origin`: in a fork plus `upstream` layout where `upstream/main` is ahead, aoe fetches and branches from there even when you typed `main`. Ties break toward `origin`. Without `--base-branch`, the branch starts from the repo's default.

Without `-b`, selecting an explicit remote-tracking ref such as `origin/main` can leave the checkout detached instead of creating a local branch. The session keeps the requested ref label; commits in that checkout do not advance the remote-tracking ref. An unqualified remote branch name can instead create and attach a local tracking branch, following Git's branch guessing.

In the TUI, enable the Worktree checkbox in the new-session dialog (`n`); the directory is derived from the session title. `Ctrl+P` on the Worktree field sets an explicit name, attaches to an existing branch, picks a base branch, or configures extra repos, and `Ctrl+P` on the Base field opens a branch picker over local and remote-tracking branches. The web wizard's **Worktree** row opens the same controls, with a base-branch typeahead and an **Attach to existing branch** toggle.

## Naming

A worktree session's title and its directory stay tied by default (`session.tie_workdir_to_name`), which applies only to aoe-managed worktree sessions:

- Renaming a session (TUI, web, `aoe session rename`, or `PATCH /api/sessions/{id}`) moves the directory to the title's path-safe slug before committing the title. A failed move leaves the title unchanged.
- The git branch is never renamed by default, since it may carry an upstream or an open PR. Opt in with the TUI rename dialog's "Also rename git branch", `--rename-branch`, or `rename_branch: true`. The TUI warns when the branch tracks a remote, because the remote branch and any open PR do not follow.
- A rename that would relocate the checkout or re-point its branch needs a stopped session and is refused while it runs. A title-only rename whose slug leaves the directory unchanged is allowed on a running session from the CLI and REST (the TUI still asks for a stopped session), and leaves a live structured-view worker alone.

`aoe add -w <branch>` names the session and directory after the branch. Pass `--generated-name` to get a generated name instead (as the TUI and web do without a branch): the title is a civilization name unused by any session, and it fills `{branch}` in the worktree path template, choosing a name whose resulting path is free. The git branch stays exactly as given. A template without `{branch}`, such as `../wt/{session-id}`, never contains the name.

```bash
aoe add . -w feat/my-feature -b --generated-name         # branch feat/my-feature, dir ../<repo>-worktrees/<generated>
```

It conflicts with `--title`. When attaching to a branch that already has a worktree, that directory is kept and only the title is generated. It is refused with `--repo`/`--project`, whose workspace directory comes from `workspace_path_template`. With `session.tie_workdir_to_name` off, the new directory still uses the generated name.

Turn the setting off to relabel sessions freely while they run and to edit the directory name independently:

| Surface | How |
|---------|-----|
| CLI | `aoe session set-worktree-name <session> --name <new-name> [--rename-branch]` |
| TUI | Select the session and press `W`, or use the command palette |
| Web | Right-click the row, "Edit workdir name" |
| REST | `PATCH /api/sessions/{id}/worktree-name` with `{ "name", "rename_branch" }` |

Renaming moves the checkout with `git worktree move`, keeping its parent directory and swapping only the final component. It works only on aoe-managed worktrees of a stopped session. Source and destination claims in other profiles prevent the move, including absent paths reserved for restoring trashed sessions.

### When the directory moves outside aoe

aoe records a worktree's directory at creation, so relocating it from another shell leaves that record stale. aoe repairs it from `git worktree list`, matching on the session's branch, shortly after TUI startup, on a background sweep, at `aoe serve` startup, and on each CLI workdir edit. If exactly one live worktree checks out the branch, the path is rewritten; if two do, aoe leaves it alone rather than guessing. Reconciliation is point-in-time, so a directory moved while a process is already running stays stale until it restarts.

Detached checkouts have no branch identity for this repair. Move them through aoe rather than relying on branch-based reconciliation after an external move.

Two caveats: aoe locks the worktrees it creates, so an out-of-band `git worktree move` needs `git worktree unlock <path>` first, and a plain `mv` is not recoverable on its own, because git's record still names the old path. For a checkout on a branch, run `git worktree repair <new-path>` so aoe can find it.

## Configuration

```toml
[worktree]
enabled = false
path_template = "../{repo-name}-worktrees/{branch}"
bare_repo_path_template = "./{branch}"
auto_cleanup = true
delete_branch_on_cleanup = false
init_submodules = true
```

Template variables are `{repo-name}` (repository folder name), `{branch}` (slashes become hyphens), and `{session-id}` (the first 8 characters of the session UUID), so `../wt/{branch}-{session-id}` or `./worktrees/{branch}` both work. Use `[session] row_tag = "branch"` to show branch tags in the TUI list.

### Bare repos

Bare repos are auto-detected and use `bare_repo_path_template` instead, so worktrees land as siblings inside the project directory. A sandboxed session needs this layout to reach the repo's git directory from the container.

### Submodules

AoE initializes submodules recursively after creating a worktree. Set `init_submodules = false` (or pass `--no-submodules`) to skip this for repositories with large submodule trees. Submodule initialization failures abort creation.

## Cleanup

Deleting a session prompts to remove an aoe-managed worktree (or pass `--delete-worktree`); a manual worktree or a non-worktree session is left alone.

**Trashing relocates the worktree** into a sibling `.aoe-trash/<session-id>` holding directory with `git worktree move`, so trashed sessions stop cluttering the active checkouts while staying previewable. Restoring moves it back, and is refused if that path is now occupied. Purging removes it.

TUI restores, including Restore All, run in the background and leave restored sessions stopped. Restore reserves the exact session before checking or discarding its old sandbox container, without holding application or session locks during Docker. It then checks the original profile, session generation, checkout identity and ownership again before moving the checkout back; a competing change can therefore refuse the restore without overwriting that change.

Background trash pointer repair only corrects stored metadata. It pins the matching directory and session generation and checks other profiles' path claims, but does not require Git metadata or move the checkout.

**The default branch's checkout is never removed.** In a bare-repo layout the default branch lives in a linked worktree other tooling expects to stay put, so aoe refuses to remove that checkout or delete its branch, reports the refusal, and deletes the session anyway. Force does not bypass this, including trash auto-purge and `aoe session empty-trash`, and `aoe worktree cleanup` lists such a checkout as skipped. Detection uses what git states: the bare repo's own `HEAD` plus every remote's `refs/remotes/<remote>/HEAD`, falling back to `main` and `master` by convention when neither exists. To remove one anyway, do it with git and then delete the session.

An externally placed `git worktree lock` is not a deletion guard: aoe locks every worktree it creates and unlocks before each intentional remove or move, so it unlocks yours too.

## Hooks that call AoE

Native Git hooks may call AoE commands. If a hook changes the session or claims a checkout needed by the operation, the operation can be cancelled without undoing the hook's changes.

Renames record their original session generation, physical profile and Git resources in the application directory's `.move-journal` before changing a branch or checkout. CLI session commands, daemon startup and TUI loads recover interrupted intents before using session snapshots. A live transaction keeps its own flock through callbacks, so a hook's nested AoE command cannot recover or consume its caller's intent.

Recovery finishes a verified publication or compensates only resources still proven original and unadopted. This includes recreating an absent original branch from its recorded object when Git stopped between deleting the old name and creating the new one. Replaced resources, newer session incarnations or generations, and competing owners are preserved instead; the intent remains an explicit conflict, shown in the TUI or reported by the CLI and daemon. This is not a promise of atomic commits across Git and JSON. Legacy v1 intents are backed up and migrated to non-authoritative conflict records; they never select or delete a session copy automatically.

Worktree creation waits for long-running hooks, LFS filters and recursive submodule initialization to finish.

Overlapping hook callbacks are independent while their owning Git command is still alive; each must pass the original ownership and checkout checks before Git continues. A callback's temporary boundary expires only after that callback has finished and been verified, so unrelated callbacks do not freeze later native progress. Reference recreation retains its committed file receipt before user hooks run; a later error cannot authorize adopting a replacement merely because it points to the same commit. Ordinary AoE commands launched from a hook stop inheriting that command's native routing and liveness reference.

Native runtimes use the reserved `native-git-runtimes-v2` application namespace and a private `aoe-native-git-v2-<uid>` socket namespace. Allocation and recovery share locks, and record the socket route before creating it, so interrupted initialization can be cleaned up without deleting unrelated neighbouring directories. Active routed Git processes, including detached Git children that retain their liveness descriptor, keep their runtime protected until the last reference closes. A short platform temporary directory is used when the socket path fits; otherwise aoe uses a short fallback. Helper failures go to stderr without changing Git's stdout.

While another worktree operation is finishing, TUI favorite, manual unread and snooze changes may show a retry message without changing memory or disk. Navigation and rendering remain available. Automatic read marking retries without blocking input.

## Warnings during create

Two kinds of non-fatal failure surface through the same channel instead of aborting the session: a `⚠` line on stderr for `aoe add`, a **Worktree warnings** dialog in the TUI, and a toast plus `warnings: string[]` on the `POST /api/sessions` response for the web.

**Post-checkout hooks.** Some repos install pre-commit hooks at the `post-checkout` stage (`uv-sync`, `npm install`, LFS smudge) that fire when `git worktree add` checks out the branch. If one fails, the worktree and its `.git` pointer already exist and are usable. Usually the hook needs network access or credentials the new worktree does not have yet: re-run it by hand once the environment is set up, or set `core.hooksPath` per checkout.

Failures during submodule checkout remain fatal to creation; they are not converted into the top-level worktree warning.

**Fetch failures.** aoe runs `git fetch <remote> <branch>` before checking out, and network errors, missing remotes, SSH key problems, and 10s timeouts surface as `git fetch <remote> <branch> failed for <repo>: <stderr>`. The session is still created, branching off whatever local ref exists, which may be stale. A multi-repo session emits one warning per failing repo.
