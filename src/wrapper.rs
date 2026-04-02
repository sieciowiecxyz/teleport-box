use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{RuntimeLayout, TeleportConfig};
use crate::util::{file_name, find_in_path, path_is_under, propagate_exit_status, shell_escape};

pub fn maybe_run_wrapper() -> Result<bool> {
    let argv0 = env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("teleport-box"));
    let name = file_name(&argv0).unwrap_or_else(|| "teleport-box".to_string());
    if name == "teleport-box" || env::var_os("TELEPORT_REMOTE_DESTINATION").is_none() {
        return Ok(false);
    }

    let args = env::args_os().skip(1).collect::<Vec<_>>();
    match name.as_str() {
        "teleport-pull" => run_teleport_pull(args)?,
        "teleport-push" => run_teleport_push(args)?,
        _ => run_wrapper(&name, args)?,
    }
    Ok(true)
}

pub fn run_wrapper(invoked_as: &str, args: Vec<OsString>) -> Result<()> {
    let envs = WrapperEnv::load()?;
    let cwd = remote_cwd_for_current_dir(&envs.remote_cwd, envs.shared_dir.as_deref())?;
    let mut remote_argv = Vec::with_capacity(args.len() + 1);
    remote_argv.push(invoked_as.to_string());
    for arg in args {
        let arg = arg
            .into_string()
            .map_err(|_| anyhow!("wrapper argument is not valid UTF-8"))?;
        remote_argv.push(arg);
    }
    let remote_command =
        build_remote_command(&cwd, &envs.remote_home, &envs.remote_path, &remote_argv);
    let ssh_remote_command = format!(
        "{} -c {}",
        shell_escape(&envs.remote_shell),
        shell_escape(&remote_command)
    );
    let mut ssh = ssh_command_from_env(&envs)?;
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        ssh.arg("-tt");
    }
    ssh.arg("--").arg(ssh_remote_command);

    let status = ssh.status().context("failed to execute ssh wrapper")?;
    propagate_exit_status(status)
}

pub fn ssh_command(config: &TeleportConfig, layout: &RuntimeLayout) -> Command {
    let host_ssh = find_in_path("ssh").unwrap_or_else(|| PathBuf::from("ssh"));
    let mut command = Command::new(host_ssh);
    apply_ssh_connection_options(
        &mut command,
        &config.ssh_destination(),
        config.port,
        config
            .identity_file
            .as_ref()
            .map(|_| layout.sandbox_identity_file.as_path()),
        Some(layout.sandbox_known_hosts.as_path()),
        Some(layout.ssh_control_socket.as_path()),
        config.batch_mode,
        &config.ssh_options,
    );
    command
}

pub fn ssh_master_command(config: &TeleportConfig, layout: &RuntimeLayout) -> Command {
    let host_ssh = find_in_path("ssh").unwrap_or_else(|| PathBuf::from("ssh"));
    let mut command = Command::new(host_ssh);
    apply_ssh_connection_options(
        &mut command,
        &config.ssh_destination(),
        config.port,
        config
            .identity_file
            .as_ref()
            .map(|_| layout.sandbox_identity_file.as_path()),
        Some(layout.sandbox_known_hosts.as_path()),
        None,
        config.batch_mode,
        &config.ssh_options,
    );
    command
}

