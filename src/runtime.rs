use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{
    ABSOLUTE_WRAP_COMMANDS, ABSOLUTE_WRAP_DIRS, Cli, Commands, LaunchMode, LaunchSpec, MountGuard,
    REMOTE_BIND_DIRS, RuntimeLayout, TeleportConfig,
};
use crate::util::{
    add_ro_bind_if_exists, bind_absolute_wrapper_if_exists, ensure_binary_exists,
    exit_code_from_status, find_in_path, is_executable_path, path_is_under, prepare_bind_dir,
    prepare_bind_parent, set_file_mode,
};
use crate::wrapper::ssh_command;

pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Exec(args) => {
            let config = TeleportConfig::from_connection(args.connection)?;
            run_subcommand(config, LaunchMode::Arbitrary(args.command))
        }
        Commands::Codex(args) => {
            let config = TeleportConfig::from_connection(args.connection)?;
            let codex_binary = resolve_local_binary(
                args.codex_binary.as_deref(),
                "codex",
                "could not find local `codex` binary",
            )?;
            let codex_package_root = codex_package_root_from_binary(&codex_binary)?;
            let node_binary =
                resolve_local_binary(None, "node", "could not find local `node` binary")?;
            let apply_patch = resolve_optional_local_binary("apply_patch");
            run_subcommand(
                config,
                LaunchMode::Codex {
                    codex_package_root,
                    node_binary,
                    apply_patch_binary: apply_patch,
                    codex_args: args.codex_args,
                },
            )
        }
    }
}

fn run_subcommand(config: TeleportConfig, mode: LaunchMode) -> Result<ExitCode> {
    preflight(&config, &mode)?;
    let layout = prepare_runtime()?;
    stage_ssh_material(&config, &layout)?;
    let wrapped_commands = discover_wrapped_commands(&config, &layout)?;
    create_wrapper_symlinks(&layout.wrapper_dir, &wrapped_commands)?;
    let _mount = mount_remote_root(&config, &layout.remote_root_mount)?;

    let (program, args) = match mode {
        LaunchMode::Arbitrary(command) => {
            let program = command
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("missing command after `exec --`"))?;
            let args = command.into_iter().skip(1).collect::<Vec<_>>();
            (program, args)
        }
        LaunchMode::Codex {
            codex_package_root,
            node_binary,
            apply_patch_binary,
            codex_args,
        } => {
            let code = run_inside_bwrap(
                &config,
                &layout,
                &wrapped_commands,
                LaunchSpec::Codex {
                    codex_package_root,
                    node_binary,
                    apply_patch_binary,
                    codex_args,
                },
            )?;
            return Ok(code);
        }
    };

    run_inside_bwrap(
        &config,
        &layout,
        &wrapped_commands,
        LaunchSpec::Arbitrary {
            program,
            args,
            apply_patch_binary: resolve_optional_local_binary("apply_patch"),
        },
    )
}

fn preflight(config: &TeleportConfig, mode: &LaunchMode) -> Result<()> {
    ensure_binary_exists("ssh")?;
    ensure_binary_exists("sshfs")?;
    ensure_binary_exists("bwrap")?;
    ensure_binary_exists("fusermount3").or_else(|_| ensure_binary_exists("fusermount"))?;
    if !config.identity_file.exists() {
        bail!(
            "identity file does not exist: {}",
            config.identity_file.display()
        );
    }
    if let LaunchMode::Codex {
        codex_package_root,
        node_binary,
        ..
    } = mode
    {
        if !codex_package_root.exists() {
            bail!(
                "codex package root does not exist: {}",
                codex_package_root.display()
            );
        }
        if !node_binary.exists() {
            bail!("node binary does not exist: {}", node_binary.display());
        }
    }
    Ok(())
}

fn prepare_runtime() -> Result<RuntimeLayout> {
    let self_binary = std::env::current_exe()
        .context("failed to resolve current teleport-box binary")?
        .canonicalize()
        .context("failed to canonicalize current teleport-box binary")?;
    let tempdir = tempfile::Builder::new()
        .prefix("teleport-box-")
        .tempdir()
        .context("failed to create runtime dir")?;
    let remote_root_mount = tempdir.path().join("remote-root");
    let codex_home = tempdir.path().join("codex-home");
    let wrapper_dir = tempdir.path().join("wrappers");
    let ssh_dir = tempdir.path().join("ssh");
    let sandbox_identity_file = ssh_dir.join("id_ed25519");
    let sandbox_known_hosts = ssh_dir.join("known_hosts");

    fs::create_dir_all(&remote_root_mount)?;
    fs::create_dir_all(&codex_home)?;
    fs::create_dir_all(&wrapper_dir)?;
    fs::create_dir_all(&ssh_dir)?;

    Ok(RuntimeLayout {
        _tempdir: tempdir,
        self_binary,
        remote_root_mount,
        codex_home,
        wrapper_dir,
        ssh_dir,
        sandbox_identity_file,
        sandbox_known_hosts,
    })
}

