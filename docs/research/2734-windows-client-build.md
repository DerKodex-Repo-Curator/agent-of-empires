# Windows client-only build: what blocks `aoe` on `x86_64-pc-windows-*`

Research for #4397 (part of #2734), input to the crate-shape decision #4398.
Inventory taken against `main` at `43dfdd13e` on 2026-10-13. Facts only; no
recommendation.

## Summary

- Every one of the 418 dependency crates checks for `x86_64-pc-windows-gnu`.
  The only crate that fails is `agent-of-empires` itself: 221 errors, all in
  the first rustc phase (name resolution and method lookup), so the count is a
  floor, not a total.
- 166 of 221 errors are in `session` (117) and `migrations` (49). The CLI
  read path proper (`src/cli/`) has 15, and 14 of those are in `cli/serve.rs`.
- Six -sys crates compile C on the Windows target and need a C toolchain
  there (`libgit2-sys`, `libz-sys`, `libsqlite3-sys`, `lzma-sys`, `zstd-sys`,
  `aws-lc-sys`, the last also needing NASM). `openssl-sys` is not in the
  Windows graph at all.
- `nix` is the only unconditional direct dependency that is unix-only.
  `portable-pty` is declared but has zero use sites in `src/`, `tests/` or
  `build.rs`; it is what pulls the second copy of `nix` (0.28) and `winapi`.
- `src/lib.rs` declares all 33 modules unconditionally. The only
  platform-conditional module declarations are inside `process/mod.rs`
  (`linux`, `macos`, `unix`, a stub `platform`) and `tmux/mod.rs` (`vt`).

## Method

Host: macOS, Homebrew `rust` 1.99.0, no `rustup`, no `nix` CLI. `rustup
target add` was not available. The official 1.99.0 tarballs (`rustc`,
`rust-std` for `aarch64-apple-darwin` and for `x86_64-pc-windows-gnu`, from
`static.rust-lang.org/dist/channel-rust-1.99.0.toml`) were installed under
`/tmp/rust-official` and selected with `RUSTC=`. Homebrew's rustc rejects the
official `rust-std` rlibs (`E0514`, incompatible rustc build), so the
official rustc was required.

The `-gnu` target was used instead of `-msvc` because `cargo check` needs no
linker and `src/` contains no `target_env` or `target_family` cfg; the Rust
level inventory is the same for both. MSVC differs only at link time and in
which C toolchain the -sys crates need.

### Stage 1: honest check

```sh
RUSTC=/tmp/rust-official/bin/rustc \
cargo check --target x86_64-pc-windows-gnu --no-default-features --keep-going --message-format short
```

Exit 101. Zero Rust errors are reached. Five build scripts fail with
`failed to find tool "x86_64-w64-mingw32-gcc"`:

| build script | pulled by |
| --- | --- |
| `aws-lc-sys 0.44.0` | `aws-lc-rs` (rustls default provider, via `reqwest`, `tokio-tungstenite`, `jsonwebtoken` with `aws_lc_rs`) |
| `libsqlite3-sys 0.38.2` | `rusqlite` (`bundled`) |
| `libz-sys 1.1.29` | `libgit2-sys` (`git2`) |
| `lzma-sys 0.1.20` | `xz2` |
| `zstd-sys 2.1.0` | `zstd-safe` (`zstd`) |

`libgit2-sys` itself is not reached because `libz-sys` fails first. With a
real mingw or MSVC toolchain these six are ordinary C builds; `aws-lc-sys`
additionally panics without `nasm` (`builder/nasm_builder.rs:137`) unless
its prebuilt NASM objects are used.

### Stage 2: reach the crate's own errors

