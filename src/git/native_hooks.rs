//! Preserve native hook dispatch while handing caller leases off during callbacks.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use fs2::FileExt;

use serde::{Deserialize, Serialize};

const CONTEXT: &str = "AOE_NATIVE_GIT_CONTEXT";
const PRIVATE_PREFIX: &str = "AOE_NATIVE_GIT_";
const REAL_EXEC: &str = "AOE_NATIVE_GIT_REAL_EXEC";
const PROXY_EXEC: &str = "AOE_NATIVE_GIT_PROXY_EXEC";
const PROGRAM_PREFIX: &str = "AOE_NATIVE_GIT_PROGRAM_PREFIX";
const PARAMETERS_PRESENT: &str = "AOE_NATIVE_GIT_PARAMETERS_PRESENT";
const OWNER_FD: &str = "AOE_NATIVE_GIT_OWNER_FD";

const LIVE_HOOKS: [&str; 4] = [
    "post-index-change",
    "reference-transaction",
    "post-checkout",
    "pre-auto-gc",
];

fn hooked_command(name: Option<&OsStr>) -> bool {
    name.is_some_and(|name| {
        matches!(
            name.to_str(),
            Some(
                "checkout"
                    | "worktree"
                    | "branch"
                    | "fetch"
                    | "reset"
                    | "clone"
                    | "maintenance"
                    | "gc"
                    | "commit"
                    | "merge"
                    | "rebase"
                    | "am"
                    | "cherry-pick"
                    | "revert"
                    | "read-tree"
                    | "update-index"
                    | "update-ref"
                    | "pack-refs"
                    | "repack"
                    | "push"
                    | "receive-pack"
                    | "stash"
            )
        )
    })
}
#[derive(Clone, Copy)]
pub(crate) struct CallbackContext<'a> {
    pub(crate) parent: Option<&'a str>,
    pub(crate) reference_transaction: Option<&'a str>,
}

pub(crate) trait HookLease {
    fn runtime_dir(&self) -> &Path;
    fn prepare_checkout(&mut self, owned_branch: bool) -> io::Result<()>;
    fn pause(
        &mut self,
        callback: &str,
        cwd: &Path,
        checkout_ready: bool,
        git_dir: Option<&Path>,
        context: CallbackContext<'_>,
    ) -> io::Result<()>;
    fn resume(&mut self, completed: Option<&str>) -> io::Result<()>;
    fn checkpoint(&mut self) -> io::Result<()>;
    // Release restored locks without replacing active callback boundaries.
    fn pause_retained(&mut self) -> io::Result<()>;
}

#[derive(Serialize, Deserialize)]
struct Scope {
    root: PathBuf,
    root_device: u64,
    root_inode: u64,
    real_git: PathBuf,
    real_exec: PathBuf,
    proxy_exec: PathBuf,
    program_prefix: String,
    socket: PathBuf,
    socket_device: u64,
    socket_inode: u64,
    owner_device: u64,
    owner_inode: u64,
    capability: String,
}

#[derive(Serialize, Deserialize)]
struct HookCommand {
    name: Vec<u8>,
    command: Vec<u8>,
    post_checkout: bool,
    reference_transaction: bool,
    program: String,
}

#[derive(Serialize, Deserialize)]
struct Descriptor {
    scope: Scope,
    hooks: PathBuf,
    original_hooks: Option<Vec<u8>>,
    commands: Vec<HookCommand>,
    shell: PathBuf,
    parameters_present: bool,
}

#[derive(Serialize, Deserialize)]
enum Frame {
    Enter {
        capability: String,
        parent: Option<String>,
        reference_transaction: Option<String>,
        cwd: Vec<u8>,
        checkout_ready: bool,
        git_dir: Option<Vec<u8>>,
    },
    Done,
}

enum Event {
    Frame {
        id: String,
        frame: Frame,
        control: UnixStream,
    },
    ReadError {
        id: String,
        authenticated: bool,
        error: io::Error,
    },
    ListenerError(io::Error),
    Exit(io::Result<Output>),
}

struct NativeFamily {
    pid: u32,
    complete: bool,
}

impl Drop for NativeFamily {
    fn drop(&mut self) {
        if !self.complete {
            crate::process::worker::kill_process_group(self.pid);
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}

fn inheritable(fd: RawFd, inherit: bool) -> io::Result<()> {
    // SAFETY: fcntl operates on this process's live liveness descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let flags = if inherit {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    // SAFETY: F_SETFD takes the integer flags read above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn liveness_file(file: fs::File) -> io::Result<fs::File> {
    if file.as_raw_fd() >= 3 {
        return Ok(file);
    }
    // SAFETY: duplicate our own live descriptor, without using a stdio slot.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new owned descriptor.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

fn inherited_owner_fd() -> Option<RawFd> {
    let value = std::env::var(OWNER_FD).ok()?;
    let mut parts = value.split(':');
    let fd: RawFd = parts.next()?.parse().ok()?;
    let device: u64 = parts.next()?.parse().ok()?;
    let inode: u64 = parts.next()?.parse().ok()?;
    if fd < 3 || parts.next().is_some() {
        return None;
    }
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat checks the untrusted number before any ownership or flag change.
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: a successful fstat initialized the structure.
    let metadata = unsafe { metadata.assume_init() };
    let actual_device: u64 = metadata.st_dev as _;
    let actual_inode: u64 = metadata.st_ino as _;
    (actual_device == device
        && actual_inode == inode
        && metadata.st_uid == unsafe { libc::geteuid() }
        && metadata.st_mode & libc::S_IFMT == libc::S_IFREG
        && metadata.st_mode & 0o777 == 0o600)
        .then_some(fd)
}

fn close_inherited_owner() -> io::Result<()> {
    if let Some(fd) = inherited_owner_fd() {
        // SAFETY: the inherited tuple matched fstat; close, never LOCK_UN.
        if unsafe { libc::close(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn helper_owner(scope: &Scope) -> io::Result<fs::File> {
    let root = DirectoryPin::open(scope.root.clone())?;
    let metadata = root.file.metadata()?;
    if metadata.dev() != scope.root_device || metadata.ino() != scope.root_inode {
        return Err(invalid("native runtime root changed"));
    }
    let owner = liveness_file(private_file(&scope.root.join("owner"), false)?)?;
    let metadata = owner.metadata()?;
    if metadata.dev() != scope.owner_device || metadata.ino() != scope.owner_inode {
        return Err(invalid("native runtime liveness owner changed"));
    }
    FileExt::try_lock_shared(&owner)?;
    // Acquire an independent shared lease before retiring the inherited OFD.
    close_inherited_owner()?;
    Ok(owner)
}

fn owner_hint(command: &mut Command, owner: &fs::File, identity: (u64, u64)) {
    command.env(
        OWNER_FD,
        format!("{}:{}:{}", owner.as_raw_fd(), identity.0, identity.1),
    );
}

fn env_delta(command: &mut Command, descriptor: &Descriptor) -> io::Result<Option<OsString>> {
    let parameters = match std::env::var_os("GIT_CONFIG_PARAMETERS") {
        Some(value) => strip_parameters(
            value.as_bytes(),
            &descriptor.scope.root,
            descriptor.scope.program_prefix.as_bytes(),
            descriptor.parameters_present,
        )?,
        None => None,
    };
    match &parameters {
        Some(value) => {
            command.env("GIT_CONFIG_PARAMETERS", value);
        }
        None => {
            command.env_remove("GIT_CONFIG_PARAMETERS");
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        let path = std::env::join_paths(std::env::split_paths(&path).map(|entry| {
            if entry == descriptor.scope.proxy_exec {
                descriptor.scope.real_exec.clone()
            } else {
                entry
            }
        }))
        .map_err(invalid)?;
        command.env("PATH", path);
    }
    command.env("GIT_EXEC_PATH", &descriptor.scope.real_exec);
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(PRIVATE_PREFIX.as_bytes()) {
            command.env_remove(key);
        }
    }
    Ok(parameters)
}

fn strip_parameters(
    bytes: &[u8],
    root: &Path,
    program_prefix: &[u8],
    present: bool,
) -> io::Result<Option<OsString>> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut copied = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let start = cursor;
        let mut quoted = false;
        let mut decoded = Vec::new();
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if !quoted && byte.is_ascii_whitespace() {
                break;
            }
            cursor += 1;
            match byte {
                b'\'' => quoted = !quoted,
                b'\\' if !quoted => {
                    let next = *bytes
                        .get(cursor)
                        .ok_or_else(|| invalid("invalid Git configuration quoting"))?;
                    decoded.push(next);
                    cursor += 1;
                }
                _ => decoded.push(byte),
            }
        }
        if quoted {
            return Err(invalid("unterminated Git configuration quoting"));
        }
        let remove = split_once_byte(&decoded, b'=').is_some_and(|(key, value)| {
            (key == b"core.hooksPath" && Path::new(OsStr::from_bytes(value)).starts_with(root))
                || (key.starts_with(b"hook.")
                    && key.ends_with(b".command")
                    && value.starts_with(program_prefix))
        });
        if remove {
            // Git adds exactly one separator before each injected -c pair.
            let remove_start = if start > copied && bytes[start - 1] == b' ' {
                start - 1
            } else {
                start
            };
            output.extend_from_slice(&bytes[copied..remove_start]);
            copied = cursor;
        }
    }
    output.extend_from_slice(&bytes[copied..]);
    if output.is_empty() && !present {
        Ok(None)
    } else {
        Ok(Some(OsString::from_vec(output)))
    }
}

fn split_once_byte(bytes: &[u8], byte: u8) -> Option<(&[u8], &[u8])> {
    bytes
        .iter()
        .position(|value| *value == byte)
        .map(|index| (&bytes[..index], &bytes[index + 1..]))
}

fn original_command(descriptor: &Descriptor, args: &[OsString]) -> io::Result<Command> {
    let mut command = Command::new(&descriptor.scope.real_git);
    env_delta(&mut command, descriptor)?;
    command.args(args).env("LC_ALL", "C");
    Ok(command)
}

fn checked_output(command: &mut Command) -> io::Result<Vec<u8>> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(invalid(String::from_utf8_lossy(&output.stderr)))
    }
}

fn global_prefix(args: &[OsString]) -> io::Result<usize> {
    let mut cursor = 0;
    while let Some(arg) = args.get(cursor) {
        let bytes = arg.as_bytes();
        if !bytes.starts_with(b"-") || bytes == b"-" {
            break;
        }
        if bytes == b"--" {
            return Ok(cursor + 1);
        }
        if [
            b"-C".as_slice(),
            b"-c",
            b"--git-dir",
            b"--work-tree",
            b"--namespace",
            b"--config-env",
        ]
        .contains(&bytes)
        {
            if args.get(cursor + 1).is_none() {
                return Err(invalid("Git global option requires a value"));
            }
            cursor += 2;
        } else {
            cursor += 1;
        }
    }
    Ok(cursor)
}

fn probe(descriptor: &Descriptor, prefix: &[OsString], args: &[&str]) -> io::Result<Vec<u8>> {
    let mut command = original_command(descriptor, prefix)?;
    command.args(args);
    checked_output(&mut command)
}

fn trim_line(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

fn executable(path: &Path) -> io::Result<bool> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(invalid)?;
    // SAFETY: path is a NUL-terminated pathname.
    if unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => Ok(false),
        _ => Err(error),
    }
}

fn effective_cwd(prefix: &[OsString]) -> io::Result<PathBuf> {
    let mut cwd = std::env::current_dir()?;
    let mut cursor = 0;
    while cursor < prefix.len() {
        let arg = prefix[cursor].as_bytes();
        let directory = if arg == b"-C" {
            cursor += 1;
            Some(prefix[cursor].as_os_str())
        } else if arg.starts_with(b"-C") && arg.len() > 2 {
            Some(OsStr::from_bytes(&arg[2..]))
        } else {
            if [
                b"-c".as_slice(),
                b"--git-dir",
                b"--work-tree",
                b"--namespace",
                b"--config-env",
            ]
            .contains(&arg)
            {
                cursor += 1;
            }
            None
        };
        if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
            cwd = cwd.join(directory);
        }
        cursor += 1;
    }
    Ok(cwd)
}

fn clone_options(args: &[OsString]) -> io::Result<(Vec<&OsStr>, Option<&OsStr>)> {
    let mut values = Vec::new();
    let mut template = None;
    let mut cursor = 0;
    while let Some(arg) = args.get(cursor) {
        let bytes = arg.as_bytes();
        if bytes == b"--" {
            break;
        }
        cursor += 1;
        if let Some(long) = bytes.strip_prefix(b"--") {
            let (name, inline) = split_once_byte(long, b'=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            if name == b"no-config" {
                values.clear();
                continue;
            }
            if name == b"no-template" {
                template = None;
                continue;
            }
            let name = if !name.is_empty() && b"config".starts_with(name) {
                b"config".as_slice()
            } else if !name.is_empty() && b"template".starts_with(name) {
                b"template".as_slice()
            } else {
                name
            };
            let takes_value = matches!(
                name,
                b"config"
                    | b"jobs"
                    | b"template"
                    | b"reference"
                    | b"reference-if-able"
                    | b"origin"
                    | b"branch"
                    | b"revision"
                    | b"upload-pack"
                    | b"depth"
                    | b"shallow-since"
                    | b"shallow-exclude"
                    | b"separate-git-dir"
                    | b"ref-format"
                    | b"server-option"
                    | b"filter"
                    | b"bundle-uri"
            );
            if takes_value {
                let value = match inline {
                    Some(value) => OsStr::from_bytes(value),
                    None => {
                        let value = args
                            .get(cursor)
                            .ok_or_else(|| invalid("Git clone option requires a value"))?;
                        cursor += 1;
                        value.as_os_str()
                    }
                };
                if name == b"config" {
                    values.push(value);
                } else if name == b"template" {
                    template = Some(value);
                }
            }
        } else if let Some(short) = bytes.strip_prefix(b"-") {
            for (index, flag) in short.iter().enumerate() {
                if b"cjobu".contains(flag) {
                    let value = if index + 1 < short.len() {
                        OsStr::from_bytes(&short[index + 1..])
                    } else {
                        let value = args
                            .get(cursor)
                            .ok_or_else(|| invalid("Git clone option requires a value"))?;
                        cursor += 1;
                        value.as_os_str()
                    };
                    if *flag == b'c' {
                        values.push(value);
                    }
                    break;
                }
            }
        }
    }
    Ok((values, template))
}