fn run_teleport_pull(args: Vec<OsString>) -> Result<()> {
    let envs = WrapperEnv::load()?;
    let shared_dir = envs
        .shared_dir
        .clone()
        .ok_or_else(|| anyhow!("teleport-pull requires --shared-dir"))?;
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow!("teleport-pull argument is not valid UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let remote_path = args
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("usage: teleport-pull <remote-path> [local-path-under-/shared]"))?;
    let local_path = match args.get(1) {
        Some(local_path) => resolve_shared_output_path(local_path, &shared_dir)?,
        None => shared_dir.join(file_basename(&remote_path)?),
    };
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let ssh_remote_command = shell_with_arg(&envs.remote_shell, r#"cat -- "$1""#, &remote_path);
    let output = ssh_command_from_env(&envs)?
        .arg("--")
        .arg(ssh_remote_command)
        .output()
        .context("failed to run teleport-pull over ssh")?;
    if !output.status.success() {
        bail!(
            "teleport-pull failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::write(&local_path, output.stdout)
        .with_context(|| format!("failed to write {}", local_path.display()))?;
    Ok(())
}

fn run_teleport_push(args: Vec<OsString>) -> Result<()> {
    let envs = WrapperEnv::load()?;
    let shared_dir = envs
        .shared_dir
        .clone()
        .ok_or_else(|| anyhow!("teleport-push requires --shared-dir"))?;
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow!("teleport-push argument is not valid UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    if args.len() != 2 {
        bail!("usage: teleport-push <local-path-under-/shared> <remote-path>");
    }
    let local_path = resolve_existing_shared_path(&args[0], &shared_dir)?;
    let remote_path = &args[1];
    let content = fs::read(&local_path)
        .with_context(|| format!("failed to read {}", local_path.display()))?;
    let remote_command = concat!(
        "dst=$1; ",
        "dir=$(dirname -- \"$dst\") || exit 1; ",
        "mkdir -p -- \"$dir\" || exit 1; ",
        "tmp=$(mktemp \"${dst}.XXXXXX\") || exit 1; ",
        "trap 'rm -f -- \"$tmp\"' EXIT; ",
        "cat > \"$tmp\" && mv -f -- \"$tmp\" \"$dst\""
    );
    let ssh_remote_command = shell_with_arg(&envs.remote_shell, remote_command, remote_path);
    let mut ssh = ssh_command_from_env(&envs)?;
    ssh.arg("--").arg(ssh_remote_command);
    let mut child = ssh
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn teleport-push over ssh")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open stdin for teleport-push"))?
        .write_all(&content)
        .context("failed to stream teleport-push payload")?;
    let status = child.wait().context("failed to wait for teleport-push")?;
    propagate_exit_status(status)
}

fn ssh_command_from_env(envs: &WrapperEnv) -> Result<Command> {
    let mut command = Command::new(&envs.host_ssh);
    apply_ssh_connection_options(
        &mut command,
        &envs.destination,
        envs.port,
        envs.identity_file.as_deref().map(Path::new),
        Some(Path::new(&envs.known_hosts)),
        envs.control_path.as_deref().map(Path::new),
        envs.batch_mode,
        &envs.ssh_options,
    );
    Ok(command)
}

fn apply_ssh_connection_options(
    command: &mut Command,
    destination: &str,
    port: Option<u16>,
    identity_file: Option<&Path>,
    known_hosts: Option<&Path>,
    control_path: Option<&Path>,
    batch_mode: bool,
    ssh_options: &[String],
) {
    command.arg("-F").arg("/dev/null");
    if let Some(identity_file) = identity_file {
        command.arg("-i").arg(identity_file);
    }
    if let Some(port) = port {
        command.arg("-p").arg(port.to_string());
    }
    if batch_mode {
        command.arg("-o").arg("BatchMode=yes");
    }
    if let Some(known_hosts) = known_hosts {
        command
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }
    if let Some(control_path) = control_path {
        command
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=no")
            .arg("-S")
            .arg(control_path);
    }
    for ssh_option in ssh_options {
        command.arg("-o").arg(ssh_option);
    }
    command.arg(destination);
}

fn build_remote_command(
    cwd: &str,
    remote_home: &str,
    remote_path: &str,
    argv: &[String],
) -> String {
    let mut command = String::from(
        "unset SSH_CLIENT SSH_CONNECTION SSH_TTY; export HISTFILE=/dev/null HISTSIZE=0; cd -- ",
    );
    command.push_str(&shell_escape(cwd));
    command.push_str(" && exec env HOME=");
    command.push_str(&shell_escape(remote_home));
    command.push_str(" PATH=");
    command.push_str(&shell_escape(remote_path));
    for arg in argv {
        command.push(' ');
        command.push_str(&shell_escape(arg));
    }
    command
}

fn remote_cwd_for_current_dir(
    default_remote_cwd: &str,
    shared_dir: Option<&Path>,
) -> Result<String> {
    let cwd = env::current_dir().context("failed to resolve current dir")?;
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| anyhow!("current dir is not valid UTF-8"))?;
    if shared_dir.is_some_and(|shared| path_is_under(cwd_str, shared.to_string_lossy().as_ref())) {
        return Ok(default_remote_cwd.to_string());
    }
    if crate::config::LOCAL_ONLY_PREFIXES
        .iter()
        .any(|prefix| cwd_str == *prefix || cwd_str.starts_with(&format!("{prefix}/")))
    {
        return Ok(default_remote_cwd.to_string());
    }
    Ok(cwd_str.to_string())
}

fn shell_with_arg(shell_path: &str, script: &str, arg: &str) -> String {
    format!(
        "{} -c {} sh {}",
        shell_escape(shell_path),
        shell_escape(script),
        shell_escape(arg)
    )
}

fn resolve_existing_shared_path(raw: &str, shared_dir: &Path) -> Result<PathBuf> {
    let path = absolute_shared_candidate(raw)?;
    let canonical_path = fs::canonicalize(&path)
        .with_context(|| format!("failed to resolve shared path {}", path.display()))?;
    ensure_within_shared_dir(&canonical_path, shared_dir)?;
    Ok(canonical_path)
}