fn create_wrapper_symlinks(wrapper_dir: &Path, wrapped_commands: &BTreeSet<String>) -> Result<()> {
    for command in wrapped_commands {
        let target = wrapper_dir.join(command);
        if target.exists() {
            continue;
        }
        symlink("../bin/teleport-box", &target).with_context(|| {
            format!("failed to create wrapper symlink: {}", target.display())
        })?;
    }
    Ok(())
}

fn discover_wrapped_commands(
    config: &TeleportConfig,
    layout: &RuntimeLayout,
) -> Result<BTreeSet<String>> {
    let mut commands = crate::config::SEED_WRAPPED_COMMANDS
        .iter()
        .map(|command| command.to_string())
        .collect::<BTreeSet<_>>();
    commands.extend(discover_local_executable_names()?);
    commands.extend(discover_remote_executable_names(config, layout)?);
    Ok(commands)
}

fn discover_local_executable_names() -> Result<BTreeSet<String>> {
    let mut commands = BTreeSet::new();
    for directory in ABSOLUTE_WRAP_DIRS {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !is_executable_path(&path) {
                continue;
            }
            if let Some(name) = path.file_name().and_then(OsStr::to_str) {
                commands.insert(name.to_string());
            }
        }
    }
    Ok(commands)
}

fn discover_remote_executable_names(
    config: &TeleportConfig,
    layout: &RuntimeLayout,
) -> Result<BTreeSet<String>> {
    let script = r#"IFS=:
for dir in $PATH; do
  [ -d "$dir" ] || continue
  find -L "$dir" -maxdepth 1 -mindepth 1 \( -type f -o -type l \) -executable -printf '%f\n'
done | sort -u"#;
    let output = ssh_command(config, layout)
        .arg("--")
        .arg("/bin/bash")
        .arg("-lc")
        .arg(script)
        .output()
        .context("failed to discover remote executables")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to discover remote executables: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn mount_remote_root(config: &TeleportConfig, mountpoint: &Path) -> Result<MountGuard> {
    let runtime_root = mountpoint
        .parent()
        .ok_or_else(|| anyhow!("mountpoint has no runtime parent"))?;
    let sandbox_identity_file = runtime_root.join("ssh").join("id_ed25519");
    let sandbox_known_hosts = runtime_root.join("ssh").join("known_hosts");
    let source = format!("{}@{}:/", config.user, config.host);
    let status = Command::new("sshfs")
        .arg(source)
        .arg(mountpoint)
        .arg("-o")
        .arg(format!(
            "ssh_command=ssh -F /dev/null -i {} -p {} -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
            sandbox_identity_file.display(),
            config.port,
            sandbox_known_hosts.display()
        ))
        .arg("-o")
        .arg("reconnect")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .status()
        .context("failed to start sshfs")?;
    if !status.success() {
        bail!("sshfs mount failed with status {status}");
    }
    Ok(MountGuard {
        mountpoint: mountpoint.to_path_buf(),
    })
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = lazy_unmount(&self.mountpoint, "fusermount3");
        let _ = lazy_unmount(&self.mountpoint, "fusermount");
    }
}