fn clone_config(
    descriptor: &Descriptor,
    prefix: &[OsString],
    args: &[OsString],
    node: &Path,
) -> io::Result<Vec<u8>> {
    let (values, template) = clone_options(args)?;
    let bootstrap = node.join("clone-config");
    let template = match template
        .map(OsStr::to_owned)
        .or_else(|| std::env::var_os("GIT_TEMPLATE_DIR"))
    {
        Some(template) => Some(template),
        None => {
            let mut command = original_command(descriptor, prefix)?;
            let output = command
                .arg("--git-dir")
                .arg(&bootstrap)
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(["config", "--path", "--get", "init.templateDir"])
                .output()?;
            if output.status.success() {
                Some(OsString::from_vec(trim_line(output.stdout)))
            } else if output.status.code() == Some(1) {
                None
            } else {
                return Err(invalid(String::from_utf8_lossy(&output.stderr)));
            }
        }
    };
    let template_config = if let Some(template) = template.filter(|path| !path.is_empty()) {
        match fs::read(effective_cwd(prefix)?.join(template).join("config")) {
            Ok(config) => Some(config),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if !values.is_empty() || template_config.is_some() {
        fs::create_dir(&bootstrap)?;
        fs::create_dir(bootstrap.join("objects"))?;
        fs::create_dir(bootstrap.join("refs"))?;
        fs::write(bootstrap.join("HEAD"), b"ref: refs/heads/clone-bootstrap\n")?;
        if let Some(config) = template_config {
            fs::write(bootstrap.join("config"), config)?;
        }
        for value in values {
            let (key, value) =
                split_once_byte(value.as_bytes(), b'=').unwrap_or((value.as_bytes(), b"true"));
            let mut command = original_command(descriptor, prefix)?;
            command
                .arg("--git-dir")
                .arg(&bootstrap)
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .args([
                    OsStr::new("config"),
                    OsStr::new("--local"),
                    OsStr::new("--add"),
                    OsStr::from_bytes(key),
                    OsStr::from_bytes(value),
                ]);
            checked_output(&mut command)?;
        }
    }
    let mut command = original_command(descriptor, prefix)?;
    command
        .arg("--git-dir")
        .arg(&bootstrap)
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["config", "--null", "--list"]);
    checked_output(&mut command)
}

fn descriptor_for(
    mut descriptor: Descriptor,
    args: &[OsString],
) -> io::Result<(Descriptor, usize, PathBuf)> {
    let prefix_len = global_prefix(args)?;
    let prefix = &args[..prefix_len];
    let parameters = {
        let mut pristine = Command::new(&descriptor.scope.real_git);
        env_delta(&mut pristine, &descriptor)?
    };
    descriptor.parameters_present = parameters.is_some();
    let node = descriptor.scope.root.join(format!(
        "node-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&node)?;
    let is_clone = args.get(prefix_len).is_some_and(|arg| arg == "clone");
    let config = if is_clone {
        clone_config(&descriptor, prefix, &args[prefix_len + 1..], &node)?
    } else {
        probe(&descriptor, prefix, &["config", "--null", "--list"])?
    };
    let mut commands: BTreeMap<Vec<u8>, (Option<Vec<u8>>, bool, bool)> = BTreeMap::new();
    for item in config
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let (key, value) = split_once_byte(item, b'\n').unwrap_or((item, &[]));
        let Some(key) = key.strip_prefix(b"hook.") else {
            continue;
        };
        if let Some(name) = key.strip_suffix(b".command") {
            commands.entry(name.to_vec()).or_default().0 = Some(value.to_vec());
        } else if let Some(name) = key.strip_suffix(b".event") {
            if value == b"post-checkout" {
                commands.entry(name.to_vec()).or_default().1 = true;
            } else if value == b"reference-transaction" {
                commands.entry(name.to_vec()).or_default().2 = true;
            }
        }
    }

    descriptor.hooks = node.join("hooks");
    fs::create_dir(&descriptor.hooks)?;
    descriptor.original_hooks = if is_clone {
        None
    } else {
        let mut hooks = trim_line(probe(
            &descriptor,
            prefix,
            &["rev-parse", "--git-path", "hooks"],
        )?);
        let worktree_add = args.get(prefix_len).is_some_and(|arg| arg == "worktree")
            && args.get(prefix_len + 1).is_some_and(|arg| arg == "add");
        if worktree_add && !Path::new(OsStr::from_bytes(&hooks)).is_absolute() {
            hooks = effective_cwd(prefix)?
                .join(OsStr::from_bytes(&hooks))
                .into_os_string()
                .into_vec();
        }
        Some(hooks)
    };
    descriptor.commands.clear();
    let helper = descriptor.scope.proxy_exec.join("git");
    for (index, (name, (command, post_checkout, reference_transaction))) in
        commands.into_iter().enumerate()
    {
        let Some(command) = command else {
            continue;
        };
        let program = format!("{}-{index}", descriptor.scope.program_prefix);
        let path = descriptor.scope.proxy_exec.join(&program);
        match symlink(&helper, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read_link(&path)? != helper {
                    return Err(invalid("native configured hook program was replaced"));
                }
            }
            Err(error) => return Err(error),
        }
        descriptor.commands.push(HookCommand {
            name,
            command,
            post_checkout,
            reference_transaction,
            program,
        });
    }
    if let Some(original_hooks) = &descriptor.original_hooks {
        let original_hooks = effective_cwd(prefix)?.join(OsStr::from_bytes(original_hooks));
        match fs::read_dir(&original_hooks) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    if entry.file_type()?.is_dir()
                        || name.to_str().is_some_and(|name| LIVE_HOOKS.contains(&name))
                    {
                        continue;
                    }
                    let target = descriptor.hooks.join(name);
                    if executable(&entry.path())? {
                        symlink(&helper, target)?;
                    } else {
                        fs::write(&target, [])?;
                        fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for event in LIVE_HOOKS {
        symlink(&helper, descriptor.hooks.join(event))?;
    }
    descriptor.shell = match probe(&descriptor, prefix, &["var", "GIT_SHELL_PATH"]) {
        Ok(value) => PathBuf::from(OsString::from_vec(trim_line(value))),
        Err(_) => {
            let mut command = original_command(&descriptor, &[])?;
            command.env_remove("GIT_EXEC_PATH").arg("--exec-path");
            let exec = PathBuf::from(OsString::from_vec(trim_line(checked_output(&mut command)?)));
            let mut first = Vec::new();
            BufReader::new(fs::File::open(exec.join("git-sh-setup"))?)
                .read_until(b'\n', &mut first)?;
            let first = trim_line(first);
            let shell = first
                .strip_prefix(b"#!")
                .ok_or_else(|| invalid("cannot determine Git's shell interpreter"))?;
            PathBuf::from(OsStr::from_bytes(shell))
        }
    };
    let path = node.join("descriptor.json");
    fs::write(&path, serde_json::to_vec(&descriptor).map_err(invalid)?)?;
    Ok((descriptor, prefix_len, path))
}

fn configured_git(
    descriptor: &Descriptor,
    args: &[OsString],
    prefix_len: usize,
    path: &Path,
) -> io::Result<Command> {
    let mut command = original_command(descriptor, &args[..prefix_len])?;
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", descriptor.hooks.display()));
    for hook in &descriptor.commands {
        let mut value = b"hook.".to_vec();
        value.extend_from_slice(&hook.name);
        value.extend_from_slice(b".command=");
        value.extend_from_slice(hook.program.as_bytes());
        command.arg("-c").arg(OsString::from_vec(value));
    }
    command.args(&args[prefix_len..]);
    command
        .env("GIT_EXEC_PATH", &descriptor.scope.proxy_exec)
        .env(CONTEXT, path);
    command
        .env(REAL_EXEC, &descriptor.scope.real_exec)
        .env(PROXY_EXEC, &descriptor.scope.proxy_exec)
        .env(PROGRAM_PREFIX, &descriptor.scope.program_prefix)
        .env(
            PARAMETERS_PRESENT,
            if descriptor.parameters_present {
                "1"
            } else {
                "0"
            },
        );
    if let Some(parent) = std::env::var_os("AOE_NATIVE_GIT_PARENT") {
        command.env("AOE_NATIVE_GIT_PARENT", parent);
    }
    Ok(command)
}

fn routing_git(descriptor: &Descriptor, args: &[OsString], path: &Path) -> io::Result<Command> {
    let mut command = original_command(descriptor, args)?;
    command
        .env("GIT_EXEC_PATH", &descriptor.scope.proxy_exec)
        .env(CONTEXT, path);
    command
        .env(REAL_EXEC, &descriptor.scope.real_exec)
        .env(PROXY_EXEC, &descriptor.scope.proxy_exec)
        .env(PROGRAM_PREFIX, &descriptor.scope.program_prefix)
        .env(
            PARAMETERS_PRESENT,
            if descriptor.parameters_present {
                "1"
            } else {
                "0"
            },
        );
    if let Some(parent) = std::env::var_os("AOE_NATIVE_GIT_PARENT") {
        command.env("AOE_NATIVE_GIT_PARENT", parent);
    }
    Ok(command)
}

// Allocation and sweep share the same private namespaces and lock order.
const RUNTIME_NAMESPACE: &str = "native-git-runtimes-v2";
const RUNTIME_MARKER: &str = "aoe-native-git-runtime-v2";

struct DirectoryPin {
    path: PathBuf,
    file: fs::File,
}

impl DirectoryPin {
    fn open(path: PathBuf) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&path)?;
        let pin = Self { path, file };
        pin.verify()?;
        Ok(pin)
    }

    fn verify(&self) -> io::Result<()> {
        let retained = self.file.metadata()?;
        let current = fs::symlink_metadata(&self.path)?;
        if !current.is_dir()
            || current.uid() != unsafe { libc::geteuid() }
            || current.mode() & 0o777 != 0o700
            || current.dev() != retained.dev()
            || current.ino() != retained.ino()
        {
            return Err(invalid("native runtime directory ownership changed"));
        }
        Ok(())
    }
}

struct RuntimeNamespace {
    pin: DirectoryPin,
    lock: fs::File,
}

impl RuntimeNamespace {
    fn open(path: PathBuf, create: bool) -> io::Result<Self> {
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let pin = DirectoryPin::open(path)?;
        let lock = private_file(&pin.path.join("namespace.lock"), create)?;
        lock.lock_exclusive()?;
        let namespace = Self { pin, lock };
        namespace.verify()?;
        Ok(namespace)
    }

    fn verify(&self) -> io::Result<()> {
        self.pin.verify()?;
        let retained = self.lock.metadata()?;
        let current = fs::symlink_metadata(self.pin.path.join("namespace.lock"))?;
        if !current.is_file() || retained.dev() != current.dev() || retained.ino() != current.ino()
        {
            return Err(invalid("native runtime allocation lock changed"));
        }
        Ok(())
    }
}

struct RuntimeDirectory {
    pin: DirectoryPin,
    remove_on_drop: bool,
}

impl RuntimeDirectory {
    fn create(namespace: &RuntimeNamespace, name: &str) -> io::Result<Self> {
        namespace.verify()?;
        let path = namespace.pin.path.join(name);
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let pin = DirectoryPin::open(path)?;
        namespace.pin.file.sync_all()?;
        Ok(Self {
            pin,
            remove_on_drop: true,
        })
    }

    fn path(&self) -> &Path {
        &self.pin.path
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop && self.pin.verify().is_ok() {
            if let Err(error) = fs::remove_dir_all(self.path()) {
                tracing::debug!(target: "git.native_hooks", phase = "cleanup", %error, "native runtime cleanup deferred");
            }
        }
    }
}

struct Runtime {
    _socket_root: RuntimeDirectory,
    _root: RuntimeDirectory,
    owner: Option<fs::File>,
    group: fs::File,
    namespace_pins: [DirectoryPin; 2],
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self._socket_root.remove_on_drop = false;
        self._root.remove_on_drop = false;
        // Closing our reference must not unlock a detached Git's shared OFD.
        drop(self.owner.take());
        if self.namespace_pins[0].verify().is_ok() && self._root.pin.verify().is_ok() {
            if let Some(app_dir) = self.namespace_pins[0].path.parent() {
                if let Err(error) = sweep_runtime(app_dir) {
                    tracing::debug!(target: "git.native_hooks", phase = "cleanup", %error, "native runtime sweep deferred");
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RuntimePhase {
    Allocating,
    Ready,
}

#[derive(Serialize, Deserialize)]
struct RuntimeMarker {
    kind: String,
    phase: RuntimePhase,
    owner_device: u64,
    owner_inode: u64,
    group_device: u64,
    group_inode: u64,
    socket_root: PathBuf,
    socket_namespace: PathBuf,
    socket_namespace_device: u64,
    socket_namespace_inode: u64,
    socket_device: Option<u64>,
    socket_inode: Option<u64>,
    token: String,
}

fn private_file(path: &Path, create: bool) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(invalid("native runtime file ownership changed"));
    }
    Ok(file)
}

fn publish_runtime_state(root: &DirectoryPin, marker: &RuntimeMarker) -> io::Result<()> {
    root.verify()?;
    let pending = root.path.join("state.next");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    serde_json::to_writer(&mut file, marker).map_err(invalid)?;
    file.sync_all()?;
    fs::rename(pending, root.path.join("state.json"))?;
    root.file.sync_all()
}

fn read_runtime_state(root: &Path) -> io::Result<RuntimeMarker> {
    serde_json::from_reader(private_file(&root.join("state.json"), false)?)
        .map_err(|_| invalid("invalid native runtime allocation state"))
}

fn socket_namespace_name() -> String {
    format!("aoe-native-git-v2-{}", unsafe { libc::geteuid() })
}

fn socket_namespace_path(name: &str) -> io::Result<(PathBuf, usize)> {
    // SAFETY: an all-zero sockaddr_un is valid; only its array length is used.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let limit = address.sun_path.len();
    let namespace = socket_namespace_name();
    let platform = std::env::temp_dir();
    let fits = |base: &Path| {
        base.join(&namespace)
            .join(name)
            .join("control")
            .as_os_str()
            .len()
            < limit
    };
    let base = if fits(&platform) {
        platform
    } else {
        PathBuf::from("/tmp")
    };
    let length = base
        .join(&namespace)
        .join(name)
        .join("control")
        .as_os_str()
        .len();
    if length >= limit {
        return Err(invalid(format!(
            "native socket base {} requires {length} bytes, limit {limit}",
            base.display()
        )));
    }
    Ok((base.join(namespace), length))
}

fn sweep_runtime(app_dir: &Path) -> io::Result<()> {
    let namespace = match RuntimeNamespace::open(app_dir.join(RUNTIME_NAMESPACE), false) {
        Ok(namespace) => namespace,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    sweep_namespace(&namespace)
}

fn sweep_namespace(namespace: &RuntimeNamespace) -> io::Result<()> {
    namespace.verify()?;
    let socket_namespace_name = socket_namespace_name();
    for entry in fs::read_dir(&namespace.pin.path)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_none_or(|name| uuid::Uuid::parse_str(name).is_err())
        {
            continue;
        }
        let root = match DirectoryPin::open(entry.path()) {
            Ok(root) => root,
            Err(error) => {
                tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "foreign-directory", %error, "native runtime retained");
                continue;
            }
        };
        let owner = match private_file(&root.path.join("owner"), false) {
            Ok(owner) => {
                if owner.try_lock_exclusive().is_err() {
                    tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "live-owner", "native runtime retained");
                    continue;
                }
                Some(owner)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        let state = match read_runtime_state(&root.path) {
            Ok(state) if state.kind == RUNTIME_MARKER => Some(state),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            _ => {
                tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "unknown-state", "native runtime retained");
                continue;
            }
        };
        if let Some(marker) = &state {
            let Some(owner) = &owner else { continue };
            let metadata = owner.metadata()?;
            if metadata.dev() != marker.owner_device || metadata.ino() != marker.owner_inode {
                tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "replaced-owner", "native runtime retained");
                continue;
            }
        }
        let group = match runtime_group(&root.path, state.as_ref()) {
            Ok(group) => group,
            Err(error)
                if state.is_none()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
                    ) =>
            {
                0
            }
            Err(_) => continue,
        };
        // A close-fds wrapper can discard the inherited liveness reference.
        if group != 0 && unsafe { libc::kill(-group, 0) } == 0 {
            if !matches!(
                crate::process::runner_group_executing(group as u32),
                Ok(false)
            ) {
                tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "live-group", "native runtime retained");
                continue;
            }
        } else if group != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            continue;
        }
        if let Some(marker) = state {
            if marker.socket_namespace.file_name() != Some(OsStr::new(&socket_namespace_name))
                || marker.socket_root.parent() != Some(marker.socket_namespace.as_path())
                || marker.socket_root.file_name() != root.path.file_name()
            {
                continue;
            }
            let socket_namespace = match RuntimeNamespace::open(
                marker.socket_namespace.clone(),
                false,
            ) {
                Ok(namespace) => Some(namespace),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !matches!(fs::symlink_metadata(&marker.socket_namespace), Err(error) if error.kind() == io::ErrorKind::NotFound)
                    {
                        tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "missing-namespace-lock", "native runtime retained");
                        continue;
                    }
                    None
                }
                Err(_) => continue,
            };
            if let Some(socket_namespace) = socket_namespace {
                let metadata = socket_namespace.pin.file.metadata()?;
                if metadata.dev() != marker.socket_namespace_device
                    || metadata.ino() != marker.socket_namespace_inode
                {
                    continue;
                }
                match DirectoryPin::open(marker.socket_root.clone()) {
                    Ok(socket) => {
                        if marker.phase == RuntimePhase::Ready {
                            if verify_socket_runtime(&marker, &socket.file.metadata()?).is_err() {
                                continue;
                            }
                        } else {
                            match private_file(&marker.socket_root.join("owner"), false) {
                                Ok(mut token) => {
                                    let mut bytes = Vec::new();
                                    token.read_to_end(&mut bytes)?;
                                    if bytes != marker.token.as_bytes() {
                                        continue;
                                    }
                                }
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                Err(_) => continue,
                            }
                        }
                        socket.verify()?;
                        fs::remove_dir_all(&socket.path)?;
                        socket_namespace.pin.file.sync_all()?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => continue,
                }
            }
        }
        namespace.verify()?;
        root.verify()?;
        fs::remove_dir_all(&root.path)?;
        namespace.pin.file.sync_all()?;
        tracing::debug!(target: "git.native_hooks", phase = "sweep", reason = "inactive-owned-allocation", "native runtime recovered");
    }
    Ok(())
}