fn resolve_shared_output_path(raw: &str, shared_dir: &Path) -> Result<PathBuf> {
    let path = absolute_shared_candidate(raw)?;
    let (existing_parent, tail) = split_existing_parent(&path)?;
    let canonical_parent = fs::canonicalize(&existing_parent).with_context(|| {
        format!(
            "failed to resolve existing shared parent {}",
            existing_parent.display()
        )
    })?;
    ensure_within_shared_dir(&canonical_parent, shared_dir)?;
    Ok(tail
        .into_iter()
        .fold(canonical_parent, |acc, part| acc.join(part)))
}

fn absolute_shared_candidate(raw: &str) -> Result<PathBuf> {
    if Path::new(raw).is_absolute() {
        return Ok(PathBuf::from(raw));
    }
    Ok(env::current_dir()
        .context("failed to resolve current dir")?
        .join(raw))
}

fn split_existing_parent(path: &Path) -> Result<(PathBuf, Vec<OsString>)> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            bail!("shared path has no existing parent: {}", path.display());
        };
        tail.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            bail!("shared path has no existing parent: {}", path.display());
        };
        existing = parent.to_path_buf();
    }
    tail.reverse();
    Ok((existing, tail))
}

fn ensure_within_shared_dir(path: &Path, shared_dir: &Path) -> Result<()> {
    if path == shared_dir || path.starts_with(shared_dir) {
        return Ok(());
    }
    bail!("shared path must stay under {}", shared_dir.display());
}

fn file_basename(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("path has no valid file name: {path}"))
}

fn env_required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("missing required environment variable `{name}`"))
}

fn env_optional(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    (!value.is_empty()).then_some(value)
}

#[derive(Debug)]
struct WrapperEnv {
    destination: String,
    port: Option<u16>,
    identity_file: Option<String>,
    known_hosts: String,
    control_path: Option<String>,
    host_ssh: String,
    batch_mode: bool,
    ssh_options: Vec<String>,
    remote_cwd: String,
    remote_home: String,
    remote_shell: String,
    remote_path: String,
    shared_dir: Option<PathBuf>,
}

impl WrapperEnv {
    fn load() -> Result<Self> {
        let port = match env_optional("TELEPORT_REMOTE_PORT") {
            Some(port) => Some(
                port.parse::<u16>()
                    .with_context(|| format!("invalid TELEPORT_REMOTE_PORT: {port}"))?,
            ),
            None => None,
        };
        let batch_mode = env::var("TELEPORT_BATCH_MODE").unwrap_or_else(|_| "0".to_string()) == "1";
        let ssh_options = env_optional("TELEPORT_SSH_OPTIONS")
            .map(|joined| joined.split('\n').map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Self {
            destination: env_required("TELEPORT_REMOTE_DESTINATION")?,
            port,
            identity_file: env_optional("TELEPORT_IDENTITY_FILE"),
            known_hosts: env_required("TELEPORT_KNOWN_HOSTS")?,
            control_path: env_optional("TELEPORT_SSH_CONTROL_PATH"),
            host_ssh: env_required("TELEPORT_HOST_SSH")?,
            batch_mode,
            ssh_options,
            remote_cwd: env_required("TELEPORT_REMOTE_CWD")?,
            remote_home: env_required("TELEPORT_REMOTE_HOME")?,
            remote_shell: env_required("TELEPORT_REMOTE_SHELL")?,
            remote_path: env_required("TELEPORT_REMOTE_PATH")?,
            shared_dir: env_optional("TELEPORT_SHARED_DIR").map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SANDBOX_HELPER_COMMANDS;

    #[test]
    fn local_control_plane_paths_fall_back_to_default_remote_cwd() {
        let cwd = "/codex/home";
        assert!(
            crate::config::LOCAL_ONLY_PREFIXES
                .iter()
                .any(|prefix| cwd == *prefix || cwd.starts_with(&format!("{prefix}/")))
        );
    }

    #[test]
    fn shared_paths_must_stay_under_shared_dir() {
        let tempdir = tempfile::tempdir().unwrap();
        let shared = tempdir.path().join("shared");
        let outside = tempdir.path().join("outside");
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let ok_file = shared.join("report.txt");
        fs::write(&ok_file, "ok").unwrap();
        assert!(resolve_existing_shared_path(ok_file.to_string_lossy().as_ref(), &shared).is_ok());

        let escaped = shared.join("escape-link");
        std::os::unix::fs::symlink(&outside, &escaped).unwrap();
        let escaped_target = escaped.join("report.txt");
        fs::write(outside.join("report.txt"), "nope").unwrap();
        assert!(
            resolve_existing_shared_path(escaped_target.to_string_lossy().as_ref(), &shared)
                .is_err()
        );
    }

    #[test]
    fn helper_commands_are_explicit() {
        assert!(SANDBOX_HELPER_COMMANDS.contains(&"teleport-pull"));
        assert!(SANDBOX_HELPER_COMMANDS.contains(&"teleport-push"));
    }
}