fn lazy_unmount(mountpoint: &Path, fusermount: &str) -> Result<()> {
    let status = Command::new(fusermount)
        .arg("-u")
        .arg("-z")
        .arg(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run {fusermount}"))?;
    if status.success() {
        Ok(())
    } else {
        let text = status.to_string();
        if text.contains("No such file") || text.contains("not found in /etc/mtab") {
            Ok(())
        } else {
            bail!("{fusermount} returned {status}");
        }
    }
}

fn run_inside_bwrap(
    config: &TeleportConfig,
    layout: &RuntimeLayout,
    wrapped_commands: &BTreeSet<String>,
    spec: LaunchSpec,
) -> Result<ExitCode> {
    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent").arg("--new-session");
    command.arg("--proc").arg("/proc");
    command.arg("--dev").arg("/dev");
    command.arg("--tmpfs").arg("/tmp");

    add_ro_bind_if_exists(&mut command, "/usr", "/usr");
    add_ro_bind_if_exists(&mut command, "/bin", "/bin");
    add_ro_bind_if_exists(&mut command, "/sbin", "/sbin");
    add_ro_bind_if_exists(&mut command, "/lib", "/lib");
    add_ro_bind_if_exists(&mut command, "/lib64", "/lib64");
    add_ro_bind_if_exists(&mut command, "/etc", "/etc");

    prepare_bind_dir(&mut command, Path::new("/codex/bin"));
    prepare_bind_dir(&mut command, Path::new("/codex/home"));
    prepare_bind_dir(&mut command, Path::new("/codex/wrappers"));
    prepare_bind_dir(&mut command, Path::new("/codex/ssh"));
    prepare_bind_dir(&mut command, Path::new("/remote-root"));

    prepare_bind_parent(&mut command, Path::new("/codex/bin/teleport-box"));
    command
        .arg("--ro-bind")
        .arg(&layout.self_binary)
        .arg("/codex/bin/teleport-box");
    command.arg("--bind").arg(&layout.codex_home).arg("/codex/home");
    command.arg("--ro-bind").arg(&layout.wrapper_dir).arg("/codex/wrappers");
    command.arg("--bind").arg(&layout.ssh_dir).arg("/codex/ssh");
    command
        .arg("--bind")
        .arg(&layout.remote_root_mount)
        .arg("/remote-root");

    if let Some(local_codex_home) = local_codex_home() {
        prepare_bind_parent(&mut command, Path::new("/codex/home/.codex"));
        command
            .arg("--bind")
            .arg(local_codex_home)
            .arg("/codex/home/.codex");
    }

    for target in absolute_wrapper_targets(wrapped_commands)? {
        bind_absolute_wrapper_if_exists(&mut command, &layout.self_binary, &target);
    }

    for remote_dir in remote_bind_targets(config) {
        let source = source_for_remote_target(&layout.remote_root_mount, &remote_dir);
        if source.exists() {
            prepare_bind_dir(&mut command, Path::new(&remote_dir));
            command.arg("--bind").arg(source).arg(remote_dir);
        }
    }

    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    command.arg("--setenv").arg("PATH").arg(
        "/codex/wrappers:/codex/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin",
    );
    command.arg("--setenv").arg("HOME").arg("/codex/home");
    command.arg("--setenv").arg("SHELL").arg("/codex/wrappers/bash");
    command.arg("--setenv").arg("TERM").arg(term);
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_HOST")
        .arg(&config.host);
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_USER")
        .arg(&config.user);
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_PORT")
        .arg(config.port.to_string());
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_CWD")
        .arg(&config.remote_cwd);
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_HOME")
        .arg(&config.remote_home);
    command
        .arg("--setenv")
        .arg("TELEPORT_IDENTITY_FILE")
        .arg("/codex/ssh/id_ed25519");
    command
        .arg("--setenv")
        .arg("TELEPORT_KNOWN_HOSTS")
        .arg("/codex/ssh/known_hosts");

    let start_dir = start_dir_for_bwrap(config);
    prepare_bind_dir(&mut command, Path::new(&start_dir));
    command.arg("--chdir").arg(start_dir);

    match spec {
        LaunchSpec::Arbitrary {
            program,
            args,
            apply_patch_binary,
        } => {
            if let Some(apply_patch_binary) = apply_patch_binary {
                prepare_bind_parent(&mut command, Path::new("/codex/bin/apply_patch"));
                command
                    .arg("--ro-bind")
                    .arg(apply_patch_binary)
                    .arg("/codex/bin/apply_patch");
            }
            command
                .arg("--")
                .arg("/usr/bin/env")
                .arg(
                    "PATH=/codex/wrappers:/codex/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin",
                )
                .arg("HOME=/codex/home")
                .arg("SHELL=/codex/wrappers/bash")
                .arg(program)
                .args(args);
        }
        LaunchSpec::Codex {
            codex_package_root,
            node_binary,
            apply_patch_binary,
            codex_args,
        } => {
            prepare_bind_dir(&mut command, Path::new("/codex/npm/codex"));
            command
                .arg("--ro-bind")
                .arg(codex_package_root)
                .arg("/codex/npm/codex");
            prepare_bind_parent(&mut command, Path::new("/codex/bin/node"));
            command.arg("--ro-bind").arg(node_binary).arg("/codex/bin/node");
            if let Some(apply_patch_binary) = apply_patch_binary {
                prepare_bind_parent(&mut command, Path::new("/codex/bin/apply_patch"));
                command
                    .arg("--ro-bind")
                    .arg(apply_patch_binary)
                    .arg("/codex/bin/apply_patch");
            }
            command
                .arg("--")
                .arg("/codex/bin/node")
                .arg("/codex/npm/codex/bin/codex.js")
                .args(codex_args);
        }
    }

    let status = command.status().context("failed to launch bwrap")?;
    exit_code_from_status(status)
}

fn stage_ssh_material(config: &TeleportConfig, layout: &RuntimeLayout) -> Result<()> {
    fs::copy(&config.identity_file, &layout.sandbox_identity_file).with_context(|| {
        format!(
            "failed to copy identity file into runtime dir: {}",
            config.identity_file.display()
        )
    })?;
    set_file_mode(&layout.sandbox_identity_file, 0o600)?;
    fs::write(&layout.sandbox_known_hosts, "").context("failed to create known_hosts file")?;
    set_file_mode(&layout.sandbox_known_hosts, 0o600)?;
    Ok(())
}

fn absolute_wrapper_targets(wrapped_commands: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut targets = Vec::new();
    for command in ABSOLUTE_WRAP_COMMANDS {
        if !wrapped_commands.contains(*command) {
            continue;
        }
        for directory in ABSOLUTE_WRAP_DIRS {
            let path = Path::new(directory).join(command);
            if is_executable_path(&path) {
                targets.push(path.display().to_string());
            }
        }
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

fn remote_bind_targets(config: &TeleportConfig) -> Vec<String> {
    let mut targets = REMOTE_BIND_DIRS
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    if !targets.iter().any(|prefix| path_is_under(&config.remote_cwd, prefix)) {
        targets.push(config.remote_cwd.clone());
    }
    targets.sort();
    targets.dedup();
    targets
}

fn source_for_remote_target(remote_root_mount: &Path, target: &str) -> PathBuf {
    if target == "/" {
        return remote_root_mount.to_path_buf();
    }
    let relative = target.trim_start_matches('/');
    remote_root_mount.join(relative)
}

fn start_dir_for_bwrap(config: &TeleportConfig) -> String {
    if remote_bind_targets(config)
        .into_iter()
        .any(|target| path_is_under(&config.remote_cwd, &target))
    {
        config.remote_cwd.clone()
    } else {
        "/root".to_string()
    }
}

fn local_codex_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".codex");
    path.exists().then_some(path)
}

fn resolve_local_binary(explicit: Option<&Path>, name: &str, message: &str) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return fs::canonicalize(path)
            .with_context(|| format!("failed to resolve `{}`", path.display()));
    }
    let path = find_in_path(name).ok_or_else(|| anyhow!(message.to_string()))?;
    fs::canonicalize(&path)
        .with_context(|| format!("failed to resolve `{}`", path.display()))
}

fn resolve_optional_local_binary(name: &str) -> Option<PathBuf> {
    find_in_path(name)
}

fn codex_package_root_from_binary(codex_binary: &Path) -> Result<PathBuf> {
    let mut current = codex_binary
        .parent()
        .ok_or_else(|| anyhow!("codex binary has no parent directory"))?;
    if current.file_name() == Some(OsStr::new("bin")) {
        current = current
            .parent()
            .ok_or_else(|| anyhow!("codex binary is missing package root"))?;
    }
    let package_json = current.join("package.json");
    if !package_json.exists() {
        bail!(
            "could not locate Codex package root next to {}",
            codex_binary.display()
        );
    }
    Ok(current.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_targets_include_remote_cwd_when_needed() {
        let config = TeleportConfig {
            host: "example.com".to_string(),
            user: "root".to_string(),
            port: 22,
            identity_file: PathBuf::from("/tmp/id"),
            remote_cwd: "/workspace/demo".to_string(),
            remote_home: "/root".to_string(),
        };
        let targets = remote_bind_targets(&config);
        assert!(targets.contains(&"/workspace/demo".to_string()));
        assert!(targets.contains(&"/root".to_string()));
    }
}
