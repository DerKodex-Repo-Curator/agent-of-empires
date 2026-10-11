//! Preserve native hook dispatch while handing caller leases off at post-checkout.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

const CONTEXT: &str = "AOE_NATIVE_GIT_CONTEXT";
const PRIVATE_PREFIX: &str = "AOE_NATIVE_GIT_";

pub(crate) trait HookLease {
    fn runtime_dir(&self) -> &Path;
    fn pause(&mut self, cwd: &Path) -> io::Result<()>;
    fn resume(&mut self) -> io::Result<()>;
    fn checkpoint(&mut self) -> io::Result<()>;
}

#[derive(Serialize, Deserialize)]
struct Scope {
    root: PathBuf,
    real_git: PathBuf,
    real_exec: PathBuf,
    proxy_exec: PathBuf,
    program_prefix: String,
    fd: RawFd,
    owner_group: i32,
}

#[derive(Serialize, Deserialize)]
struct HookCommand {
    name: Vec<u8>,
    command: Vec<u8>,
    post_checkout: bool,
    program: String,
}

#[derive(Serialize, Deserialize)]
struct Descriptor {
    scope: Scope,
    hooks: PathBuf,
    original_hooks: Vec<u8>,
    commands: Vec<HookCommand>,
    shell: PathBuf,
    parameters_present: bool,
    oid_width: usize,
}

#[derive(Serialize, Deserialize)]
enum Frame {
    Enter(Vec<u8>),
    Done,
}