Same command with a shim standing in for `x86_64-w64-mingw32-gcc` and
`nasm` (writes an empty object for every `-o`/`-Fo`, answers cc-rs's family
probe with `gcc` and openssl-sys's version probe), plus
`OPENSSL_NO_VENDOR=1`, `AWS_LC_SYS_CMAKE_BUILDER=0`, `AWS_LC_SYS_NO_ASM=1`.
This is diagnostic only: nothing links, but every build script passes and
rustc checks the crate.

Result: `error: could not compile agent-of-empires (lib) due to 221 previous
errors`. No dependency crate errors. The `aoe` binary target was not reached
(the lib failed first), and test targets were not checked.

## First-error inventory

### By module

| module | errors | files |
| --- | --- | --- |
| `session` | 117 | `config/container_config/seed/guard.rs` 27, `config/container_config/seed.rs` 27, `anchored_fs.rs` 25, `config/container_config/seed/hermes.rs` 10, `config/container_config.rs` 10, `config/container_config/seed/hermes/mixed.rs` 9, `config/container_config/seed/guard/inventory.rs` 8, `storage.rs` 1 |
| `migrations` | 49 | `v017_rewrite_hook_strings_for_per_user_base.rs` 35, `v033_isolate_sandbox_content.rs` 14 |
| `cli` | 15 | `serve.rs` 14, `extract_session_id.rs` 1 |
| `hooks` | 12 | `dir_guard.rs` 7, `config_io.rs` 4, `status_file.rs` 1 |
| `process` | 7 | `runner/mod.rs` 3, `runner/shared.rs` 2, `runner/connection.rs` 2 |
| `tui` | 5 | `home/live_send.rs` 5 |
| `server` | 5 | `tunnel.rs` 3, `ip_discovery.rs` 1, `api/file_provenance.rs` 1 |
| `acp` | 5 | `acp_client/control.rs` 5 |
| `plugin` | 3 | `host.rs` 3 |
| `tmux` | 2 | `session.rs` 2 |
| `logging` | 1 | `logging.rs` 1 |

Modules with zero Windows errors: `agents`, `build_info`, `claude_settings`,
`containers`, `daemon`, `events`, `file_watch`, `git`, `github`, `sound`,
`status_hooks`, `task_util`, `telemetry`, `terminal`, `tips`, `update`,
`util`, `yaml`. (`git`, `containers`, `update`, `file_watch`, `telemetry`
already carry `cfg(unix)`/`cfg(not(unix))` pairs for their POSIX bits.)

### By cause

| cause | errors | where |
| --- | --- | --- |
| `nix::*` paths unresolved (`nix` compiles to an empty crate off unix) | 154 mentions | `session` (seed guard, anchored_fs), `migrations` v017/v033, `cli/serve.rs` (14: `errno`, `sys`, `unistd`), `hooks/dir_guard.rs`, `server/tunnel.rs`, `server/ip_discovery.rs` (`ifaddrs`), `server/api/file_provenance.rs` |
| `libc` unlinked (`libc` is already `cfg(unix)` in Cargo.toml, its use sites are not) | 88 mentions | `session/config/container_config.rs`, seed guard, `hooks/config_io.rs`, `acp/acp_client/control.rs`, `migrations` |
| `std::os::unix::*` and `std::os::fd` | 46 | `MetadataExt` (`dev`, `ino`, `mode`, `nlink`, `mtime`, `ctime`, `uid`), `PermissionsExt::from_mode`, `OpenOptionsExt::{mode, custom_flags}`, `OsStringExt::from_vec`, `AsFd` |
| `tokio::net::UnixStream`/`UnixListener`/`tokio::net::unix` | 8 | `process/runner/{mod,connection,shared}.rs`, `acp/acp_client/control.rs` |
| ungated reference to a `cfg(unix)` item | 20 | `tmux::vt` from `tmux/session.rs:172,758` and `tui/home/live_send.rs:657,666,667,672,1012` (14); `process::rename_exclusive` from `session/anchored_fs.rs:358,392`; `process::worker::reap_group_escalating` from `plugin/host.rs:372,562,603`; `replace_file_no_follow_as` from `session/storage.rs:2511` |
| the one `cfg(windows)` branch is itself broken | 1 | `logging.rs:744` `MetadataExt::file_index` is unstable (`windows_by_handle`, E0658) |

Error codes: E0433 129, E0599 63, E0432 21, E0425 7, E0658 1.

## Module inventory

Line counts are `wc -l` over `*.rs`. "cfg sites" counts
`cfg(unix)` / `cfg(not(unix))` / `cfg(target_os = ...)` occurrences.
"Needed by client-only CLI" is read off `src/main.rs`, `src/cli/*.rs`
imports, and the `runtime_read` modules of PR #4136 (the baseline for CLI
reads through the Daemon; not merged, fetched from
`jerome-benoit:aoe-cli-server-client`).