fn verify_socket_runtime(marker: &RuntimeMarker, metadata: &fs::Metadata) -> io::Result<()> {
    let mut token = Vec::new();
    private_file(&marker.socket_root.join("owner"), false)?.read_to_end(&mut token)?;
    if marker.phase != RuntimePhase::Ready
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
        || Some(metadata.dev()) != marker.socket_device
        || Some(metadata.ino()) != marker.socket_inode
        || token != marker.token.as_bytes()
    {
        return Err(invalid("native runtime socket ownership changed"));
    }
    Ok(())
}

fn runtime_group(root: &Path, expected: Option<&RuntimeMarker>) -> io::Result<i32> {
    let mut file = private_file(&root.join("group"), false)?;
    if let Some(expected) = expected {
        let metadata = file.metadata()?;
        if metadata.dev() != expected.group_device || metadata.ino() != expected.group_inode {
            return Err(invalid("native runtime process group marker changed"));
        }
    }
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    let group = i32::from_ne_bytes(bytes);
    if group < 0 || group == 1 {
        return Err(invalid("invalid native runtime process group"));
    }
    Ok(group)
}

fn publish_group(fd: RawFd) -> io::Result<()> {
    // SAFETY: this is the child before exec, writing its PID to our open marker.
    let group = unsafe { libc::getpid() }.to_ne_bytes();
    let written = unsafe { libc::pwrite(fd, group.as_ptr().cast(), group.len(), 0) };
    if written != group.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
fn allocation_barrier(phase: &str, root: &Path, socket: &Path) -> io::Result<()> {
    if std::env::var_os("AOE_TEST_NATIVE_ALLOCATE_PHASE").as_deref() == Some(OsStr::new(phase)) {
        let app = root
            .parent()
            .and_then(Path::parent)
            .expect("private runtime namespace");
        let ready = app.join(format!("ready-{}", std::process::id()));
        let pending = ready.with_extension("pending");
        fs::write(
            &pending,
            format!("{}\n{}", root.display(), socket.display()),
        )?;
        fs::rename(pending, ready)?;
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

fn initialize(
    cwd: &Path,
    app_dir: &Path,
) -> io::Result<(Runtime, Descriptor, PathBuf, UnixListener)> {
    let namespace = RuntimeNamespace::open(app_dir.join(RUNTIME_NAMESPACE), true)?;
    sweep_namespace(&namespace)?;
    let name = uuid::Uuid::new_v4().to_string();
    let (socket_base, socket_length) = socket_namespace_path(&name)?;
    let root = RuntimeDirectory::create(&namespace, &name)?;
    #[cfg(test)]
    allocation_barrier("root", root.path(), &socket_base.join(&name))?;
    let owner = liveness_file(private_file(&root.path().join("owner"), true)?)?;
    FileExt::lock_shared(&owner)?;
    let owner_metadata = owner.metadata()?;
    let mut group = private_file(&root.path().join("group"), true)?;
    group.write_all(&0i32.to_ne_bytes())?;
    group.sync_all()?;
    let group_metadata = group.metadata()?;
    let socket_namespace = RuntimeNamespace::open(socket_base.clone(), true).map_err(|error| {
        invalid(format!(
            "native socket base {} (route {socket_length} bytes) cannot allocate: {error}",
            socket_base.display()
        ))
    })?;
    let socket_namespace_metadata = socket_namespace.pin.file.metadata()?;
    let mut marker = RuntimeMarker {
        kind: RUNTIME_MARKER.into(),
        phase: RuntimePhase::Allocating,
        owner_device: owner_metadata.dev(),
        owner_inode: owner_metadata.ino(),
        group_device: group_metadata.dev(),
        group_inode: group_metadata.ino(),
        socket_root: socket_namespace.pin.path.join(&name),
        socket_namespace: socket_namespace.pin.path.clone(),
        socket_namespace_device: socket_namespace_metadata.dev(),
        socket_namespace_inode: socket_namespace_metadata.ino(),
        socket_device: None,
        socket_inode: None,
        token: uuid::Uuid::new_v4().to_string(),
    };
    publish_runtime_state(&root.pin, &marker)?;
    let socket_root = RuntimeDirectory::create(&socket_namespace, &name)?;
    #[cfg(test)]
    allocation_barrier("socket", root.path(), socket_root.path())?;
    let socket_directory = socket_root.pin.file.metadata()?;
    let mut socket_owner = private_file(&socket_root.path().join("owner"), true)?;
    socket_owner.write_all(marker.token.as_bytes())?;
    socket_owner.sync_all()?;
    #[cfg(test)]
    allocation_barrier("socket-owned", root.path(), socket_root.path())?;
    marker.phase = RuntimePhase::Ready;
    marker.socket_device = Some(socket_directory.dev());
    marker.socket_inode = Some(socket_directory.ino());
    publish_runtime_state(&root.pin, &marker)?;
    let socket = socket_root.path().join("control");
    let listener = UnixListener::bind(&socket).map_err(|error| {
        invalid(format!(
            "native socket {} ({socket_length} bytes) cannot bind: {error}",
            socket.display()
        ))
    })?;
    let socket_metadata = fs::symlink_metadata(&socket)?;
    let real_git = which::which("git").map_err(invalid)?;
    let mut command = Command::new(&real_git);
    command.current_dir(cwd).arg("--exec-path");
    let real_exec = PathBuf::from(OsString::from_vec(trim_line(checked_output(&mut command)?)));
    let proxy_exec = root.path().join("exec");
    fs::create_dir(&proxy_exec)?;
    for entry in fs::read_dir(&real_exec)? {
        let entry = entry?;
        if entry.file_name() != "git" {
            symlink(entry.path(), proxy_exec.join(entry.file_name()))?;
        }
    }
    let program = helper_program(root.path())?;
    symlink(program, proxy_exec.join("git"))?;
    let root_metadata = root.pin.file.metadata()?;
    let descriptor = Descriptor {
        scope: Scope {
            root: root.path().to_path_buf(),
            root_device: root_metadata.dev(),
            root_inode: root_metadata.ino(),
            real_git,
            real_exec,
            proxy_exec,
            program_prefix: format!("aoe-native-{}", uuid::Uuid::new_v4()),
            socket,
            socket_device: socket_metadata.dev(),
            socket_inode: socket_metadata.ino(),
            owner_device: owner_metadata.dev(),
            owner_inode: owner_metadata.ino(),
            capability: uuid::Uuid::new_v4().to_string(),
        },
        hooks: PathBuf::new(),
        original_hooks: None,
        commands: Vec::new(),
        shell: PathBuf::new(),
        parameters_present: std::env::var_os("GIT_CONFIG_PARAMETERS").is_some(),
    };
    let path = root.path().join("scope.json");
    fs::write(&path, serde_json::to_vec(&descriptor).map_err(invalid)?)?;
    tracing::debug!(target: "git.native_hooks", phase = "allocate", socket_bytes = socket_length, "native runtime ready");
    Ok((
        Runtime {
            _root: root,
            owner: Some(owner),
            _socket_root: socket_root,
            group,
            namespace_pins: [namespace.pin, socket_namespace.pin],
        },
        descriptor,
        path,
        listener,
    ))
}

pub(crate) fn run(cwd: &Path, args: &[OsString], lease: &mut dyn HookLease) -> io::Result<Output> {
    Ok(run_inner(cwd, args, lease, None)?.expect("native Git without a deadline cannot expire"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static MUTATION_TIMEOUT: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
    static EXHAUST_ACCEPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct AcceptExhaustion {
    _files: Vec<fs::File>,
    limit: libc::rlimit,
}

#[cfg(test)]
impl Drop for AcceptExhaustion {
    fn drop(&mut self) {
        // SAFETY: restore this isolated test subprocess's previous descriptor limit.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.limit) };
    }
}

#[cfg(test)]
fn exhaust_accept_for_test(listener: &UnixListener) -> io::Result<Option<AcceptExhaustion>> {
    if !EXHAUST_ACCEPT.get() {
        return Ok(None);
    }
    let mut pending = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the live listener and one initialized pollfd remain valid.
    if unsafe { libc::poll(&mut pending, 1, 10_000) } != 1 || pending.revents & libc::POLLIN == 0 {
        return Err(invalid(
            "real native callback did not connect before descriptor exhaustion",
        ));
    }
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes the supplied structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getrlimit succeeded.
    let limit = unsafe { limit.assume_init() };
    let reduced = libc::rlimit {
        rlim_cur: limit.rlim_cur.min(128),
        rlim_max: limit.rlim_max,
    };
    // SAFETY: only this test supervisor, after Git was spawned, changes its soft limit.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &reduced) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut exhaustion = AcceptExhaustion {
        _files: Vec::new(),
        limit,
    };
    loop {
        match fs::File::open("/dev/null") {
            Ok(file) => exhaustion._files.push(file),
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(Some(exhaustion))
}

#[cfg(test)]
pub(crate) fn creation_test_timeout(
    args: &[OsString],
    timeout: std::time::Duration,
) -> std::time::Duration {
    MUTATION_TIMEOUT.with(|value| {
        value
            .get()
            .filter(|_| {
                let Ok(prefix) = global_prefix(args) else {
                    return false;
                };
                args.get(prefix).is_some_and(|arg| arg == "worktree")
                    && args.get(prefix + 1).is_some_and(|arg| arg == "add")
            })
            .unwrap_or(timeout)
    })
}

pub(crate) fn run_timed(
    cwd: &Path,
    args: &[OsString],
    lease: &mut dyn HookLease,
    timeout: std::time::Duration,
) -> io::Result<Option<Output>> {
    #[cfg(test)]
    let timeout = creation_test_timeout(args, timeout);
    run_inner(cwd, args, lease, Some(timeout))
}

fn run_inner(
    cwd: &Path,
    args: &[OsString],
    lease: &mut dyn HookLease,
    timeout: Option<std::time::Duration>,
) -> io::Result<Option<Output>> {
    let (runtime, descriptor, routing_path, listener) = initialize(cwd, lease.runtime_dir())?;
    let capability = Arc::<str>::from(descriptor.scope.capability.as_str());
    let owner_identity = (descriptor.scope.owner_device, descriptor.scope.owner_inode);
    let socket_path = descriptor.scope.socket.clone();
    let prefix_len = global_prefix(args)?;
    let eligible = hooked_command(args.get(prefix_len).map(OsString::as_os_str));
    let mut command = if eligible {
        let (descriptor, prefix, path) = descriptor_for_in(cwd, descriptor, args)?;
        configured_git(&descriptor, args, prefix, &path)?
    } else {
        routing_git(&descriptor, args, &routing_path)?
    };
    command
        .current_dir(cwd)
        .env_remove("AOE_NATIVE_GIT_PARENT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let capture = if timeout.is_some() {
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command.stdout(Stdio::from(stdout.try_clone()?));
        command.stderr(Stdio::from(stderr.try_clone()?));
        Some((stdout, stderr))
    } else {
        None
    };
    let owner = runtime
        .owner
        .as_ref()
        .expect("runtime owns its liveness lease");
    owner_hint(&mut command, owner, owner_identity);
    let fd = owner.as_raw_fd();
    let group_fd = runtime.group.as_raw_fd();
    command.process_group(0);
    // The liveness lock, never a protocol descriptor, survives supervisor death.
    unsafe {
        command.pre_exec(move || {
            inheritable(fd, true)?;
            publish_group(group_fd)
        });
    }
    let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
    let mut child = command.spawn()?;
    let mut family = NativeFamily {
        pid: child.id(),
        complete: false,
    };
    #[cfg(test)]
    let _accept_exhaustion = exhaust_accept_for_test(&listener)?;
    let (sender, receiver) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stopping);
    let reader_sender = sender.clone();
    let connections = Arc::new(parking_lot::Mutex::new(
        BTreeMap::<String, UnixStream>::new(),
    ));
    let reader_connections = Arc::clone(&connections);
    let reader_capability = capability;
    let reader = std::thread::spawn(move || {
        let mut readers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        for connection in listener.incoming() {
            if stop_reader.load(Ordering::Acquire) {
                break;
            }
            let control = match connection {
                Ok(control) => control,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue
                }
                Err(error) => {
                    let _ = reader_sender.send(Event::ListenerError(error));
                    break;
                }
            };
            let connection = match control.try_clone() {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = reader_sender.send(Event::ListenerError(error));
                    break;
                }
            };
            let id = uuid::Uuid::new_v4().to_string();
            {
                let mut connections = reader_connections.lock();
                if stop_reader.load(Ordering::Acquire) {
                    break;
                }
                connections.insert(id.clone(), connection);
            }
            let sender = reader_sender.clone();
            let capability = Arc::clone(&reader_capability);
            let connections = Arc::clone(&reader_connections);
            readers.retain(|reader| !reader.is_finished());
            let reader = std::thread::Builder::new().spawn(move || {
                let mut authenticated = false;
                let result = (|| -> io::Result<()> {
                    control.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
                    let mut input = BufReader::new(control.try_clone()?);
                    let mut line = Vec::new();
                    input.read_until(b'\n', &mut line)?;
                    let frame: Frame = serde_json::from_slice(&line)
                        .map_err(|_| invalid("invalid native callback frame"))?;
                    if !matches!(&frame, Frame::Enter { capability: supplied, .. } if supplied.as_str() == capability.as_ref()) {
                        return Err(invalid("native callback authentication failed"));
                    }
                    authenticated = true;
                    writeln!(&control, "{capability}")?;
                    control.set_read_timeout(None)?;
                    sender.send(Event::Frame { id: id.clone(), frame, control: control.try_clone()? })
                        .map_err(invalid)?;
                    line.clear();
                    if input.read_until(b'\n', &mut line)? == 0 {
                        return Err(invalid("native callback disconnected before completion"));
                    }
                    let frame = serde_json::from_slice(&line)
                        .map_err(|_| invalid("invalid native callback completion frame"))?;
                    if !matches!(frame, Frame::Done) {
                        return Err(invalid("invalid native callback completion"));
                    }
                    sender.send(Event::Frame { id: id.clone(), frame, control }).map_err(invalid)?;
                    Ok(())
                })();
                connections.lock().remove(&id);
                if let Err(error) = result {
                    let _ = sender.send(Event::ReadError { id, authenticated, error });
                }
            });
            match reader {
                Ok(reader) => readers.push(reader),
                Err(error) => {
                    let _ = reader_sender.send(Event::ListenerError(error));
                    break;
                }
            }
        }
        for reader in readers {
            let _ = reader.join();
        }
    });
    let waiter = std::thread::spawn(move || {
        let output = (|| {
            if let Some((mut stdout_file, mut stderr_file)) = capture {
                let status = child.wait()?;
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                stdout_file.rewind()?;
                stderr_file.rewind()?;
                stdout_file.read_to_end(&mut stdout)?;
                stderr_file.read_to_end(&mut stderr)?;
                Ok(Output {
                    status,
                    stdout,
                    stderr,
                })
            } else {
                child.wait_with_output()
            }
        })();
        let _ = sender.send(Event::Exit(output));
    });
    let mut callbacks = BTreeSet::new();
    let mut expired = false;
    let mut failure = None;
    let output = loop {
        let event = if let Some(deadline) = deadline.filter(|_| !expired) {
            match receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    expired = true;
                    tracing::debug!(target: "git.native_hooks", phase = "timeout", "native Git deadline expired");
                    crate::process::worker::kill_process_group(family.pid);
                    continue;
                }
                Err(error) => break Err(invalid(error)),
            }
        } else {
            match receiver.recv() {
                Ok(event) => event,
                Err(error) => break Err(invalid(error)),
            }
        };
        match event {
            Event::Frame {
                id,
                frame:
                    Frame::Enter {
                        parent,
                        cwd,
                        checkout_ready,
                        git_dir,
                        reference_transaction,
                        ..
                    },
                mut control,
            } if failure.is_none() && !expired => {
                let result = lease.pause(
                    &id,
                    Path::new(OsStr::from_bytes(&cwd)),
                    checkout_ready,
                    git_dir
                        .as_deref()
                        .map(|path| Path::new(OsStr::from_bytes(path))),
                    CallbackContext {
                        parent: parent.as_deref(),
                        reference_transaction: reference_transaction.as_deref(),
                    },
                );
                if let Err(error) = result {
                    tracing::warn!(target: "git.native_hooks", phase = "enter", %error, "native callback refused");
                    failure = Some(error);
                    crate::process::worker::kill_process_group(family.pid);
                } else {
                    tracing::debug!(target: "git.native_hooks", phase = "enter", callback = %id, parent = ?parent, active = callbacks.len() + 1, "native callback admitted");
                    callbacks.insert(id.clone());
                    if let Err(error) = writeln!(control, "{id}") {
                        failure = Some(error);
                        crate::process::worker::kill_process_group(family.pid);
                    }
                }
            }
            Event::Frame {
                id,
                frame: Frame::Done,
                mut control,
            } if failure.is_none() && !expired => {
                let result = if !callbacks.contains(&id) {
                    Err(invalid("native callback completion is not active"))
                } else {
                    lease.resume(Some(&id))
                };
                if let Err(error) = result {
                    tracing::warn!(target: "git.native_hooks", phase = "continue", %error, "native continuation refused");
                    failure = Some(error);
                    crate::process::worker::kill_process_group(family.pid);
                } else {
                    callbacks.remove(&id);
                    tracing::debug!(target: "git.native_hooks", phase = "continue", callback = %id, active = callbacks.len(), "native callback completed");
                    if !callbacks.is_empty() {
                        if let Err(error) = lease.pause_retained() {
                            failure = Some(error);
                            crate::process::worker::kill_process_group(family.pid);
                            continue;
                        }
                    }
                    if let Err(error) = control.write_all(&[1]) {
                        failure = Some(error);
                        crate::process::worker::kill_process_group(family.pid);
                    }
                }
            }
            Event::ReadError {
                id,
                authenticated,
                error,
            } if failure.is_none() && !expired => {
                // An unknown connector cannot cancel this owned Git family.
                if authenticated || callbacks.contains(&id) {
                    tracing::warn!(target: "git.native_hooks", phase = "transport", %error, "authenticated native callback connection failed");
                    failure = Some(error);
                    crate::process::worker::kill_process_group(family.pid);
                }
            }
            Event::ListenerError(error) if failure.is_none() && !expired => {
                tracing::warn!(target: "git.native_hooks", phase = "listener", %error, "native callback listener failed");
                failure = Some(error);
                crate::process::worker::kill_process_group(family.pid);
            }
            Event::Exit(output) => break output,
            _ => {}
        }
    };
    if output.is_err() {
        crate::process::worker::kill_process_group(family.pid);
    }
    stopping.store(true, Ordering::Release);
    for connection in connections.lock().values() {
        let _ = connection.shutdown(std::net::Shutdown::Both);
    }
    let _ = UnixStream::connect(socket_path);
    let _ = waiter.join();
    if reader.join().is_err() && failure.is_none() && !expired {
        failure = Some(invalid("native Git control reader panicked"));
    }
    if !expired {
        for event in receiver.try_iter() {
            let error = match event {
                Event::ReadError {
                    id,
                    authenticated,
                    error,
                } if authenticated || callbacks.contains(&id) => Some(error),
                Event::Frame { .. } => Some(invalid("native Git exited with a pending callback")),
                Event::ListenerError(error) => Some(error),
                Event::ReadError { .. } | Event::Exit(_) => None,
            };
            if failure.is_none() {
                failure = error;
            }
        }
    }
    if failure.is_some() {
        crate::process::worker::kill_process_group(family.pid);
    }
    family.complete = true;
    if !callbacks.is_empty() && failure.is_none() {
        failure = if expired {
            lease.resume(None).err()
        } else {
            Some(invalid(
                "native hook exited before its lease was reacquired",
            ))
        };
    }
    if let Some(error) = failure {
        Err(error)
    } else if expired {
        output?;
        Ok(None)
    } else {
        output.map(|output| {
            tracing::debug!(target: "git.native_hooks", phase = "exit", status = ?output.status.code(), "native Git completed");
            Some(output)
        })
    }
}

fn descriptor_for_in(
    cwd: &Path,
    descriptor: Descriptor,
    args: &[OsString],
) -> io::Result<(Descriptor, usize, PathBuf)> {
    // Probes use -C rather than changing the process-wide working directory.
    let mut with_cwd = Vec::with_capacity(args.len() + 2);
    with_cwd.extend([OsString::from("-C"), cwd.as_os_str().to_owned()]);
    with_cwd.extend_from_slice(args);
    let (descriptor, prefix, path) = descriptor_for(descriptor, &with_cwd)?;
    Ok((descriptor, prefix - 2, path))
}

fn load_descriptor() -> io::Result<Descriptor> {
    let path = std::env::var_os(CONTEXT)
        .ok_or_else(|| invalid("native Git gateway context is missing"))?;
    serde_json::from_slice(&fs::read(path)?).map_err(|_| invalid("invalid native Git descriptor"))
}

fn is_post_checkout(args: &[OsString]) -> bool {
    args.len() == 3
        && [40, 64].contains(&args[0].as_bytes().len())
        && args[..2].iter().all(|arg| {
            arg.as_bytes().len() == args[0].as_bytes().len()
                && arg.as_bytes().iter().all(u8::is_ascii_hexdigit)
        })
        && (args[2] == "0" || args[2] == "1")
}

fn send_frame(control: &mut UnixStream, frame: &Frame) -> io::Result<()> {
    serde_json::to_writer(&mut *control, frame).map_err(invalid)?;
    control.write_all(b"\n")?;
    let mut ack = [0];
    control.read_exact(&mut ack)?;
    if ack == [1] {
        Ok(())
    } else {
        Err(invalid("native Git continuation was refused"))
    }
}

fn connect_callback(descriptor: &Descriptor) -> io::Result<UnixStream> {
    let metadata = fs::symlink_metadata(&descriptor.scope.socket)?;
    if !std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type())
        || metadata.dev() != descriptor.scope.socket_device
        || metadata.ino() != descriptor.scope.socket_inode
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(invalid("native callback endpoint changed"));
    }
    let control = UnixStream::connect(&descriptor.scope.socket)?;
    control.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    Ok(control)
}