enum Event {
    Frame(Frame),
    ReadError(io::Error),
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
    // SAFETY: fcntl operates on this process's live control descriptor.
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

fn env_delta(command: &mut Command, descriptor: &Descriptor) -> io::Result<Option<OsString>> {
    let parameters = match std::env::var_os("GIT_CONFIG_PARAMETERS") {
        Some(value) => strip_parameters(
            value.as_bytes(),
            &descriptor.scope,
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

fn strip_parameters(bytes: &[u8], scope: &Scope, present: bool) -> io::Result<Option<OsString>> {
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
            (key == b"core.hooksPath"
                && Path::new(OsStr::from_bytes(value)).starts_with(&scope.root))
                || (key.starts_with(b"hook.")
                    && key.ends_with(b".command")
                    && value.starts_with(scope.program_prefix.as_bytes()))
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
    let config = probe(&descriptor, prefix, &["config", "--null", "--list"])?;
    let mut commands: BTreeMap<Vec<u8>, (Option<Vec<u8>>, bool)> = BTreeMap::new();
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
            }
        }
    }
    let node = descriptor.scope.root.join(format!(
        "node-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&node)?;
    descriptor.hooks = node.join("hooks");
    fs::create_dir(&descriptor.hooks)?;
    let mut hooks = trim_line(probe(
        &descriptor,
        prefix,
        &["rev-parse", "--git-path", "hooks"],
    )?);
    let is_worktree_add = args.get(prefix_len).is_some_and(|arg| arg == "worktree")
        && args.get(prefix_len + 1).is_some_and(|arg| arg == "add");
    if is_worktree_add && !Path::new(OsStr::from_bytes(&hooks)).is_absolute() {
        hooks = effective_cwd(prefix)?
            .join(OsStr::from_bytes(&hooks))
            .into_os_string()
            .into_vec();
    }
    descriptor.original_hooks = hooks;
    descriptor.commands.clear();
    let helper = helper_program(&descriptor.scope.root, descriptor.scope.fd)?;
    for (index, (name, (command, post_checkout))) in commands.into_iter().enumerate() {
        let Some(command) = command else {
            continue;
        };
        let program = format!("{}-{index}", descriptor.scope.program_prefix);
        let path = descriptor.scope.proxy_exec.join(&program);
        if !path.exists() {
            symlink(&helper, &path)?;
        }
        descriptor.commands.push(HookCommand {
            name,
            command,
            post_checkout,
            program,
        });
    }
    let original_hooks = effective_cwd(prefix)?.join(OsStr::from_bytes(&descriptor.original_hooks));
    match fs::read_dir(&original_hooks) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                if entry.file_type()?.is_dir() {
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
    descriptor.oid_width = match trim_line(probe(
        &descriptor,
        prefix,
        &["rev-parse", "--show-object-format"],
    )?)
    .as_slice()
    {
        b"sha1" => 40,
        b"sha256" => 64,
        _ => return Err(invalid("unsupported Git object format")),
    };
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
    Ok(command)
}

fn routing_git(descriptor: &Descriptor, args: &[OsString], path: &Path) -> io::Result<Command> {
    let mut command = original_command(descriptor, args)?;
    command
        .env("GIT_EXEC_PATH", &descriptor.scope.proxy_exec)
        .env(CONTEXT, path);
    Ok(command)
}

fn initialize(
    cwd: &Path,
    lease: &dyn HookLease,
    fd: RawFd,
) -> io::Result<(tempfile::TempDir, Descriptor, PathBuf)> {
    let root = tempfile::Builder::new()
        .prefix("native-git-")
        .tempdir_in(lease.runtime_dir())?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
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
    let program = helper_program(root.path(), fd)?;
    symlink(program, proxy_exec.join("git"))?;
    let descriptor = Descriptor {
        scope: Scope {
            root: root.path().to_path_buf(),
            real_git,
            real_exec,
            proxy_exec,
            program_prefix: format!("aoe-native-{}", uuid::Uuid::new_v4()),
            fd,
            owner_group: unsafe { libc::getpgrp() },
        },
        hooks: PathBuf::new(),
        original_hooks: Vec::new(),
        commands: Vec::new(),
        shell: PathBuf::new(),
        parameters_present: std::env::var_os("GIT_CONFIG_PARAMETERS").is_some(),
        oid_width: 0,
    };
    let path = root.path().join("scope.json");
    fs::write(&path, serde_json::to_vec(&descriptor).map_err(invalid)?)?;
    Ok((root, descriptor, path))
}

pub(crate) fn run(cwd: &Path, args: &[OsString], lease: &mut dyn HookLease) -> io::Result<Output> {
    let (mut control, child_control) = UnixStream::pair()?;
    let (_root, descriptor, routing_path) = initialize(cwd, lease, child_control.as_raw_fd())?;
    let prefix_len = global_prefix(args)?;
    let eligible = args
        .get(prefix_len)
        .is_some_and(|arg| arg == "checkout" || arg == "worktree");
    let mut command = if eligible {
        // Configuration probes must use the same cwd as this native invocation.
        let (descriptor, prefix, path) = descriptor_for_in(cwd, descriptor, args)?;
        configured_git(&descriptor, args, prefix, &path)?
    } else {
        routing_git(&descriptor, args, &routing_path)?
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let fd = child_control.as_raw_fd();
    command.process_group(0);
    // SAFETY: the child only changes a descriptor flag before exec.
    unsafe {
        command.pre_exec(move || inheritable(fd, true));
    }
    let child = command.spawn()?;
    let mut family = NativeFamily {
        pid: child.id(),
        complete: false,
    };
    drop(child_control);
    let (sender, receiver) = mpsc::channel();
    let input = control.try_clone()?;
    let reader_sender = sender.clone();
    let reader = std::thread::spawn(move || {
        let mut input = BufReader::new(input);
        let mut line = Vec::new();
        loop {
            line.clear();
            match input.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => match serde_json::from_slice(&line) {
                    Ok(frame) => {
                        if reader_sender.send(Event::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = reader_sender.send(Event::ReadError(invalid(error)));
                        break;
                    }
                },
                Err(error) => {
                    let _ = reader_sender.send(Event::ReadError(error));
                    break;
                }
            }
        }
    });
    let waiter = std::thread::spawn(move || {
        let _ = sender.send(Event::Exit(child.wait_with_output()));
    });
    let mut paused = false;
    let mut failure = None;
    let output = loop {
        match receiver.recv().map_err(invalid)? {
            Event::Frame(Frame::Enter(cwd)) if failure.is_none() => {
                let result = if paused {
                    Err(invalid("overlapping native post-checkout callbacks"))
                } else {
                    lease.pause(Path::new(OsStr::from_bytes(&cwd)))
                };
                if let Err(error) = result {
                    failure = Some(error);
                    crate::process::worker::kill_process_group(family.pid);
                } else {
                    paused = true;
                    if let Err(error) = control.write_all(&[1]) {
                        failure = Some(error);
                        crate::process::worker::kill_process_group(family.pid);
                    }
                }
            }
            Event::Frame(Frame::Done) if failure.is_none() => {
                let result = if paused {
                    lease.resume()
                } else {
                    Err(invalid("native callback completed without a handoff"))
                };
                if let Err(error) = result {
                    failure = Some(error);
                    crate::process::worker::kill_process_group(family.pid);
                } else {
                    paused = false;
                    if let Err(error) = control.write_all(&[1]) {
                        failure = Some(error);
                        crate::process::worker::kill_process_group(family.pid);
                    }
                }
            }
            Event::ReadError(error) if failure.is_none() => {
                failure = Some(error);
                crate::process::worker::kill_process_group(family.pid);
            }
            Event::Exit(output) => break output,
            _ => {}
        }
    };
    family.complete = true;
    drop(control);
    waiter
        .join()
        .map_err(|_| invalid("native Git waiter panicked"))?;
    reader
        .join()
        .map_err(|_| invalid("native Git control reader panicked"))?;
    if paused && failure.is_none() {
        failure = Some(invalid(
            "native hook exited before its lease was reacquired",
        ));
    }
    if let Some(error) = failure {
        Err(error)
    } else {
        output
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
    serde_json::from_slice(&fs::read(path)?).map_err(invalid)
}

fn is_post_checkout(args: &[OsString], width: usize) -> bool {
    args.len() == 3
        && [40, 64].contains(&width)
        && args[..2].iter().all(|arg| {
            arg.as_bytes().len() == width && arg.as_bytes().iter().all(u8::is_ascii_hexdigit)
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

fn run_hook(descriptor: &Descriptor, argv: &[OsString]) -> io::Result<i32> {
    let program = Path::new(&argv[0])
        .file_name()
        .ok_or_else(|| invalid("native hook has no program name"))?;
    let args = &argv[1..];
    let configured = descriptor
        .commands
        .iter()
        .find(|hook| OsStr::new(&hook.program) == program);
    let (original, post_checkout) = match configured {
        Some(hook) => (OsString::from_vec(hook.command.clone()), hook.post_checkout),
        None => (
            Path::new(OsStr::from_bytes(&descriptor.original_hooks))
                .join(program)
                .into_os_string(),
            program == "post-checkout",
        ),
    };
    let gated = post_checkout && is_post_checkout(args, descriptor.oid_width);
    // SAFETY: this descriptor was deliberately inherited from the supervising Git process.
    let mut control = unsafe { UnixStream::from_raw_fd(descriptor.scope.fd) };
    inheritable(control.as_raw_fd(), false)?;
    if gated {
        send_frame(
            &mut control,
            &Frame::Enter(std::env::current_dir()?.into_os_string().into_vec()),
        )?;
    }
    let mut command = hook_command(descriptor, &original, args, configured.is_some())?;
    let status = match command.status() {
        Ok(status) => Some(status),
        Err(error) if error.raw_os_error() == Some(libc::ENOEXEC) => {
            let mut fallback = Command::new(&descriptor.shell);
            fallback.arg(&original).args(args);
            env_delta(&mut fallback, descriptor)?;
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
    if gated {
        send_frame(&mut control, &Frame::Done)?;
    }
    Ok(status.map_or(255, |status| {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }))
}

fn proxy_git(descriptor: Descriptor, args: &[OsString]) -> io::Result<i32> {
    inheritable(descriptor.scope.fd, false)?;
    let prefix = global_prefix(args)?;
    let name = args.get(prefix).map(OsString::as_os_str);
    if args.iter().any(|arg| arg == "--exec-path") {
        io::stdout().write_all(descriptor.scope.proxy_exec.as_os_str().as_bytes())?;
        io::stdout().write_all(b"\n")?;
        return Ok(0);
    }
    let eligible = name.is_some_and(|name| name == "checkout" || name == "worktree");
    let routing = name.is_some_and(|name| name == "submodule" || name == "submodule--helper");
    let mut command = if eligible {
        let (descriptor, prefix, path) = descriptor_for(descriptor, args)?;
        configured_git(&descriptor, args, prefix, &path)?
    } else if routing {
        let path = std::env::var_os(CONTEXT)
            .ok_or_else(|| invalid("native Git routing context is missing"))?;
        routing_git(&descriptor, args, Path::new(&path))?
    } else {
        original_command(&descriptor, args)?
    };
    if eligible || routing {
        inheritable(descriptor_fd(&command)?, true)?;
    }
    Err(command.exec())
}

fn descriptor_fd(command: &Command) -> io::Result<RawFd> {
    let path = command
        .get_envs()
        .find(|(key, _)| *key == CONTEXT)
        .and_then(|(_, value)| value)
        .ok_or_else(|| invalid("native routing command has no context"))?;
    let descriptor: Descriptor = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
    Ok(descriptor.scope.fd)
}

/// Handle native Git helpers before initializing a binary's CLI or runtime.
/// Binaries using guarded session creation or attachment must call this at entry.
pub fn try_proxy_entry() -> Option<io::Result<i32>> {
    std::env::var_os(CONTEXT)?;
    let argv: Vec<OsString> = std::env::args_os().collect();
    proxy_entry(&argv)
}

fn proxy_entry(argv: &[OsString]) -> Option<io::Result<i32>> {
    let descriptor = match load_descriptor() {
        Ok(descriptor) => descriptor,
        Err(error) => return Some(Err(error)),
    };
    let program = Path::new(argv.first()?).file_name()?;
    let hook = program
        .as_bytes()
        .starts_with(descriptor.scope.program_prefix.as_bytes())
        || Path::new(&argv[0]).parent() == Some(descriptor.hooks.as_path());
    if program != "git" && !hook {
        return None;
    }
    let owner_group = descriptor.scope.owner_group;
    let result = if program == "git" {
        proxy_git(descriptor, &argv[1..])
    } else {
        run_hook(&descriptor, argv)
    };
    if result.is_err() {
        // A gateway error cannot fall through Git's ordinary hook-warning path.
        let group = unsafe { libc::getpgrp() };
        if group != owner_group && group > 1 {
            crate::process::worker::kill_process_group(group as u32);
        }
    }
    Some(result)
}

#[cfg(not(test))]
fn helper_program(_: &Path, _: RawFd) -> io::Result<PathBuf> {
    std::env::current_exe()
}

#[cfg(test)]
fn helper_program(root: &Path, control_fd: RawFd) -> io::Result<PathBuf> {
    use crate::session::environment::shell_escape;
    let path = root.join("helper");
    if !path.exists() {
        let exe = std::env::current_exe()?;
        let stdout_fd = if control_fd == 9 { 8 } else { 9 };
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
            let result = proxy_entry(&argv).expect("native helper invocation");
            std::process::exit(result.unwrap());
        }
        if std::env::var_os("AOE_TEST_NATIVE_MUTATOR").is_some() {
            let source = std::env::var("AOE_TEST_NATIVE_PROFILE").unwrap();
            let action = std::env::var("AOE_TEST_NATIVE_ACTION").unwrap();
            let cwd = std::env::current_dir().unwrap();
            if matches!(action.as_str(), "adopt" | "workspace-adopt") {
                let target = if action == "workspace-adopt" {
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
                let row = crate::session::Instance::new("adopter", target.to_str().unwrap());
                storage
                    .update_with_ownership(&root, |rows, _| {
                        rows.push(row);
                        Ok(())
                    })
                    .unwrap();
            } else {
                if action == "commit" {
                    fs::write(cwd.join("tracked"), "callback").unwrap();
                    for args in [["add", "tracked"], ["commit", "-mcallback"]] {
                        assert!(Command::new("git").args(args).status().unwrap().success());
                    }
                } else if action == "dirty" {
                    fs::write(cwd.join("untracked"), "callback").unwrap();
                }
                crate::session::rename_profile(&source, &format!("{source}-renamed")).unwrap();
            }
            return;
        }
        if std::env::var_os("AOE_TEST_NATIVE_RUNNER").is_none() {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "git::native_hooks::tests::native_callbacks_reenter_ownership",
                    "--nocapture",
                ])
                .env("AOE_TEST_NATIVE_RUNNER", "1");
            let output = crate::process::run_with_timeout_process_group(
                &mut child,
                std::time::Duration::from_secs(30),
            )
            .unwrap()
            .expect("native callback must not wait on its parent lease");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
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
        let exe = std::env::current_exe().unwrap();
        let git = crate::git::GitWorktree::new(repo.clone())
            .unwrap()
            .with_init_submodules(false);
        for action in ["rename", "owner", "adopt", "commit", "dirty", "preexisting"] {
            let owner_name = format!("owner-{action}");
            let peer_name = format!("peer-{action}");
            let owner = crate::session::Storage::new_unwatched(&owner_name).unwrap();
            let peer = crate::session::Storage::new_unwatched(&peer_name).unwrap();
            let target = fixture.path().join(action);
            let branch = format!("topic-{action}");
            let hook = repo.join(".git/hooks/post-checkout");
            if action == "preexisting" {
                fs::remove_file(&hook).unwrap();
                git.create_worktree(&branch, &target, true, Some("main"))
                    .unwrap();
            }
            let profile = if matches!(action, "owner" | "commit" | "dirty") {
                &owner_name
            } else {
                &peer_name
            };
            fs::write(
                &hook,
                format!(
                    "#!/bin/sh
export AOE_TEST_NATIVE_MUTATOR=1 AOE_TEST_NATIVE_PROFILE={profile} AOE_TEST_NATIVE_ACTION={action}
exec {} --exact git::native_hooks::tests::native_callbacks_reenter_ownership --nocapture
",
                    crate::session::environment::shell_escape(&exe.to_string_lossy())
                ),
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
            let mut ownership = Some(crate::session::storage::acquire_ownership_read().unwrap());
            let result = crate::session::native_hooks::create_worktree(
                &mut ownership,
                Some(&owner),
                &git,
                (&repo, &target, &branch),
                true,
                Some("main"),
            );
            if action == "rename" {
                result.unwrap().1.verify().unwrap();
                assert!(
                    crate::session::Storage::open_unwatched(&format!("{peer_name}-renamed"))
                        .is_ok()
                );
                assert!(crate::session::Storage::open_unwatched(&peer_name).is_err());
            } else {
                assert!(
                    result.is_err(),
                    "{action} must invalidate creation authority"
                );
                if action == "adopt" {
                    assert_eq!(
                        peer.load().unwrap()[0].project_path,
                        target.to_str().unwrap()
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
                } else if action == "dirty" {
                    assert_eq!(fs::read(target.join("untracked")).unwrap(), b"callback");
                } else if action == "preexisting" {
                    assert!(target.join("tracked").exists());
                    assert!(git2::Repository::open(&repo)
                        .unwrap()
                        .find_branch(&branch, git2::BranchType::Local)
                        .is_ok());
                }
            }
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
            (&mut ownership, Some(&workspace_owner)),
        );
        assert!(
            result.is_err(),
            "a peer adoption between completed workers must cancel publication"
        );
        assert_eq!(
            adopter.load().unwrap()[0].project_path,
            target.to_str().unwrap()
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
        )
        .unwrap()
        .1
        .verify()
        .unwrap();
        assert_eq!(fs::read(target.join("module/tracked")).unwrap(), b"base\n");
        assert!(crate::session::Storage::open_unwatched("peer-submodule-renamed").is_ok());
    }
}