| module | lines | needed by client-only CLI? | POSIX-only today? | gating needed |
| --- | --- | --- | --- | --- |
| `cli` | 16840 | yes (the surface) | no; 2 `cfg(unix)` sites, both in `serve.rs` | `serve.rs` (14 errors: `nix::{errno, sys, unistd}` calls between lines 675 and 1296) and `extract_session_id.rs:93`; `definition.rs` names `process::runner::AcpRunnerArgs`; imports `session` 296, `tmux` 75, `acp` 64, `agents` 38, `process` 25 |
| `daemon` | 592 | yes (HTTP client + wire DTOs) | no; 0 cfg sites, 0 errors | none for itself; it imports `session::{SessionScope, View, smart_rename::SmartRenameState}` and `acp::state::RateLimitInfo` |
| `acp::client` | ~100 KB, 5 files | yes for structured-view reads/WS | no; has its own `cfg(not(unix))` fallback (`passphrase_session.rs:255`) | imports `server::login::decode_binding_secret`, `session::get_app_dir`, `plugin::ui_state::UiSnapshot`, `cli::serve` |
| `acp` (rest: supervisor, runner lifecycle, sandbox, fs_handler, node, ...) | 43584 | no | partly; 19 `cfg(unix)`, 4 `cfg(not(unix))`; `UnixListener` 9, `UnixStream` 4 | `acp_client/control.rs` (5 errors: `std::os::fd`, `tokio::net::unix`, `libc`) |
| `session::config` + `get_app_dir`/profile dirs | ~920 KB of source | yes (`main.rs` loads config and resolves the app dir for every command) | `container_config.rs` and `container_config/seed*` are: 91 of session's 117 errors | `config/container_config.rs:906,935,1255` (`OpenOptionsExt`, `libc::O_NOFOLLOW`, `from_mode`), all of `seed/`; `config/mod.rs` itself is clean |
| `session` (rest) | 95972 total | partly: `Instance`, `Status`, `Config`, `list_profiles`, `storage` are used by `cli/list.rs` and `cli/status.rs` | 88 `cfg(unix)`, 10 `cfg(not(unix))`, 9 macOS, 2 Linux; `nix` 59, `libc` 44 | `anchored_fs.rs` (25), `storage.rs:2511`; `storage.rs` already has `cfg(not(unix))` twins for its no-follow reads |
| `migrations` | 18384 | on the path: `main.rs` runs `run_migrations_with` for every subcommand except the early-return set (`completion`, `init`, `extract-session-id`, `tmux`, `agents`, `logs`, `log-level`, `sounds`, `theme`, `settings`, `mcp`, `skill`, `uninstall`, `update`, `migrate`, `stop`) | 18 `cfg(unix)`, 3 `cfg(not(unix))`, 7 Linux, 3 macOS; `nix` 54, `libc` 33 | `v017` (35), `v033` (14); `store_fs.rs` and `v027` already have `cfg(not(unix))` twins |
| `logging` | 1109 | yes (`main.rs` subscriber init) | no; 3 `cfg(unix)`, 1 `cfg(windows)` | `logging.rs:744` (`file_index` unstable) |
| `telemetry` | 1823 | yes (`track_cli_command` on every subcommand) | no; `cfg(unix)`/`cfg(not(unix))` pair in `state.rs` | none |
| `update` | 1237 | yes (`aoe update`, `print_update_notice` in `list`/`status`) | 13 `cfg(unix)` sites in `install.rs`, all paired | none (0 errors) |
| `plugin` | 11422 | yes for `cli::graft` (plugin-grafted commands load the registry on parse failure) and `aoe plugin` | 7 `cfg(unix)`, 2 `cfg(not(unix))`; `launch.rs`, `fetch.rs` have fallbacks | `host.rs:372,562,603` (`worker::reap_group_escalating` is `cfg(unix)`) |
| `agents` | 1849 | indirectly (session and hooks import it) | no; 3 `cfg(unix)` test-only sites | none |
| `hooks` | 5890 | no (hook scripts run beside sessions) | yes: `dir_guard.rs` is `nix`/`std::os::fd` throughout; 7 Linux, 2 macOS, 5 unix sites | `dir_guard.rs` (7), `config_io.rs` (4), `status_file.rs` (1); `session` imports `hooks` at 150 sites |
| `status_hooks` | 621 | config type only (`session` re-exports `StatusHookConfig`) | 1 `cfg(unix)` site | none (0 errors) |
| `process` | 7826 | PR #4136 uses `process::effective_uid` (14 sites) and `process::runtime_io` constants; `cli/list.rs` and `cli/status.rs` use `process::systemd` | yes: 34 `cfg(unix)`, 13 Linux, 15 macOS, 6 `cfg(not(unix))`; `nix` 39, `libc` 23; `UnixListener`/`UnixStream` 13 | `runner/` (7 errors, Unix sockets); `rename_exclusive` and `reset_signals_on_exec` are `cfg(unix)` with no twin; `platform` stub exists for non-Linux/macOS process-group calls |
| `tmux` | 23795 | no | yes: 17 `cfg(unix)`, 7 Linux; `libc` 44; `vt.rs` (`cfg(unix)`) holds `UnixListener` 27, `UnixStream` 55; 123 `tmux_command()` call sites in `tmux/` alone (`src/tmux/mod.rs:160`) | `session.rs:172,758` reference `vt` ungated; `session` imports `tmux` at 314 sites, `cli` at 75, `tui` at 284 |
| `server` | 54784 | PR #4136 uses `server::runtime_ws::{CONNECTION_BUDGET, NAMESPACE}`, `server::runtime_uds::{serve, try_publish}` (Linux-gated in the PR), `server::login::LoginManager`; `acp::client` uses `server::login::decode_binding_secret` | Daemon: `axum` 437 use sites, `sd_notify` 6; 70 `cfg(unix)`, 17 `cfg(not(unix))` (mostly `live_ws.rs`, `startup.rs`) | `tunnel.rs` (3), `ip_discovery.rs` (1), `api/file_provenance.rs` (1) |
| `containers` | 4537 | no | 1 `cfg(unix)` site; 0 errors | none; `session` imports it at 70 sites |
| `git` | 6767 | `aoe worktree` only | `cfg(unix)`/`cfg(not(unix))` pairs; `git2` 119 use sites; 0 errors | none at compile time; `libgit2-sys` + `libz-sys` need a C toolchain |
| `tui` | 113034 | no (TUI client is a separate wave) | 70 `cfg(unix)`, 9 `cfg(not(unix))`, 1 `cfg(windows)`; `crossterm` 171; `nix` 11 | `home/live_send.rs` (5 `tmux::vt` references) |
| `events`, `file_watch`, `github`, `sound`, `tips`, `terminal`, `claude_settings`, `build_info`, `task_util`, `util`, `yaml` | small | mixed | no; 0 errors | none |