fn enter_callback(
    control: &mut UnixStream,
    descriptor: &Descriptor,
    checkout_ready: bool,
    reference_transaction: Option<&str>,
) -> io::Result<String> {
    let frame = Frame::Enter {
        capability: descriptor.scope.capability.clone(),
        parent: std::env::var("AOE_NATIVE_GIT_PARENT").ok(),
        reference_transaction: reference_transaction.map(str::to_owned),
        cwd: std::env::current_dir()?.into_os_string().into_vec(),
        checkout_ready,
        git_dir: std::env::var_os("GIT_DIR").map(OsString::into_vec),
    };
    serde_json::to_writer(&mut *control, &frame).map_err(invalid)?;
    control.write_all(b"\n")?;
    let mut input = BufReader::new(control);
    let mut ack = String::new();
    input.read_line(&mut ack)?;
    if ack.trim_end() != descriptor.scope.capability {
        return Err(invalid("native callback capability admission failed"));
    }
    input.get_ref().set_read_timeout(None)?;
    ack.clear();
    input.read_line(&mut ack)?;
    let id = ack.trim_end();
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(invalid("invalid native callback acknowledgement"));
    }
    Ok(id.to_string())
}

fn configure_callback(
    command: &mut Command,
    descriptor: &Descriptor,
    callback: &str,
) -> io::Result<()> {
    let context =
        std::env::var_os(CONTEXT).ok_or_else(|| invalid("native callback context is missing"))?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(descriptor.scope.proxy_exec.clone()).chain(std::env::split_paths(&path)),
    )
    .map_err(invalid)?;
    command
        .env(CONTEXT, context)
        .env_remove(OWNER_FD)
        .env("AOE_NATIVE_GIT_PARENT", callback)
        .env(REAL_EXEC, &descriptor.scope.real_exec)
        .env(PROXY_EXEC, &descriptor.scope.proxy_exec)
        .env(PROGRAM_PREFIX, &descriptor.scope.program_prefix)
        .env(
            PARAMETERS_PRESENT,
            if descriptor.parameters_present {
                "1"
            } else {
                "0"
            },
        )
        .env("GIT_EXEC_PATH", &descriptor.scope.proxy_exec)
        .env("PATH", path);
    Ok(())
}

fn hook_command(
    descriptor: &Descriptor,
    original: &OsStr,
    args: &[OsString],
    configured: bool,
) -> io::Result<Command> {
    let shell = configured
        && original
            .as_bytes()
            .iter()
            .any(|byte| b"|&;<>()$`\\\"' \t\n*?[#~=%".contains(byte));
    let mut command = if shell {
        let mut command = Command::new(&descriptor.shell);
        let mut script = original.as_bytes().to_vec();
        if !args.is_empty() {
            script.extend_from_slice(b" \"$@\"");
        }
        command
            .arg("-c")
            .arg(OsString::from_vec(script))
            .arg(original);
        command
    } else {
        Command::new(original)
    };
    command.args(args);
    env_delta(&mut command, descriptor)?;
    Ok(command)
}

fn warn_ignored_hook(descriptor: &Descriptor, original: &OsStr) -> io::Result<()> {
    let enabled = |command: &mut Command| -> io::Result<bool> {
        let output = command
            .args(["config", "--type=bool", "--get", "advice.ignoredHook"])
            .output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(invalid(String::from_utf8_lossy(&output.stderr)));
        }
        Ok(!output.status.success() || trim_line(output.stdout) != b"false")
    };
    if std::env::var_os("GIT_ADVICE").is_some() {
        let mut command = original_command(descriptor, &[])?;
        command.arg("--config-env=advice.ignoredHook=GIT_ADVICE");
        if !enabled(&mut command)? {
            return Ok(());
        }
    }
    if !enabled(&mut original_command(descriptor, &[])?)? {
        return Ok(());
    }
    let mut stderr = io::stderr().lock();
    stderr.write_all(b"hint: The '")?;
    stderr.write_all(original.as_bytes())?;
    stderr.write_all(b"' hook was ignored because it's not set as executable.\nhint: You can disable this warning with `git config advice.ignoredHook false`.\n")
}