## Where `main.rs` and `lib.rs` touch POSIX code

`src/lib.rs` declares 29 `pub mod` and 4 private modules with no `cfg` on
any of them.

`src/main.rs`, before clap parses anything:

- `aoe __vt-pipe <socket>` calls `tui::run_vt_pipe`, which is `cfg(unix)`
  with a `cfg(not(unix))` twin that returns `ErrorKind::Unsupported`
  (`src/tui/mod.rs:99-110`).
- `aoe __smart-rename [--force] <profile> <id>` calls
  `session::smart_rename::run_smart_rename_now`, ungated.

After clap, on every command path:

- `Cli::try_parse` failure loads the plugin registry via
  `cli::graft::augmented_command` (plugin-grafted subcommands).
- `session::debug_namespace_drift`, `session::get_app_dir`,
  `session::load_config`, then `logging::init_subscriber_with_options`
  (`acp::session_tee::SessionTeeLayer` only for `serve`).
- `telemetry::track_cli_command` for every named subcommand.
- `migrations::run_migrations_with` and
  `session::poller::configure_session_id_poller_max_threads` for every
  subcommand outside the early-return set listed in the table above, then
  `session::collect_startup_config_warnings`.
- `Commands::AcpRunner` dispatches to `process::runner::run` (Unix sockets,
  7 of the `process` errors).