fn run_hook(descriptor: &Descriptor, argv: &[OsString]) -> io::Result<i32> {
    let program = Path::new(&argv[0])
        .file_name()
        .ok_or_else(|| invalid("native hook has no program name"))?;
    let args = &argv[1..];
    let configured = descriptor
        .commands
        .iter()
        .find(|hook| OsStr::new(&hook.program) == program);
    let (original, post_checkout, reference_transaction) = match configured {
        Some(hook) => (
            OsString::from_vec(hook.command.clone()),
            hook.post_checkout,
            hook.reference_transaction,
        ),
        None => {
            let hooks = match &descriptor.original_hooks {
                Some(hooks) => hooks.clone(),
                None => trim_line(probe(
                    descriptor,
                    &[],
                    &["rev-parse", "--git-path", "hooks"],
                )?),
            };
            (
                Path::new(OsStr::from_bytes(&hooks))
                    .join(program)
                    .into_os_string(),
                program == "post-checkout",
                program == "reference-transaction",
            )
        }
    };
    if configured.is_none() && !executable(Path::new(&original))? {
        if Path::new(&original).exists() {
            warn_ignored_hook(descriptor, &original)?;
        }
        return Ok(0);
    }
    let checkout_ready = post_checkout && is_post_checkout(args);
    let mut control = connect_callback(descriptor)?;
    let phase = reference_transaction
        .then(|| args.first().and_then(|arg| arg.to_str()))
        .flatten()
        .filter(|_| args.len() == 1);
    let callback = enter_callback(&mut control, descriptor, checkout_ready, phase)?;
    let mut command = hook_command(descriptor, &original, args, configured.is_some())?;
    configure_callback(&mut command, descriptor, &callback)?;
    let status = match command.status() {
        Ok(status) => Some(status),
        Err(error) if error.raw_os_error() == Some(libc::ENOEXEC) => {
            let mut fallback = Command::new(&descriptor.shell);
            fallback.arg(&original).args(args);
            env_delta(&mut fallback, descriptor)?;
            configure_callback(&mut fallback, descriptor, &callback)?;
            Some(fallback.status()?)
        }
        Err(error) => {
            let mut stderr = io::stderr().lock();
            stderr.write_all(b"error: cannot run ")?;
            stderr.write_all(original.as_bytes())?;
            writeln!(stderr, ": {error}")?;
            None
        }
    };
    send_frame(&mut control, &Frame::Done)?;
    Ok(status.map_or(255, |status| {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }))
}

fn global_exec_path_action(args: &[OsString], prefix: usize) -> bool {
    let mut cursor = 0;
    while cursor < prefix {
        if args[cursor] == "--exec-path" {
            return true;
        }
        cursor += if [
            "-C",
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--config-env",
        ]
        .iter()
        .any(|option| args[cursor] == *option)
        {
            2
        } else {
            1
        };
    }
    false
}

fn proxy_git(descriptor: Descriptor, args: &[OsString], owner: &fs::File) -> io::Result<i32> {
    let owner_identity = (descriptor.scope.owner_device, descriptor.scope.owner_inode);
    let prefix = global_prefix(args)?;
    let name = args.get(prefix).map(OsString::as_os_str);
    if global_exec_path_action(args, prefix) {
        io::stdout().write_all(descriptor.scope.proxy_exec.as_os_str().as_bytes())?;
        io::stdout().write_all(b"\n")?;
        return Ok(0);
    }
    let eligible = hooked_command(name);
    let mut command = if eligible {
        let (descriptor, prefix, path) = descriptor_for(descriptor, args)?;
        configured_git(&descriptor, args, prefix, &path)?
    } else {
        let path = std::env::var_os(CONTEXT)
            .ok_or_else(|| invalid("native Git routing context is missing"))?;
        routing_git(&descriptor, args, Path::new(&path))?
    };
    owner_hint(&mut command, owner, owner_identity);
    inheritable(owner.as_raw_fd(), true)?;

    Err(command.exec())
}

/// Handle native Git helpers before initializing a binary's CLI or runtime.
/// Call before starting threads or the CLI/runtime.
pub fn try_proxy_entry() -> Option<io::Result<i32>> {
    let Some(context) = std::env::var_os(CONTEXT) else {
        if let Err(error) = close_inherited_owner() {
            return Some(Err(error));
        }
        // SAFETY: this entrypoint runs before threads or configuration.
        unsafe { std::env::remove_var(OWNER_FD) };
        return None;
    };
    let argv: Vec<OsString> = std::env::args_os().collect();
    let result = proxy_entry(&argv);
    if result.is_none() {
        if let Err(error) = clear_normal_routing(Path::new(&context)) {
            return Some(Err(error));
        }
    }
    result
}

fn context_root(context: &Path) -> Option<&Path> {
    match context.file_name()?.to_str()? {
        "scope.json" => context.parent(),
        "descriptor.json" => context.parent()?.parent(),
        _ => None,
    }
}

fn clear_normal_routing(context: &Path) -> io::Result<()> {
    close_inherited_owner()?;
    let parameters = match std::env::var_os("GIT_CONFIG_PARAMETERS") {
        Some(value) => match (
            context_root(context),
            std::env::var_os(PROGRAM_PREFIX),
            std::env::var_os(PARAMETERS_PRESENT),
        ) {
            (Some(root), Some(prefix), Some(present)) => {
                if prefix.is_empty() || !root.is_absolute() {
                    return Err(invalid("invalid native Git routing identity"));
                }
                let present = match present.as_bytes() {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(invalid("invalid native Git parameter provenance")),
                };
                Some(strip_parameters(
                    value.as_bytes(),
                    root,
                    prefix.as_bytes(),
                    present,
                )?)
            }
            _ => None,
        },
        None => None,
    };
    let proxy = std::env::var_os(PROXY_EXEC)
        .map(PathBuf::from)
        .or_else(|| context_root(context).map(|root| root.join("exec")));
    let real_exec = std::env::var_os(REAL_EXEC);
    let path = std::env::var_os("PATH").and_then(|path| {
        proxy.as_ref().map(|proxy| {
            std::env::join_paths(std::env::split_paths(&path).filter(|entry| entry != proxy))
                .expect("split PATH entries contain no separator")
        })
    });
    // SAFETY: entry runs before any threads. Ordinary AoE children must start
    // independent Git supervisors, not reenter their caller's paused lease.
    unsafe {
        if let Some(parameters) = parameters {
            match parameters {
                Some(value) => std::env::set_var("GIT_CONFIG_PARAMETERS", value),
                None => std::env::remove_var("GIT_CONFIG_PARAMETERS"),
            }
        }
        if let Some(path) = path {
            std::env::set_var("PATH", path);
        }
        if let Some(real_exec) = real_exec {
            std::env::set_var("GIT_EXEC_PATH", real_exec);
        } else if proxy.as_ref().is_some_and(|proxy| {
            std::env::var_os("GIT_EXEC_PATH").as_deref() == Some(proxy.as_os_str())
        }) {
            std::env::remove_var("GIT_EXEC_PATH");
        }
        for (key, _) in std::env::vars_os() {
            if key.as_bytes().starts_with(PRIVATE_PREFIX.as_bytes()) {
                std::env::remove_var(key);
            }
        }
    }
    Ok(())
}

fn proxy_entry(argv: &[OsString]) -> Option<io::Result<i32>> {
    let argument = argv.first()?;
    let program = Path::new(argument).file_name()?;
    let invoked = if Path::new(argument).components().count() == 1 {
        which::which(argument).ok()?
    } else if Path::new(argument).is_absolute() {
        PathBuf::from(argument)
    } else {
        std::env::current_dir().ok()?.join(argument)
    };
    let context = PathBuf::from(std::env::var_os(CONTEXT)?);
    let root = context_root(&context)?;
    // Route only our physical helper paths. Ordinary renamed/symlinked AoE
    // invocations must never read an inherited, possibly stale descriptor.
    let proxy = invoked.parent() == Some(root.join("exec").as_path())
        && (program == "git" || program.as_bytes().starts_with(b"aoe-native-"));
    let hook = context.file_name() == Some(OsStr::new("descriptor.json"))
        && invoked.parent()
            == context
                .parent()
                .map(|parent| parent.join("hooks"))
                .as_deref();
    if !invoked.is_absolute() || (!proxy && !hook) {
        return None;
    }
    let descriptor = match load_descriptor() {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "aoe native Git helper: {error}");
            return Some(Err(error));
        }
    };
    let hook = program
        .as_bytes()
        .starts_with(descriptor.scope.program_prefix.as_bytes())
        || invoked.parent() == Some(descriptor.hooks.as_path());
    if program != "git" && !hook {
        return None;
    }
    let owned_group = (|| -> io::Result<Option<u32>> {
        let metadata = fs::symlink_metadata(&descriptor.scope.root)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
            || metadata.dev() != descriptor.scope.root_device
            || metadata.ino() != descriptor.scope.root_inode
        {
            return Ok(None);
        }
        let marker = read_runtime_state(&descriptor.scope.root)?;
        let owner = private_file(&descriptor.scope.root.join("owner"), false)?.metadata()?;
        if owner.dev() != descriptor.scope.owner_device
            || owner.ino() != descriptor.scope.owner_inode
            || owner.dev() != marker.owner_device
            || owner.ino() != marker.owner_inode
        {
            return Ok(None);
        }
        let group = unsafe { libc::getpgrp() };
        Ok((marker.kind == RUNTIME_MARKER
            && marker.phase == RuntimePhase::Ready
            && group > 1
            && runtime_group(&descriptor.scope.root, Some(&marker))? == group)
            .then_some(group as u32))
    })()
    .ok()
    .flatten();
    let result = (|| {
        let owner = helper_owner(&descriptor.scope)?;
        if program == "git" {
            proxy_git(descriptor, &argv[1..], &owner)
        } else {
            run_hook(&descriptor, argv)
        }
    })();
    if let Err(error) = &result {
        tracing::warn!(target: "git.native_hooks", phase = "helper", kind = ?error.kind(), "native Git helper failed");
        let _ = writeln!(io::stderr().lock(), "aoe native Git helper: {error}");
    }
    if result.is_err() {
        if let Some(group) = owned_group {
            crate::process::worker::kill_process_group(group);
        }
    }
    Some(result)
}

#[cfg(not(test))]
fn helper_program(_: &Path) -> io::Result<PathBuf> {
    crate::process::running_image()
}