## Dependencies to make target-conditional or drop for a client-only build

| crate | declared today | on the Windows graph | used by | note |
| --- | --- | --- | --- | --- |
| `nix 0.31` (`signal`, `process`, `net`, `fs`, `resource`, `dir`, `user`, `poll`) | unconditional | yes, resolves to an empty crate; named in 154 unresolved-path errors | `acp`, `cli`, `hooks`, `migrations`, `plugin`, `process`, `server`, `session`, `status_hooks`, `tui` | the only unconditional unix-only direct dependency |
| `portable-pty 0.9` | unconditional | yes; brings `nix 0.28`, `filedescriptor`, `winapi`, `shared_library` | no use sites in `src/`, `tests/`, `build.rs` | dead dependency |
| `libc`, `sd-notify` | already `[target.'cfg(unix)']` | absent | `libc` use sites are ungated in `session`, `hooks`, `acp`, `migrations` (named in 88 errors) | the declaration is gated, the call sites are not |
| `git2 0.21` (`vendored-openssl`) | unconditional | yes; `libgit2-sys` + `libz-sys` compile C; no `openssl-sys` or `libssh2-sys` on Windows | `git` 119, `session` 66, `tui` 11, `server` 2 | not on the read path; needed by `aoe worktree` |
| `rusqlite 0.40` (`bundled`) | unconditional | yes; `libsqlite3-sys` compiles C | `session` 43, `acp` 17, `migrations` 11, `plugin` 10, `cli` 6, `events` 3 | opencode session store reader, event log, plugin state |
| `zstd 0.14` | unconditional | yes; `zstd-sys` compiles C | `session` 2 | |
| `xz2 0.1` | unconditional | yes; `lzma-sys` compiles C ("needs system liblzma" per Cargo.toml) | `acp` 1 (bundled-Node tar.xz fallback) | |
| `reqwest 0.13` (`rustls`), `tokio-tungstenite 0.30` (`rustls-tls-webpki-roots`), `jsonwebtoken 11` (`aws_lc_rs`) | unconditional | yes; all three select `aws-lc-rs`, so `aws-lc-sys` compiles C and wants NASM | `reqwest`: `daemon` 10, `acp` 24, `server` 17, `github` 11, `containers` 9; `tokio-tungstenite`: `acp` 17, `server` 7; `jsonwebtoken`: `server` 1 | the client needs `reqwest` and `tokio-tungstenite`; `jsonwebtoken` is Web Push (Daemon) |
| `axum 0.8`, `tower-http`, `cap-std`, `argon2`, `p256`, `hkdf`, `aes-gcm`, `subtle`, `mime_guess` | unconditional | yes, pure Rust, no errors | `server` only (`axum` 437 sites) | Daemon surface; nothing in `cli` or `daemon` uses them |
| `tokio` (`full`) | unconditional | yes | everywhere | `tokio::net::Unix*` and `tokio::signal::unix` are the unix-only APIs (8 errors + `tui/app.rs:560`, `server/startup.rs:941` which are already gated) |
| `crossterm`, `ratatui`, `vt100`, `ratatui-textarea`, `tui-input`, `ansi-to-tui`, `pulldown-cmark`, `rattles` | unconditional | yes, no errors | `tui`, `terminal` | TUI surface |
| `notify`, `fs2`, `which`, `webbrowser`, `dirs`, `tempfile`, `glob` | unconditional | yes, no errors | mixed | portable |

## Candidate seams for the crate split (facts)

1. `src/daemon/` is already a dependency-light client: 592 lines, `reqwest`
   + `serde` + `thiserror`, no `cfg`, no Windows errors. Its only ties to the
   rest of the crate are six imports of `session::{SessionScope, View,
   smart_rename::SmartRenameState}` and `acp::state::RateLimitInfo`, used by
   the wire DTOs in `daemon/wire.rs`.
2. `src/acp/client/` (http, ws, discovery, passphrase session, daemon
   manager) is the structured-view client. It imports
   `server::login::decode_binding_secret` (5 sites), `session::get_app_dir`
   (4), `plugin::ui_state::UiSnapshot`, `cli::serve`, and `daemon`. It sits
   under `acp`, whose other submodules hold 19 `cfg(unix)` sites and the
   Unix-socket supervisor side.
3. `src/session/config` plus the app-dir and profile-dir helpers in
   `session/mod.rs` are on every CLI path (`main.rs` loads config before
   dispatch). `config/mod.rs` is clean on Windows; `config/container_config.rs`
   and `config/container_config/seed/` hold 91 of the 117 `session` errors.
   `config` imports `agents` (30 sites), `hooks` (28), `migrations::v*` (15),
   `session::instance` (26) and `session::anchored_fs` (11, itself 25
   errors).
4. The CLI read path. On `main`, `cli/list.rs` and `cli/status.rs` import
   `session::{Instance, Status, Config, list_profiles, get_app_dir}`,
   `process::systemd` (3), `tmux::refresh_session_cache` (1) and
   `update::print_update_notice`. PR #4136's `cli/runtime_read/` adds
   `process::effective_uid` (14), `process::runtime_io::{KIND_MASK,
   DIRECTORY, STICKY, SOCKET, REGULAR}`, `server::runtime_ws::{CONNECTION_BUDGET,
   NAMESPACE}`, `server::runtime_uds::{serve, try_publish}` (6
   `cfg(target_os = "linux")` sites), `server::login::LoginManager` and
   `acp::client::HttpError`. Those are the symbols a client crate has to own
   or re-declare.
5. `session` as the shared data model is the densest edge in the graph:
   `session -> tmux` 314 use sites, `-> agents` 257, `-> hooks` 150,
   `-> containers` 70, `-> git` 46, `-> migrations` 37, `-> process` 23, and
   `tmux -> session` 81 back. Consumers: `tui -> session` 978, `server ->
   session` 931, `cli -> session` 296, `migrations -> session` 370. Any crate
   split that keeps `session` on the client side carries `tmux`, `hooks` and
   `containers` unless those edges are cut; any split that keeps it on the
   Daemon side needs `daemon::wire` to stop importing `session` types (seam 1).

## Reproduction notes

- Official toolchain tarballs: see `[pkg.rustc.target.aarch64-apple-darwin]`,
  `[pkg.rust-std.target.aarch64-apple-darwin]` and
  `[pkg.rust-std.target.x86_64-pc-windows-gnu]` in
  `https://static.rust-lang.org/dist/channel-rust-1.99.0.toml`; install each
  with its `install.sh --prefix=<dir> --disable-ldconfig`.
- On a host with `rustup`, `rustup target add x86_64-pc-windows-gnu` replaces
  the tarball step; stage 1 is then the plain `cargo check` above.
- A real `x86_64-w64-mingw32-gcc` (or `cargo check --target
  x86_64-pc-windows-msvc` on a Windows host with MSVC and NASM) replaces the
  stage 2 shim; the 221-error inventory is what either reaches.