#[cfg(test)]
fn helper_program(root: &Path) -> io::Result<PathBuf> {
    use crate::session::environment::shell_escape;
    let path = root.join("helper");
    if !path.exists() {
        let exe = crate::process::running_image()?;
        let stdout_fd = 9;
        let script = format!("#!/bin/sh\nargs=$(mktemp {}/argv.XXXXXX) || exit 1\nprintf '%s\\0' \"$0\" \"$@\" >\"$args\"\nexec {stdout_fd}>&1\nexport AOE_NATIVE_GIT_TEST_ARGS=\"$args\" AOE_NATIVE_GIT_TEST_STDOUT={stdout_fd}\nexec {} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture >/dev/null\n", shell_escape(&root.to_string_lossy()), shell_escape(&exe.to_string_lossy()));
        fs::write(&path, script)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_callbacks_reenter_ownership() {
        if let Some(path) = std::env::var_os("AOE_NATIVE_GIT_TEST_ARGS") {
            let stdout_fd: RawFd = std::env::var("AOE_NATIVE_GIT_TEST_STDOUT")
                .unwrap()
                .parse()
                .unwrap();
            // SAFETY: the fixture helper preserves native stdout separately from libtest output.
            assert_eq!(
                unsafe { libc::dup2(stdout_fd, libc::STDOUT_FILENO) },
                libc::STDOUT_FILENO
            );
            // SAFETY: stdout now owns the duplicated descriptor.
            unsafe {
                libc::close(stdout_fd);
            }
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(path).unwrap();
            let argv: Vec<_> = bytes
                .strip_suffix(&[0])
                .unwrap_or(&bytes)
                .split(|byte| *byte == 0)
                .map(|arg| OsString::from_vec(arg.to_vec()))
                .collect();
            if std::env::var_os("AOE_TEST_NATIVE_CLOSE_FDS").is_some() {
                let mut command =
                    Command::new(std::env::var_os("AOE_TEST_NATIVE_REAL_GIT").unwrap());
                command
                    .args(&argv[1..])
                    .env_remove("AOE_TEST_NATIVE_CLOSE_FDS")
                    .env_remove("AOE_TEST_CLOSE_FDS_ONCE");
                let descriptors: Vec<i32> = fs::read_dir("/dev/fd")
                    .unwrap()
                    .filter_map(|entry| {
                        entry
                            .ok()?
                            .file_name()
                            .to_str()?
                            .parse()
                            .ok()
                            .filter(|fd| *fd > 2)
                    })
                    .collect();
                for fd in descriptors {
                    // SAFETY: this isolated helper is about to exec real Git.
                    unsafe {
                        libc::close(fd);
                    }
                }
                fs::write(
                    std::env::var_os("AOE_TEST_NATIVE_FDS_CLOSED").unwrap(),
                    "closed",
                )
                .unwrap();
                panic!(
                    "real Git exec failed after closing descriptors: {}",
                    command.exec()
                );
            }
            let result = proxy_entry(&argv).expect("native helper invocation");
            std::process::exit(result.unwrap());
        }
        if std::env::var_os("AOE_TEST_NATIVE_MUTATOR").is_some() {
            let source = std::env::var("AOE_TEST_NATIVE_PROFILE").unwrap();
            let action = std::env::var("AOE_TEST_NATIVE_ACTION").unwrap();
            let cwd = std::env::current_dir().unwrap();
            if action == "recover" {
                let before = crate::session::move_journal::scan();
                assert_eq!(
                    before.entries.len(),
                    1,
                    "the live rename has one canonical intent"
                );
                let path = before.entries[0].0.clone();
                let bytes = fs::read(&path).unwrap();
                assert!(crate::session::Storage::recover_pending_moves(
                    crate::file_watch::FileWatchService::noop()
                )
                .unwrap()
                .is_empty());
                assert_eq!(
                    fs::read(&path).unwrap(),
                    bytes,
                    "recovery must not consume or rewrite its live caller"
                );
                fs::write(
                    std::env::var_os("AOE_TEST_NATIVE_MARKER").unwrap(),
                    "active intent retained",
                )
                .unwrap();
                return;
            }
            if action == "replace-result-ref" {
                let repo = git2::Repository::open(&cwd).unwrap();
                let reference = repo.commondir().join("refs/heads/new");
                let bytes = fs::read(&reference).unwrap();
                let saved = PathBuf::from(std::env::var_os("AOE_TEST_NATIVE_MARKER").unwrap())
                    .with_extension("original-ref");
                fs::rename(&reference, saved).unwrap();
                fs::write(reference, bytes).unwrap();
                return;
            }
            if matches!(
                action.as_str(),
                "adopt" | "adopt-rename" | "workspace-adopt" | "reuse"
            ) {
                let target = if action == "reuse" {
                    PathBuf::from("/unrelated")
                } else if action == "adopt-rename" {
                    PathBuf::from(std::env::var_os("AOE_TEST_NATIVE_TARGET").unwrap())
                } else if action == "workspace-adopt" {
                    let target = PathBuf::from(std::env::var_os("AOE_TEST_NATIVE_TARGET").unwrap());
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if git2::Repository::open(&target)
                            .is_ok_and(|repo| repo.path().join("locked").exists())
                        {
                            break;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "first worker did not finish its checkout"
                        );
                        std::thread::yield_now();
                    }
                    target
                } else {
                    cwd
                };
                let storage = crate::session::Storage::open_unwatched(&source).unwrap();
                let root = crate::session::storage::acquire_ownership_lock().unwrap();
                let mut row = crate::session::Instance::new("adopter", target.to_str().unwrap());
                if action == "reuse" {
                    row.id = std::env::var("AOE_TEST_NATIVE_ID").unwrap();
                }
                storage
                    .update_with_ownership(&root, |rows, _| {
                        rows.push(row);
                        Ok(())
                    })
                    .unwrap();
            } else {
                if matches!(action.as_str(), "commit" | "detached-commit") {
                    fs::write(cwd.join("tracked"), "callback").unwrap();
                    for args in [["add", "tracked"], ["commit", "-mcallback"]] {
                        assert!(Command::new("git").args(args).status().unwrap().success());
                    }
                } else if action == "switch" {
                    assert!(Command::new("git")
                        .args(["symbolic-ref", "HEAD", "refs/heads/main"])
                        .status()
                        .unwrap()
                        .success());
                } else if action == "dirty" {
                    fs::write(cwd.join("untracked"), "callback").unwrap();
                }
                if action != "switch" {
                    crate::session::rename_profile(&source, &format!("{source}-renamed")).unwrap();
                }
            }
            return;
        }
        if std::env::var_os("AOE_TEST_NATIVE_RUNNER").is_none() {
            // Child commands must not inherit another test's temporary process environment.
            let (environment, phase_root) = {
                let _env = crate::session::test_support::EnvGuard::read_lock();
                (
                    std::env::vars_os().collect::<Vec<_>>(),
                    tempfile::tempdir().unwrap(),
                )
            };
            for phase in [
                "callbacks",
                "protocol",
                "dynamic",
                "remote",
                "workspace",
                "replaced",
            ] {
                eprintln!("native callbacks: starting {phase}");
                let started = std::time::Instant::now();
                let private_image = tempfile::tempdir_in(phase_root.path()).unwrap();
                let executable = if phase == "replaced" && cfg!(target_os = "linux") {
                    let image = private_image.path().join("test-image");
                    fs::copy(std::env::current_exe().unwrap(), &image).unwrap();
                    image
                } else {
                    std::env::current_exe().unwrap()
                };
                let mut child = Command::new(executable);
                child
                    .env_clear()
                    .envs(environment.iter().map(|(key, value)| (key, value)))
                    .env("TMPDIR", private_image.path());
                if phase == "replaced" {
                    child.env("AOE_TEST_NATIVE_REPLACED_IMAGE", "1");
                }
                child
                    .args([
                        "--exact",
                        "git::native_hooks::tests::native_callbacks_reenter_ownership",
                        "--nocapture",
                    ])
                    .env(
                        "AOE_TEST_NATIVE_RUNNER",
                        if phase == "replaced" {
                            "callbacks"
                        } else {
                            phase
                        },
                    );
                let output = crate::process::run_with_timeout_process_group(
                    &mut child,
                    std::time::Duration::from_secs(30),
                )
                .unwrap()
                .unwrap_or_else(|| panic!("native callback phase {phase} exceeded its deadline"));
                assert!(
                    output.status.success(),
                    "{phase}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                eprintln!(
                    "native callbacks: {phase} completed in {:?}",
                    started.elapsed()
                );
            }
            return;
        }
        let phase = std::env::var("AOE_TEST_NATIVE_RUNNER").unwrap();
        #[cfg(target_os = "linux")]
        if std::env::var_os("AOE_TEST_NATIVE_REPLACED_IMAGE").is_some() {
            let image = std::env::current_exe().unwrap();
            let replacement = image.with_extension("replacement");
            fs::write(&replacement, "#!/bin/sh\nexit 97\n").unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
            fs::rename(replacement, &image).unwrap();
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("source");
        fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "fixture"],
            vec!["config", "user.email", "fixture@example.invalid"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success());
        }
        fs::write(repo.join("tracked"), "base\n").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-m", "base"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        let exe = crate::process::running_image().unwrap();
        let git = crate::git::GitWorktree::new(repo.clone())
            .unwrap()
            .with_init_submodules(false);
        if phase == "protocol" {
            use crate::session::environment::shell_escape;
            let other = fixture.path().join("other");
            assert!(Command::new("git")
                .arg("clone")
                .arg(&repo)
                .arg(&other)
                .status()
                .unwrap()
                .success());
            let owner = crate::session::Storage::new_unwatched("protocol-owner").unwrap();
            let real_git = which::which("git").unwrap();
            let bin = fixture.path().join("bin");
            fs::create_dir(&bin).unwrap();
            let wrapper = bin.join("git");
            fs::write(&wrapper, "#!/bin/sh\nif [ \"$AOE_TEST_CLOSE_FDS_ONCE\" = 1 ] && [ -n \"$AOE_NATIVE_GIT_CONTEXT\" ]; then\n case \" $* \" in *\" worktree add \"*) export AOE_TEST_NATIVE_CLOSE_FDS=1; exec \"$GIT_EXEC_PATH/git\" \"$@\";; esac\nfi\nexec \"$AOE_TEST_NATIVE_REAL_GIT\" \"$@\"\n").unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
            for mode in [
                "long",
                "nested",
                "parallel",
                "argv",
                "closed-fds",
                "parallel-configured",
                "late-enter",
                "parent-done",
                "listener-exhausted",
            ] {
                let marker = fixture.path().join(format!("{mode}-completed"));
                let source_entered = fixture.path().join("entered-source");
                let other_entered = fixture.path().join("entered-other");
                let ordered = matches!(mode, "late-enter" | "parent-done");
                let order_root = fixture.path().join(mode);
                let reference_hook = repo.join(".git/hooks/reference-transaction");
                let child_effect = order_root.join("child-effect");
                if ordered {
                    fs::create_dir(&order_root).unwrap();
                    assert!(Command::new("mkfifo")
                        .args(
                            ["start", "entered", "release", "done"]
                                .map(|name| order_root.join(name))
                        )
                        .status()
                        .unwrap()
                        .success());
                    let start = if mode == "late-enter" {
                        format!(
                            "IFS= read -r go < {} || exit 99\n",
                            shell_escape(&order_root.join("start").to_string_lossy())
                        )
                    } else {
                        String::new()
                    };
                    let wait = if mode == "parent-done" {
                        format!(
                            "IFS= read -r go < {} || exit 99\n",
                            shell_escape(&order_root.join("entered").to_string_lossy())
                        )
                    } else {
                        String::new()
                    };
                    fs::write(&reference_hook, format!(
                        "#!/bin/sh\n[ \"$1\" = prepared ] || exit 0\n[ ! -e {started} ] || exit 0\n: > {started}\n(\n{start}unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE\nexport AOE_TEST_CALLBACK_DEPTH=1\ngit -C {other} checkout -- tracked\nstatus=$?\nprintf '%s\\n' \"$status\" > {done}\nexit \"$status\"\n) &\n{wait}exit 0\n",
                        started = shell_escape(&order_root.join("parent-started").to_string_lossy()),
                        other = shell_escape(&other.to_string_lossy()),
                        done = shell_escape(&order_root.join("done").to_string_lossy()),
                    )).unwrap();
                    fs::set_permissions(&reference_hook, fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let body = match mode {
                    "long" => format!("printf entered > {}\nsleep 0.1 || exit 91\n", shell_escape(&fixture.path().join("long-entered").to_string_lossy())),
                    "nested" => format!("if [ \"$AOE_TEST_CALLBACK_DEPTH\" != 1 ]; then\n export AOE_TEST_CALLBACK_DEPTH=1\n unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE\n git -C {} checkout -- tracked || exit 92\nfi\n", shell_escape(&repo.to_string_lossy())),
                    "parallel" => format!(
                        "if [ \"$AOE_TEST_CALLBACK_DEPTH\" = 1 ]; then\n : > {root}/entered-$(basename \"$PWD\")\n n=0\n while [ ! -e {source_entered} ] || [ ! -e {other_entered} ]; do\n n=$((n + 1)); [ \"$n\" -lt 200 ] || exit 93\n sleep 0.005\n done\nelse\n export AOE_TEST_CALLBACK_DEPTH=1\n unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE\n git -C {source} checkout -- tracked & a=$!\n git -C {other} checkout -- tracked & b=$!\n wait \"$a\" || exit 94\n wait \"$b\" || exit 95\nfi\n",
                        root = shell_escape(&fixture.path().to_string_lossy()),
                        source_entered = shell_escape(&source_entered.to_string_lossy()),
                        other_entered = shell_escape(&other_entered.to_string_lossy()),
                        source = shell_escape(&repo.to_string_lossy()),
                        other = shell_escape(&other.to_string_lossy()),
                    ),
                    "argv" => "test -z \"$(git log --format=%s --grep --exec-path)\" || exit 96\ntest \"$(git --exec-path)\" = \"$GIT_EXEC_PATH\" || exit 97\n".to_string(),
                    "closed-fds" => format!("test -e {} || exit 98\n", shell_escape(&fixture.path().join("fds-closed").to_string_lossy())),
                    "late-enter" | "parent-done" => {
                        let child_gate = if mode == "parent-done" {
                            format!("printf 'entered\\n' > {}\nIFS= read -r go < {} || exit 99\n", shell_escape(&order_root.join("entered").to_string_lossy()), shell_escape(&order_root.join("release").to_string_lossy()))
                        } else { String::new() };
                        let signal = order_root.join(if mode == "late-enter" { "start" } else { "release" });
                        format!(
                            "if [ \"$AOE_TEST_CALLBACK_DEPTH\" = 1 ]; then\n{child_gate}printf child > {effect}\nexit 0\nelse\nprintf 'go\\n' > {signal}\nIFS= read -r status < {done} || exit 99\n[ \"$status\" = 0 ] || exit 99\nfi\n",
                            effect = shell_escape(&child_effect.to_string_lossy()),
                            signal = shell_escape(&signal.to_string_lossy()),
                            done = shell_escape(&order_root.join("done").to_string_lossy()),
                        )
                    }
                    "parallel-configured" => {
                        let source_command = format!("hook.race.command=printf source > {}", shell_escape(&source_entered.to_string_lossy()));
                        let other_command = format!("hook.race.command=printf other > {}", shell_escape(&other_entered.to_string_lossy()));
                        format!("if [ \"$AOE_TEST_CALLBACK_DEPTH\" = 1 ]; then exit 0; fi\nexport AOE_TEST_CALLBACK_DEPTH=1\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE\ngit -C {source} -c hook.race.event=post-checkout -c {source_command} checkout -- tracked & a=$!\ngit -C {other} -c hook.race.event=post-checkout -c {other_command} checkout -- tracked & b=$!\nwait \"$a\" || exit 94\nwait \"$b\" || exit 95\n",
                            source = shell_escape(&repo.to_string_lossy()),
                            other = shell_escape(&other.to_string_lossy()),
                            source_command = shell_escape(&source_command),
                            other_command = shell_escape(&other_command))
                    }
                    "listener-exhausted" => String::new(),
                    _ => unreachable!(),
                };
                let script = format!(
                    "#!/bin/sh\n{body}printf complete > {}\n",
                    shell_escape(&marker.to_string_lossy())
                );
                for source in [&repo, &other] {
                    let hook = source.join(".git/hooks/post-checkout");
                    fs::write(&hook, &script).unwrap();
                    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
                }
                let target = fixture.path().join(format!("checkout-{mode}"));
                let branch = format!("topic-{mode}");
                let routing = if mode == "closed-fds" {
                    let path = std::env::var_os("PATH").unwrap_or_default();
                    let path = std::env::join_paths(
                        std::iter::once(bin.clone()).chain(std::env::split_paths(&path)),
                    )
                    .unwrap();
                    Some(crate::session::test_support::EnvGuard::set(&[
                        ("PATH", path),
                        ("AOE_TEST_CLOSE_FDS_ONCE", OsString::from("1")),
                        ("AOE_TEST_NATIVE_REAL_GIT", real_git.as_os_str().to_owned()),
                        (
                            "AOE_TEST_NATIVE_FDS_CLOSED",
                            fixture.path().join("fds-closed").into_os_string(),
                        ),
                    ]))
                } else {
                    None
                };
                let previous = MUTATION_TIMEOUT
                    .replace((mode == "long").then_some(std::time::Duration::from_millis(40)));
                EXHAUST_ACCEPT.set(mode == "listener-exhausted");
                let mut ownership =
                    Some(crate::session::storage::acquire_ownership_read().unwrap());
                let result = crate::session::native_hooks::create_worktree(
                    &mut ownership,
                    Some(&owner),
                    &git,
                    (&repo, &target, &branch),
                    true,
                    Some("main"),
                    None,
                );
                MUTATION_TIMEOUT.set(previous);
                EXHAUST_ACCEPT.set(false);
                drop(routing);
                if mode == "listener-exhausted" {
                    let error = match result {
                        Err(error) => error,
                        Ok(_) => panic!("descriptor exhaustion must fail the supervisor"),
                    };
                    assert_eq!(
                        error
                            .chain()
                            .find_map(|cause| cause.downcast_ref::<io::Error>())
                            .and_then(io::Error::raw_os_error),
                        Some(libc::EMFILE)
                    );
                    assert!(!marker.exists(), "unadmitted user hook must not execute");
                    continue;
                }
                let (_, witness) = result.unwrap_or_else(|error| panic!("{mode}: {error:#}"));
                witness.verify().unwrap();
                assert_eq!(fs::read(&marker).unwrap(), b"complete", "{mode}");
                assert!(target.join("tracked").exists(), "{mode}");
                assert_eq!(
                    crate::git::GitWorktree::get_current_branch(&target).unwrap(),
                    branch
                );
                if mode == "long" {
                    assert_eq!(
                        fs::read(fixture.path().join("long-entered")).unwrap(),
                        b"entered"
                    );
                }
                if mode == "parallel" {
                    assert!(source_entered.exists() && other_entered.exists());
                }
                if mode == "parallel-configured" {
                    assert_eq!(fs::read(&source_entered).unwrap(), b"source");
                    assert_eq!(fs::read(&other_entered).unwrap(), b"other");
                }
                if ordered {
                    assert_eq!(fs::read(&child_effect).unwrap(), b"child");
                    fs::remove_file(&reference_hook).unwrap();
                }
            }
            return;
        }
        if phase == "callbacks" {
            let mut previous_hook = None;
            for (hook_name, action) in [
                ("post-checkout", "rename"),
                ("post-checkout", "owner"),
                ("post-checkout", "adopt"),
                ("post-checkout", "commit"),
                ("post-checkout", "detached-commit"),
                ("post-checkout", "detached-rename"),
                ("post-checkout", "dirty"),
                ("post-checkout", "preexisting"),
                ("post-index-change", "rename"),
                ("reference-transaction", "rename"),
                ("post-index-change", "adopt"),
                ("post-index-change", "commit"),
                ("post-index-change", "switch"),
            ] {
                if let Some(previous) = previous_hook.take() {
                    fs::remove_file(previous).unwrap();
                }
                let case = format!("{hook_name}-{action}");
                let owner_name = format!("owner-{case}");
                let peer_name = format!("peer-{case}");
                let owner = crate::session::Storage::new_unwatched(&owner_name).unwrap();
                let peer = crate::session::Storage::new_unwatched(&peer_name).unwrap();
                let target = fixture.path().join(&case);
                let detached = action.starts_with("detached-");
                let branch = if detached {
                    let repository = git2::Repository::open(&repo).unwrap();
                    let name = format!("origin/topic-{case}");
                    repository
                        .reference(
                            &format!("refs/remotes/{name}"),
                            repository.head().unwrap().target().unwrap(),
                            false,
                            "fixture",
                        )
                        .unwrap();
                    name
                } else {
                    format!("topic-{case}")
                };
                let hook = repo.join(".git/hooks").join(hook_name);
                if action == "switch" {
                    let repository = git2::Repository::open(&repo).unwrap();
                    repository
                        .branch(
                            &branch,
                            &repository.head().unwrap().peel_to_commit().unwrap(),
                            false,
                        )
                        .unwrap();
                }
                if action == "preexisting" {
                    git.create_worktree(&branch, &target, true, Some("main"))
                        .unwrap();
                }
                let profile = if matches!(
                    action,
                    "owner" | "commit" | "dirty" | "detached-commit" | "detached-rename"
                ) {
                    &owner_name
                } else {
                    &peer_name
                };
                let marker = fixture.path().join(format!("entered-{case}"));
                let phase = if hook_name == "reference-transaction" {
                    "[ \"$1\" = prepared ] || exit 0\n"
                } else {
                    ""
                };
                fs::write(&hook, format!(
                "#!/bin/sh\n{phase}[ ! -e {marker} ] || exit 0\n: > {marker}\nexport AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE={profile} AOE_TEST_NATIVE_ACTION={action}\nexec {exe} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture\n",
                marker = crate::session::environment::shell_escape(&marker.to_string_lossy()),
                exe = crate::session::environment::shell_escape(&exe.to_string_lossy()),
            )).unwrap();
                fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
                previous_hook = Some(hook);
                let mut ownership =
                    Some(crate::session::storage::acquire_ownership_read().unwrap());
                let result = crate::session::native_hooks::create_worktree(
                    &mut ownership,
                    Some(&owner),
                    &git,
                    (&repo, &target, &branch),
                    action != "switch" && !detached,
                    Some("main"),
                    None,
                );
                if action == "rename" {
                    result.unwrap().1.verify().unwrap();
                    assert!(crate::session::Storage::open_unwatched(&format!(
                        "{peer_name}-renamed"
                    ))
                    .is_ok());
                    assert!(crate::session::Storage::open_unwatched(&peer_name).is_err());
                } else {
                    assert!(
                        result.is_err(),
                        "{action} must invalidate creation authority"
                    );
                    if action == "adopt" {
                        assert_eq!(
                            fs::canonicalize(&peer.load().unwrap()[0].project_path).unwrap(),
                            fs::canonicalize(&target).unwrap()
                        );
                        assert!(target.join("tracked").exists());
                    } else if action == "commit" {
                        let repository = git2::Repository::open(&repo).unwrap();
                        let commit = repository
                            .find_branch(&branch, git2::BranchType::Local)
                            .unwrap()
                            .get()
                            .peel_to_commit()
                            .unwrap();
                        let entry = commit.tree().unwrap().get_name("tracked").unwrap().id();
                        assert_eq!(repository.find_blob(entry).unwrap().content(), b"callback");
                    } else if action == "detached-commit" {
                        let repository = git2::Repository::open(&target).unwrap();
                        assert!(repository.head_detached().unwrap());
                        let commit = repository.head().unwrap().peel_to_commit().unwrap();
                        let entry = commit.tree().unwrap().get_name("tracked").unwrap().id();
                        assert_eq!(repository.find_blob(entry).unwrap().content(), b"callback");
                    } else if action == "detached-rename" {
                        assert!(!target.exists());
                        assert!(git
                            .list_worktrees()
                            .unwrap()
                            .iter()
                            .all(|worktree| worktree.path != target));
                    } else if action == "dirty" {
                        assert_eq!(fs::read(target.join("untracked")).unwrap(), b"callback");
                    } else if action == "preexisting" {
                        assert!(target.join("tracked").exists());
                        assert!(git2::Repository::open(&repo)
                            .unwrap()
                            .find_branch(&branch, git2::BranchType::Local)
                            .is_ok());
                    } else if action == "switch" {
                        assert_eq!(
                            git2::Repository::open(&target)
                                .unwrap()
                                .head()
                                .unwrap()
                                .name()
                                .unwrap(),
                            "refs/heads/main"
                        );
                    }
                }
            }
            fs::remove_file(previous_hook.unwrap()).unwrap();
            return;
        }
        if phase == "dynamic" {
            for (case, active) in [
                ("installed", true),
                ("enabled", true),
                ("removed", false),
                ("disabled", false),
            ] {
                let owner =
                    crate::session::Storage::new_unwatched(&format!("owner-dynamic-{case}"))
                        .unwrap();
                let peer_name = format!("peer-dynamic-{case}");
                crate::session::Storage::new_unwatched(&peer_name).unwrap();
                let checkout_hook = repo.join(".git/hooks/post-checkout");
                let index_hook = repo.join(".git/hooks/post-index-change");
                let quote = crate::session::environment::shell_escape;
                let payload = format!("#!/bin/sh\nexport AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE={peer_name} AOE_TEST_NATIVE_ACTION=rename\nexec {} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture\n", quote(&exe.to_string_lossy()));
                if case != "installed" {
                    fs::write(&checkout_hook, &payload).unwrap();
                    fs::set_permissions(
                        &checkout_hook,
                        fs::Permissions::from_mode(if case == "enabled" { 0o600 } else { 0o700 }),
                    )
                    .unwrap();
                }
                let effect = match case {
                    "installed" => format!(
                        "printf '%s' {} > {}; chmod 700 {}",
                        crate::session::environment::shell_escape_script_word(&payload),
                        quote(&checkout_hook.to_string_lossy()),
                        quote(&checkout_hook.to_string_lossy())
                    ),
                    "enabled" => format!("chmod 700 {}", quote(&checkout_hook.to_string_lossy())),
                    "removed" => format!("rm -f {}", quote(&checkout_hook.to_string_lossy())),
                    _ => format!("chmod 600 {}", quote(&checkout_hook.to_string_lossy())),
                };
                fs::write(&index_hook, format!("#!/bin/sh\n{effect}\n")).unwrap();
                fs::set_permissions(&index_hook, fs::Permissions::from_mode(0o700)).unwrap();
                let target = fixture.path().join(format!("dynamic-{case}"));
                let mut ownership =
                    Some(crate::session::storage::acquire_ownership_read().unwrap());
                let (warnings, _) = crate::session::native_hooks::create_worktree(
                    &mut ownership,
                    Some(&owner),
                    &git,
                    (&repo, &target, &format!("dynamic-{case}")),
                    true,
                    Some("main"),
                    None,
                )
                .unwrap();
                drop(ownership);
                assert_eq!(
                    crate::session::Storage::open_unwatched(&peer_name).is_ok(),
                    !active,
                    "{case}: {warnings:?}; payload={:?}",
                    fs::read_to_string(&checkout_hook)
                );
                assert_eq!(
                    crate::session::Storage::open_unwatched(&format!("{peer_name}-renamed"))
                        .is_ok(),
                    active,
                    "{case}: {warnings:?}"
                );
                assert_eq!(fs::read(target.join("tracked")).unwrap(), b"base\n");
                fs::remove_file(index_hook).unwrap();
                if checkout_hook.exists() {
                    fs::remove_file(checkout_hook).unwrap();
                }
            }
            return;
        }
        if phase == "remote" {
            let repository = git2::Repository::open(&repo).unwrap();
            let tip = repository.head().unwrap().target().unwrap();
            repository.remote("origin", repo.to_str().unwrap()).unwrap();
            repository
                .reference("refs/remotes/origin/remote-only", tip, false, "fixture")
                .unwrap();
            assert!(repository
                .find_branch("remote-only", git2::BranchType::Local)
                .is_err());
            let target = fixture.path().join("remote-only");
            let owner = crate::session::Storage::new_unwatched("owner-remote").unwrap();
            let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
            crate::session::native_hooks::create_worktree(
                &mut ownership,
                Some(&owner),
                &git,
                (&repo, &target, "remote-only"),
                false,
                None,
                None,
            )
            .unwrap()
            .1
            .verify()
            .unwrap();
            assert_eq!(
                git2::Repository::open(&target)
                    .unwrap()
                    .head()
                    .unwrap()
                    .name()
                    .unwrap(),
                "refs/heads/remote-only"
            );
            repository
                .reference("refs/remotes/origin/main", tip, false, "fixture")
                .unwrap();
            let detached = fixture.path().join("detached-remote");
            crate::session::native_hooks::create_worktree(
                &mut ownership,
                Some(&owner),
                &git,
                (&repo, &detached, "origin/main"),
                false,
                None,
                None,
            )
            .unwrap()
            .1
            .verify()
            .unwrap();
            assert!(git2::Repository::open(&detached)
                .unwrap()
                .head_detached()
                .unwrap());
            repository.remote_delete("origin").unwrap();
            return;
        }
        use crate::session::builder::{create_workspace_with_ownership, WorkspaceRepoSpec};
        let extra = fixture.path().join("extra");
        assert!(Command::new("git")
            .args([OsStr::new("clone"), repo.as_os_str(), extra.as_os_str()])
            .status()
            .unwrap()
            .success());
        let workspace_owner = crate::session::Storage::new_unwatched("owner-workspace").unwrap();
        let adopter = crate::session::Storage::new_unwatched("peer-workspace").unwrap();
        let workspace = fixture.path().join("workspace/workspace-adopt");
        let target = workspace.join("source");
        fs::write(repo.join(".git/hooks/post-checkout"), "#!/bin/sh\nexit 0\n").unwrap();
        let extra_hook = extra.join(".git/hooks/post-checkout");
        fs::write(&extra_hook, format!("#!/bin/sh\nexport AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE=peer-workspace AOE_TEST_NATIVE_ACTION=workspace-adopt AOE_TEST_NATIVE_TARGET={}\nexec {} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture\n", crate::session::environment::shell_escape(&target.to_string_lossy()), crate::session::environment::shell_escape(&exe.to_string_lossy()))).unwrap();
        fs::set_permissions(&extra_hook, fs::Permissions::from_mode(0o700)).unwrap();
        let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
        let result = create_workspace_with_ownership(
            &WorkspaceRepoSpec {
                path: repo.clone(),
                base_branch: Some("main".to_owned()),
            },
            &[WorkspaceRepoSpec {
                path: extra.clone(),
                base_branch: Some("main".to_owned()),
            }],
            "workspace-adopt",
            true,
            fixture.path().join("workspace/{branch}").to_str().unwrap(),
            false,
            (&mut ownership, Some(&workspace_owner), None),
        );
        assert!(
            result.is_err(),
            "a peer adoption between completed workers must cancel publication"
        );
        assert_eq!(
            fs::canonicalize(&adopter.load().unwrap()[0].project_path).unwrap(),
            fs::canonicalize(&target).unwrap()
        );
        assert_eq!(fs::read(target.join("tracked")).unwrap(), b"base\n");

        fs::remove_file(repo.join(".git/hooks/post-checkout")).unwrap();
        fs::remove_file(extra_hook).unwrap();
        for args in [
            vec![
                OsString::from("config"),
                OsString::from("--global"),
                OsString::from("protocol.file.allow"),
                OsString::from("always"),
            ],
            vec![
                OsString::from("submodule"),
                OsString::from("add"),
                extra.into_os_string(),
                OsString::from("module"),
            ],
            vec![
                OsString::from("commit"),
                OsString::from("-am"),
                OsString::from("submodule"),
            ],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success());
        }
        let child = fixture.path().join("extra");
        assert!(Command::new("git")
            .args(["symbolic-ref", "HEAD", "refs/heads/missing"])
            .current_dir(&child)
            .status()
            .unwrap()
            .success());
        let hooks = fixture.path().join("submodule-hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("post-checkout");
        fs::write(&hook, format!("#!/bin/sh\ncase \"$PWD\" in\n*/module) export AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE=peer-submodule AOE_TEST_NATIVE_ACTION=rename\nexec {} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture;;\nesac\n", crate::session::environment::shell_escape(&exe.to_string_lossy()))).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(Command::new("git")
            .args([
                OsStr::new("config"),
                OsStr::new("--global"),
                OsStr::new("core.hooksPath"),
                hooks.as_os_str()
            ])
            .status()
            .unwrap()
            .success());
        let owner = crate::session::Storage::new_unwatched("owner-submodule").unwrap();
        crate::session::Storage::new_unwatched("peer-submodule").unwrap();
        crate::session::Storage::new_unwatched("peer-submodule-clone").unwrap();
        let reference_hook = hooks.join("reference-transaction");
        let clone_entered = fixture.path().join("submodule-clone-entered");
        let quote = crate::session::environment::shell_escape;
        fs::write(&reference_hook, format!("#!/bin/sh\n[ \"$1\" = prepared ] || exit 0\ncase \"$GIT_DIR\" in */modules/module) [ ! -f {marker} ] || exit 0; : > {marker}\nexport AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE=peer-submodule-clone AOE_TEST_NATIVE_ACTION=rename\nexec {exe} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture;; esac\n", marker=quote(&clone_entered.to_string_lossy()),exe=quote(&exe.to_string_lossy()))).unwrap();
        fs::set_permissions(&reference_hook, fs::Permissions::from_mode(0o700)).unwrap();
        let configured_marker = fixture.path().join("configured-hook-supported");
        assert!(Command::new("git")
            .args([
                OsStr::new("-c"),
                OsStr::new("hook.probe.event=reference-transaction"),
                OsStr::new("-c"),
                OsStr::new(&format!(
                    "hook.probe.command=printf yes > {}",
                    quote(&configured_marker.to_string_lossy())
                )),
                OsStr::new("update-ref"),
                OsStr::new("refs/aoe-hook-probe"),
                OsStr::new("HEAD"),
            ])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        let configured = configured_marker.exists();
        let template = fixture.path().join("configured-template");
        fs::create_dir(&template).unwrap();
        let configured_entered = fixture.path().join("submodule-configured-entered");
        if configured {
            crate::session::Storage::new_unwatched("peer-submodule-configured").unwrap();
            let command = format!("case \"$GIT_DIR:$1\" in */modules/module:prepared) [ ! -f {marker} ] || exit 0; : > {marker}; export AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE=peer-submodule-configured AOE_TEST_NATIVE_ACTION=rename; exec {exe} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture;; *) exit 0;; esac", marker=quote(&configured_entered.to_string_lossy()),exe=quote(&exe.to_string_lossy()));
            let command = format!(
                "sh -c {} --",
                crate::session::environment::shell_escape_script_word(&command)
            );
            for (key, value) in [
                ("hook.clone-fixture.event", "reference-transaction"),
                ("hook.clone-fixture.command", command.as_str()),
            ] {
                assert!(Command::new("git")
                    .args([
                        OsStr::new("config"),
                        OsStr::new("--file"),
                        template.join("config").as_os_str(),
                        OsStr::new(key),
                        OsStr::new(value)
                    ])
                    .status()
                    .unwrap()
                    .success());
            }
        }
        let _template_env = configured.then(|| {
            crate::session::test_support::EnvGuard::set(&[("GIT_TEMPLATE_DIR", &template)])
        });
        let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
        let target = fixture.path().join("submodule-checkout");
        let git = crate::git::GitWorktree::new(repo.clone()).unwrap();
        crate::session::native_hooks::create_worktree(
            &mut ownership,
            Some(&owner),
            &git,
            (&repo, &target, "submodule-checkout"),
            true,
            Some("main"),
            None,
        )
        .unwrap()
        .1
        .verify()
        .unwrap();
        assert_eq!(fs::read(target.join("module/tracked")).unwrap(), b"base\n");
        assert!(crate::session::Storage::open_unwatched("peer-submodule-renamed").is_ok());
        assert!(clone_entered.exists());
        assert!(crate::session::Storage::open_unwatched("peer-submodule-clone-renamed").is_ok());
        if configured {
            assert!(configured_entered.exists());
            assert!(
                crate::session::Storage::open_unwatched("peer-submodule-configured-renamed")
                    .is_ok()
            );
        }
    }

    #[test]
    fn runtime_cleanup_reclaims_only_dead_owned_invocations() {
        if let Some(app) = std::env::var_os("AOE_TEST_NATIVE_RUNTIME_OWNER") {
            let app = PathBuf::from(app);
            let (runtime, _, _, _listener) = initialize(&app, &app).unwrap();
            std::os::unix::fs::FileExt::write_all_at(
                &runtime.group,
                &(std::process::id() as i32).to_ne_bytes(),
                0,
            )
            .unwrap();
            let ready = app.join(format!("ready-{}", std::process::id()));
            let pending = ready.with_extension("pending");
            fs::write(
                &pending,
                format!(
                    "{}\n{}",
                    runtime._root.path().display(),
                    runtime._socket_root.path().display()
                ),
            )
            .unwrap();
            fs::rename(pending, ready).unwrap();
            loop {
                std::thread::park();
            }
        }
        struct Owner(std::process::Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn start(app: &Path, phase: &str, temp: Option<&Path>) -> (Owner, PathBuf, PathBuf) {
            let child = Command::new(crate::process::running_image().unwrap())
                .args(["--exact", "git::native_hooks::tests::runtime_cleanup_reclaims_only_dead_owned_invocations", "--nocapture"])
                .env("AOE_TEST_NATIVE_ALLOCATE_PHASE", phase)
                .envs(temp.map(|path| ("TMPDIR", path)))
                .env("AOE_TEST_NATIVE_RUNTIME_OWNER", app).process_group(0)
                .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let mut owner = Owner(child);
            let ready = app.join(format!("ready-{}", owner.0.id()));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !ready.exists() {
                assert!(
                    owner.0.try_wait().unwrap().is_none(),
                    "runtime owner exited before publishing its marker"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "runtime owner did not initialize"
                );
                std::thread::yield_now();
            }
            let routes = fs::read_to_string(ready).unwrap();
            let (root, socket) = routes.split_once('\n').unwrap();
            (owner, root.into(), socket.into())
        }
        let fixture = tempfile::tempdir().unwrap();
        let app = fixture.path().join("a".repeat(120));
        fs::create_dir(&app).unwrap();
        let (mut stale, stale_root, stale_socket) = start(&app, "", None);
        let (active, active_root, active_socket) = start(&app, "", None);
        let foreign = app.join("native-git-foreign");
        fs::create_dir(&foreign).unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(foreign.join("sentinel"), "keep").unwrap();
        stale.0.kill().unwrap();
        stale.0.wait().unwrap();
        let (_next, _, _, _) = initialize(&app, &app).unwrap();
        assert!(!stale_root.exists() && !stale_socket.exists());
        assert!(active_root.exists() && active_socket.exists());
        assert_eq!(fs::read(foreign.join("sentinel")).unwrap(), b"keep");
        for phase in ["root", "socket", "socket-owned"] {
            let (mut interrupted, root, socket) = start(&app, phase, None);
            assert!(root.exists(), "{phase}");
            assert_eq!(socket.exists(), phase != "root", "{phase}");
            interrupted.0.kill().unwrap();
            interrupted.0.wait().unwrap();
            let (_next, _, _, _) = initialize(&app, &app).unwrap();
            assert!(!root.exists() && !socket.exists(), "{phase}");
            assert!(active_root.exists() && active_socket.exists(), "{phase}");
            assert_eq!(fs::read(foreign.join("sentinel")).unwrap(), b"keep");
        }
        let short_temp = tempfile::tempdir_in("/tmp").unwrap();
        let long_temp = fixture.path().join("t".repeat(120));
        fs::create_dir(&long_temp).unwrap();
        for temp in [short_temp.path(), long_temp.as_path()] {
            let (routed, root, socket) = start(&app, "", Some(temp));
            assert_eq!(socket.starts_with(temp), temp == short_temp.path());
            // SAFETY: only the initialized array's length is inspected.
            let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            assert!(socket.join("control").as_os_str().len() < address.sun_path.len());
            let _connection = UnixStream::connect(socket.join("control")).unwrap();
            drop(routed);
            sweep_runtime(&app).unwrap();
            assert!(!root.exists() && !socket.exists());
            assert!(active_root.exists() && active_socket.exists());
            assert_eq!(fs::read(foreign.join("sentinel")).unwrap(), b"keep");
        }
        drop(active);
        sweep_runtime(&app).unwrap();
        assert!(!active_root.exists() && !active_socket.exists());
    }

    #[test]
    fn callback_refuses_a_replaced_foreign_socket_without_connecting() {
        let app = tempfile::tempdir().unwrap();
        let (_runtime, descriptor, _, listener) = initialize(app.path(), app.path()).unwrap();
        let foreign_path = descriptor.scope.socket.with_extension("foreign");
        let foreign = UnixListener::bind(&foreign_path).unwrap();
        fs::remove_file(&descriptor.scope.socket).unwrap();
        fs::rename(foreign_path, &descriptor.scope.socket).unwrap();
        drop(listener);
        foreign.set_nonblocking(true).unwrap();
        assert!(connect_callback(&descriptor).is_err());
        assert_eq!(
            foreign.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn ordinary_entry_removes_only_its_inherited_git_parameters() {
        const CHILD: &str = "AOE_TEST_NATIVE_NORMAL_ENTRY";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(crate::process::running_image().unwrap())
                .args(["--exact", "git::native_hooks::tests::ordinary_entry_removes_only_its_inherited_git_parameters", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let context = root.path().join("node/descriptor.json");
        let proxy = root.path().join("exec");
        let prefix = "aoe-native-precise-fixture";
        let injected = format!(
            "'core.hooksPath={}/node/hooks' 'hook.post-checkout.command={prefix}-post-checkout'",
            root.path().display()
        )
        .into_bytes();
        let user_before = b"'fixture.duplicate=first' 'hook.external.command=aoe-native-foreign' 'core.hooksPath=/foreign/hooks'";
        let user_after = b" 'fixture.duplicate=last' 'fixture.bytes=can'\\''t \xff'";
        for (present, with_user) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut parameters = Vec::new();
            let mut expected = Vec::new();
            if with_user {
                parameters.extend_from_slice(user_before);
                parameters.push(b' ');
                expected.extend_from_slice(user_before);
            }
            parameters.extend_from_slice(&injected);
            if with_user {
                parameters.extend_from_slice(user_after);
                expected.extend_from_slice(user_after);
            }
            let path = std::env::join_paths(std::iter::once(proxy.clone()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))
            .unwrap();
            let _env = crate::session::test_support::EnvGuard::set(&[
                (CONTEXT, context.clone().into_os_string()),
                (PROXY_EXEC, proxy.clone().into_os_string()),
                (REAL_EXEC, OsString::from("/real/git-exec")),
                (PROGRAM_PREFIX, OsString::from(prefix)),
                (
                    PARAMETERS_PRESENT,
                    OsString::from(if present { "1" } else { "0" }),
                ),
                ("GIT_EXEC_PATH", proxy.clone().into_os_string()),
                ("PATH", path),
                ("GIT_CONFIG_PARAMETERS", OsString::from_vec(parameters)),
            ]);
            assert!(!context.exists());
            assert!(try_proxy_entry().is_none());
            assert_eq!(
                std::env::var_os("GIT_CONFIG_PARAMETERS"),
                (present || with_user).then(|| OsString::from_vec(expected))
            );
            assert!(std::env::var_os(CONTEXT).is_none());
            assert_eq!(
                std::env::var_os("GIT_EXEC_PATH"),
                Some(OsString::from("/real/git-exec"))
            );
        }
        let malformed = OsString::from("'unterminated");
        let _env = crate::session::test_support::EnvGuard::set(&[
            (CONTEXT, context.clone().into_os_string()),
            (PROGRAM_PREFIX, OsString::from(prefix)),
            (PARAMETERS_PRESENT, OsString::from("0")),
            ("GIT_CONFIG_PARAMETERS", malformed.clone()),
        ]);
        assert!(try_proxy_entry().unwrap().is_err());
        assert_eq!(std::env::var_os(CONTEXT), Some(context.into_os_string()));
        assert_eq!(std::env::var_os("GIT_CONFIG_PARAMETERS"), Some(malformed));
    }

    #[test]
    fn ordinary_entry_releases_only_the_verified_inherited_owner_reference() {
        use std::os::fd::IntoRawFd;
        for (replace, has_context) in [(false, true), (true, true), (false, false), (true, false)] {
            let app = tempfile::tempdir().unwrap();
            let (runtime, descriptor, context, _listener) =
                initialize(app.path(), app.path()).unwrap();
            let root = runtime._root.path().to_path_buf();
            let owner = runtime.owner.as_ref().unwrap();
            let fd = owner.try_clone().unwrap().into_raw_fd();
            let foreign_path = app.path().join("foreign");
            let mut foreign = private_file(&foreign_path, true).unwrap();
            foreign.write_all(b"before").unwrap();
            let reused = if replace {
                // SAFETY: fd is our raw owned duplicate, atomically replaced by another owned file.
                assert_eq!(unsafe { libc::dup2(foreign.as_raw_fd(), fd) }, fd);
                // SAFETY: dup2 left this new descriptor owned by the test.
                Some(unsafe { fs::File::from_raw_fd(fd) })
            } else {
                None
            };
            let _env = crate::session::test_support::EnvGuard::set(&[
                (CONTEXT, context.into_os_string()),
                (
                    OWNER_FD,
                    OsString::from(format!(
                        "{fd}:{}:{}",
                        descriptor.scope.owner_device, descriptor.scope.owner_inode
                    )),
                ),
                (
                    PROGRAM_PREFIX,
                    OsString::from(&descriptor.scope.program_prefix),
                ),
                (PARAMETERS_PRESENT, OsString::from("0")),
                ("GIT_CONFIG_PARAMETERS", OsString::new()),
            ]);
            if !has_context {
                // SAFETY: EnvGuard serializes this test's environment changes.
                unsafe { std::env::remove_var(CONTEXT) };
            }
            assert!(try_proxy_entry().is_none());
            if let Some(mut reused) = reused {
                reused.write_all(b"after").unwrap();
                assert_eq!(fs::read(&foreign_path).unwrap(), b"beforeafter");
            }
            sweep_runtime(app.path()).unwrap();
            assert!(
                root.exists(),
                "retiring one reference must not LOCK_UN the original"
            );
            drop(runtime);
            assert!(
                !root.exists(),
                "an ordinary invocation must not retain runtime liveness"
            );
            assert_eq!(
                fs::read(&foreign_path).unwrap(),
                if replace {
                    b"beforeafter".as_slice()
                } else {
                    b"before".as_slice()
                }
            );
        }
    }

    #[test]
    fn configured_proxy_refuses_foreign_program_without_replacing_or_running_it() {
        let app = tempfile::tempdir().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
        ]);
        let repo = app.path().join("repository");
        let repository = git2::Repository::init(&repo).unwrap();
        fs::write(repo.join("tracked"), b"base").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        fs::write(repo.join("tracked"), b"user-change").unwrap();
        let (_runtime, descriptor, context, _listener) = initialize(&repo, app.path()).unwrap();
        let sentinel = app.path().join("foreign-target");
        fs::write(&sentinel, b"keep").unwrap();
        let program = descriptor
            .scope
            .proxy_exec
            .join(format!("{}-0", descriptor.scope.program_prefix));
        symlink(&sentinel, &program).unwrap();
        let before = fs::symlink_metadata(&program).unwrap();
        let effect = app.path().join("configured-effect");
        let output = Command::new(descriptor.scope.proxy_exec.join("git"))
            .current_dir(&repo)
            .args(["-c", "hook.race.event=post-checkout", "-c"])
            .arg(format!(
                "hook.race.command=printf ran > {}",
                crate::session::environment::shell_escape(&effect.to_string_lossy())
            ))
            .args(["checkout", "--", "tracked"])
            .env(CONTEXT, context)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
        assert!(!output
            .stderr
            .windows(descriptor.scope.capability.len())
            .any(|part| part == descriptor.scope.capability.as_bytes()));
        let after = fs::symlink_metadata(&program).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(fs::read_link(&program).unwrap(), sentinel);
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
        assert_eq!(fs::read(repo.join("tracked")).unwrap(), b"user-change");
        assert!(!effect.exists());
    }
}
