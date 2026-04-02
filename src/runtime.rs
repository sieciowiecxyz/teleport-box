use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{
    Cli, Commands, DoctorArgs, LaunchMode, LaunchSpec, MountGuard, REMOTE_BIND_DIRS, RemoteProfile,
    RuntimeLayout, SANDBOX_HELPER_COMMANDS, TeleportConfig,
};
use crate::util::{
    add_ro_bind_if_exists, ensure_binary_exists, exit_code_from_status, find_in_path,
    path_is_under, prepare_bind_dir, prepare_bind_parent, set_file_mode, shell_escape,
};
use crate::wrapper::{ssh_command, ssh_master_command};

const DEFAULT_REMOTE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const SHELL_CANDIDATES: &[&str] = &[
    "/bin/sh",
    "/bin/ash",
    "/bin/dash",
    "/bin/bash",
    "/usr/bin/bash",
    "sh",
];

pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Doctor(args) => run_doctor(args),
        Commands::Shell(args) => {
            let config = TeleportConfig::from_connection(args.connection)?;
            run_subcommand(config, LaunchMode::Shell(args.shell_args))
        }
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
                    codex_args: prepare_codex_args(args.codex_args),
                },
            )
        }
    }
}

fn run_doctor(args: DoctorArgs) -> Result<ExitCode> {
    let config = TeleportConfig::from_connection(args.connection)?;
    preflight(&config, &LaunchMode::Arbitrary(vec!["true".to_string()]))?;
    let layout = prepare_runtime()?;
    stage_ssh_material(&config, &layout)?;
    stage_codex_home(&layout)?;
    let _ssh_master = start_ssh_master(&config, &layout)?;
    let result = (|| -> Result<ExitCode> {
        let profile = probe_remote_profile(&config, &layout)?;
        let _mount = mount_remote_root(&config, &layout.remote_root_mount)?;

        println!("destination={}", config.ssh_destination());
        println!("shell_path={}", profile.shell_path);
        println!("shell_name={}", profile.shell_name);
        println!("busybox_like={}", profile.busybox_like);
        println!("remote_path={}", profile.remote_path.join(":"));
        println!(
            "remote_exec_dirs={}",
            profile
                .exec_names_by_dir
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(":")
        );
        println!(
            "wrapped_commands_count={}",
            wrapped_command_names(&profile).len()
        );
        println!("sshfs_mount=ok");

        Ok(ExitCode::SUCCESS)
    })();
    persist_ssh_material(&layout)?;
    result
}

fn run_subcommand(config: TeleportConfig, mode: LaunchMode) -> Result<ExitCode> {
    preflight(&config, &mode)?;
    let layout = prepare_runtime()?;
    stage_ssh_material(&config, &layout)?;
    stage_codex_home(&layout)?;
    let _ssh_master = start_ssh_master(&config, &layout)?;
    let result = (|| -> Result<ExitCode> {
        let profile = probe_remote_profile(&config, &layout)?;
        let wrapped_commands = wrapped_command_names(&profile);
        create_wrapper_symlinks(&layout.wrapper_dir, &wrapped_commands)?;
        build_exec_overlay_dirs(&layout, &config, &profile)?;
        let _mount = mount_remote_root(&config, &layout.remote_root_mount)?;

        let (program, args) = match mode {
            LaunchMode::Shell(shell_args) => (profile.shell_name.clone(), shell_args),
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
                    &profile,
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
            &profile,
            LaunchSpec::Arbitrary {
                program,
                args,
                apply_patch_binary: resolve_optional_local_binary("apply_patch"),
            },
        )
    })();
    persist_ssh_material(&layout)?;
    result
}

fn preflight(config: &TeleportConfig, mode: &LaunchMode) -> Result<()> {
    ensure_binary_exists("ssh")?;
    ensure_binary_exists("sshfs")?;
    ensure_binary_exists("bwrap")?;
    ensure_binary_exists("fusermount3").or_else(|_| ensure_binary_exists("fusermount"))?;
    if let Some(identity_file) = &config.identity_file {
        if !identity_file.exists() {
            bail!("identity file does not exist: {}", identity_file.display());
        }
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
    let exec_overlay_root = tempdir.path().join("exec-overlay");
    let ssh_dir = tempdir.path().join("ssh");
    let ssh_control_dir = tempdir.path().join("ssh-control");
    let ssh_control_socket = ssh_control_dir.join("control.sock");
    let sandbox_identity_file = ssh_dir.join("id_ed25519");
    let sandbox_known_hosts = ssh_dir.join("known_hosts");
    let sshfs_ssh_command = ssh_dir.join("sshfs-ssh");

    fs::create_dir_all(&remote_root_mount)?;
    fs::create_dir_all(&codex_home)?;
    fs::create_dir_all(&wrapper_dir)?;
    fs::create_dir_all(&exec_overlay_root)?;
    fs::create_dir_all(&ssh_dir)?;
    fs::create_dir_all(&ssh_control_dir)?;

    Ok(RuntimeLayout {
        _tempdir: tempdir,
        self_binary,
        remote_root_mount,
        codex_home,
        wrapper_dir,
        exec_overlay_root,
        ssh_control_dir,
        ssh_control_socket,
        sandbox_identity_file,
        sandbox_known_hosts,
        sshfs_ssh_command,
    })
}

fn create_wrapper_symlinks(wrapper_dir: &Path, wrapped_commands: &BTreeSet<String>) -> Result<()> {
    for command in wrapped_commands {
        let target = wrapper_dir.join(command);
        if target.exists() {
            continue;
        }
        symlink("/codex/bin/teleport-box", &target)
            .with_context(|| format!("failed to create wrapper symlink: {}", target.display()))?;
    }
    Ok(())
}

fn build_exec_overlay_dirs(
    layout: &RuntimeLayout,
    config: &TeleportConfig,
    profile: &RemoteProfile,
) -> Result<()> {
    for target in overlay_target_dirs(config, profile) {
        let source = overlay_source_dir(&layout.exec_overlay_root, &target);
        fs::create_dir_all(&source)
            .with_context(|| format!("failed to create synthetic exec dir {}", source.display()))?;
        let Some(commands) = profile.exec_names_by_dir.get(&target) else {
            continue;
        };
        for command in commands {
            let symlink_path = source.join(command);
            if symlink_path.exists() {
                continue;
            }
            symlink("/codex/bin/teleport-box", &symlink_path).with_context(|| {
                format!(
                    "failed to create synthetic exec symlink {}",
                    symlink_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn probe_remote_profile(config: &TeleportConfig, layout: &RuntimeLayout) -> Result<RemoteProfile> {
    let shell_path = probe_shell_path(config, layout)?;
    let shell_name = Path::new(&shell_path)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("sh")
        .to_string();
    let remote_path_output = run_probe_capture(
        config,
        layout,
        &shell_path,
        r#"echo "${PATH:-/usr/bin:/bin}""#,
    )?;
    let remote_path = split_remote_path(&remote_path_output);
    let exec_names_by_dir = probe_remote_exec_map(config, layout, &shell_path, &remote_path)?;
    let busybox_like = run_probe_capture(
        config,
        layout,
        &shell_path,
        r#"if command -v busybox >/dev/null 2>&1; then echo yes; else echo no; fi"#,
    )? == "yes";

    Ok(RemoteProfile {
        shell_path,
        shell_name,
        remote_path,
        exec_names_by_dir,
        busybox_like,
    })
}

fn probe_shell_path(config: &TeleportConfig, layout: &RuntimeLayout) -> Result<String> {
    let mut candidates = Vec::new();
    if let Some(remote_shell) = &config.remote_shell {
        candidates.push(remote_shell.clone());
    }
    for candidate in SHELL_CANDIDATES {
        if !candidates.iter().any(|existing| existing == candidate) {
            candidates.push((*candidate).to_string());
        }
    }

    for candidate in candidates {
        let ssh_remote_command = format!("{} -c {}", shell_escape(&candidate), shell_escape(":"));
        let output = ssh_command(config, layout)
            .arg("--")
            .arg(ssh_remote_command)
            .output()
            .with_context(|| format!("failed probing remote shell `{candidate}`"))?;
        if output.status.success() {
            return Ok(candidate);
        }
    }

    bail!(
        "failed to find a working remote shell; tried {:?}",
        SHELL_CANDIDATES
    )
}

fn run_probe_capture(
    config: &TeleportConfig,
    layout: &RuntimeLayout,
    shell_path: &str,
    script: &str,
) -> Result<String> {
    let ssh_remote_command = format!("{} -c {}", shell_escape(shell_path), shell_escape(script));
    let output = ssh_command(config, layout)
        .arg("--")
        .arg(ssh_remote_command)
        .output()
        .with_context(|| format!("failed to run remote probe via shell `{shell_path}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("remote probe failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn split_remote_path(remote_path_output: &str) -> Vec<String> {
    let mut remote_path = remote_path_output
        .split(':')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if remote_path.is_empty() {
        remote_path = DEFAULT_REMOTE_PATH
            .split(':')
            .map(ToOwned::to_owned)
            .collect();
    }
    remote_path
}

fn probe_remote_exec_map(
    config: &TeleportConfig,
    layout: &RuntimeLayout,
    shell_path: &str,
    remote_path: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut exec_dirs = remote_path.to_vec();
    for extra in &config.remote_bin_dirs {
        if !exec_dirs.iter().any(|entry| entry == extra) {
            exec_dirs.push(extra.clone());
        }
    }
    exec_dirs.sort();
    exec_dirs.dedup();

    let mut script = String::new();
    for dir in &exec_dirs {
        script.push_str("if [ -d ");
        script.push_str(&shell_escape(dir));
        script.push_str(" ]; then for f in ");
        script.push_str(&shell_escape(dir));
        script.push_str("/*");
        script.push_str("; do [ -e \"$f\" ] || continue; [ -x \"$f\" ] || continue; echo ");
        script.push_str(&shell_escape(&format!("{dir}|")));
        script.push_str("\"${f##*/}\"; done; fi; ");
    }

    let output = run_probe_capture(config, layout, shell_path, &script)?;
    let mut map = BTreeMap::<String, BTreeSet<String>>::new();
    for line in output.lines() {
        let mut parts = line.splitn(2, '|');
        let Some(dir) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        if dir.is_empty() || name.is_empty() {
            continue;
        }
        map.entry(dir.to_string())
            .or_default()
            .insert(name.to_string());
    }
    Ok(map)
}

fn mount_remote_root(config: &TeleportConfig, mountpoint: &Path) -> Result<MountGuard> {
    let source = config.sshfs_source_root();
    let status = Command::new("sshfs")
        .arg(source)
        .arg(mountpoint)
        .arg("-o")
        .arg(format!(
            "ssh_command={}",
            mountpoint
                .parent()
                .unwrap()
                .join("ssh")
                .join("sshfs-ssh")
                .display()
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
    profile: &RemoteProfile,
    spec: LaunchSpec,
) -> Result<ExitCode> {
    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent").arg("--new-session");
    command.arg("--proc").arg("/proc");
    command.arg("--dev").arg("/dev");
    command.arg("--tmpfs").arg("/tmp");
    command.arg("--unsetenv").arg("SSH_AUTH_SOCK");
    command.arg("--unsetenv").arg("GIT_DIR");
    command.arg("--unsetenv").arg("GIT_WORK_TREE");
    command.arg("--unsetenv").arg("GIT_INDEX_FILE");
    command.arg("--unsetenv").arg("PYTHONPATH");
    command.arg("--unsetenv").arg("VIRTUAL_ENV");
    command.arg("--unsetenv").arg("PIP_REQUIRE_VIRTUALENV");

    add_ro_bind_if_exists(&mut command, "/usr", "/usr");
    add_ro_bind_if_exists(&mut command, "/bin", "/bin");
    add_ro_bind_if_exists(&mut command, "/sbin", "/sbin");
    add_ro_bind_if_exists(&mut command, "/lib", "/lib");
    add_ro_bind_if_exists(&mut command, "/lib64", "/lib64");
    add_ro_bind_if_exists(&mut command, "/etc", "/etc");

    prepare_bind_dir(&mut command, Path::new("/codex/bin"));
    prepare_bind_dir(&mut command, Path::new("/codex/home"));
    prepare_bind_dir(&mut command, Path::new("/codex/wrappers"));
    prepare_bind_dir(&mut command, Path::new("/codex/host-bin"));
    prepare_bind_dir(&mut command, Path::new("/codex/ssh"));
    prepare_bind_dir(&mut command, Path::new("/codex/ssh-control"));
    prepare_bind_dir(&mut command, Path::new("/remote-root"));
    prepare_bind_dir(&mut command, Path::new("/shared"));

    prepare_bind_parent(&mut command, Path::new("/codex/bin/teleport-box"));
    command
        .arg("--ro-bind")
        .arg(&layout.self_binary)
        .arg("/codex/bin/teleport-box");
    prepare_bind_parent(&mut command, Path::new("/codex/host-bin/ssh"));
    command
        .arg("--ro-bind")
        .arg(resolve_local_binary(
            None,
            "ssh",
            "could not find local `ssh` binary",
        )?)
        .arg("/codex/host-bin/ssh");
    command
        .arg("--bind")
        .arg(&layout.codex_home)
        .arg("/codex/home");
    command
        .arg("--ro-bind")
        .arg(&layout.wrapper_dir)
        .arg("/codex/wrappers");
    prepare_bind_parent(&mut command, Path::new("/codex/ssh/known_hosts"));
    command
        .arg("--bind")
        .arg(&layout.sandbox_known_hosts)
        .arg("/codex/ssh/known_hosts");
    if config.identity_file.is_some() && layout.sandbox_identity_file.exists() {
        prepare_bind_parent(&mut command, Path::new("/codex/ssh/id_ed25519"));
        command
            .arg("--bind")
            .arg(&layout.sandbox_identity_file)
            .arg("/codex/ssh/id_ed25519");
    }
    command
        .arg("--bind")
        .arg(&layout.ssh_control_dir)
        .arg("/codex/ssh-control");
    command
        .arg("--bind")
        .arg(&layout.remote_root_mount)
        .arg("/remote-root");

    if let Some(shared_dir) = &config.shared_dir {
        command.arg("--bind").arg(shared_dir).arg("/shared");
    } else {
        command.arg("--tmpfs").arg("/shared");
    }

    for remote_dir in remote_bind_targets(config, profile) {
        let source = source_for_remote_target(&layout.remote_root_mount, &remote_dir);
        if source.exists() {
            prepare_bind_dir(&mut command, Path::new(&remote_dir));
            command.arg("--bind").arg(source).arg(&remote_dir);
        }
    }

    for target in overlay_target_dirs(config, profile) {
        let source = overlay_source_dir(&layout.exec_overlay_root, &target);
        if source.exists() {
            prepare_bind_dir(&mut command, Path::new(&target));
            command.arg("--ro-bind").arg(source).arg(target);
        }
    }

    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let path_env = format!("/codex/wrappers:{}", profile.remote_path.join(":"));
    let shell_env = format!("/codex/wrappers/{}", profile.shell_name);
    command.arg("--setenv").arg("PATH").arg(&path_env);
    command.arg("--setenv").arg("HOME").arg("/codex/home");
    command.arg("--setenv").arg("SHELL").arg(&shell_env);
    command.arg("--setenv").arg("TERM").arg(term);
    command.arg("--setenv").arg("TMPDIR").arg("/tmp");
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_DESTINATION")
        .arg(config.ssh_destination());
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_PORT")
        .arg(config.port.map(|port| port.to_string()).unwrap_or_default());
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
        .arg("TELEPORT_REMOTE_SHELL")
        .arg(&profile.shell_path);
    command
        .arg("--setenv")
        .arg("TELEPORT_REMOTE_PATH")
        .arg(profile.remote_path.join(":"));
    command
        .arg("--setenv")
        .arg("TELEPORT_IDENTITY_FILE")
        .arg(if config.identity_file.is_some() {
            "/codex/ssh/id_ed25519"
        } else {
            ""
        });
    command
        .arg("--setenv")
        .arg("TELEPORT_KNOWN_HOSTS")
        .arg("/codex/ssh/known_hosts");
    command
        .arg("--setenv")
        .arg("TELEPORT_HOST_SSH")
        .arg("/codex/host-bin/ssh");
    command
        .arg("--setenv")
        .arg("TELEPORT_SSH_CONTROL_PATH")
        .arg("/codex/ssh-control/control.sock");
    command
        .arg("--setenv")
        .arg("TELEPORT_BATCH_MODE")
        .arg(if config.batch_mode { "1" } else { "0" });
    command
        .arg("--setenv")
        .arg("TELEPORT_SSH_OPTIONS")
        .arg(config.ssh_options.join("\n"));
    command
        .arg("--setenv")
        .arg("TELEPORT_SHARED_DIR")
        .arg(if config.shared_dir.is_some() {
            "/shared"
        } else {
            ""
        });

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
            command.arg("--").arg(program).args(args);
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
            command
                .arg("--ro-bind")
                .arg(node_binary)
                .arg("/codex/bin/node");
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
    if let Some(identity_file) = &config.identity_file {
        fs::copy(identity_file, &layout.sandbox_identity_file).with_context(|| {
            format!(
                "failed to copy identity file into runtime dir: {}",
                identity_file.display()
            )
        })?;
        set_file_mode(&layout.sandbox_identity_file, 0o600)?;
    }
    if let Some(local_known_hosts) = local_known_hosts_path() {
        if local_known_hosts.exists() {
            fs::copy(&local_known_hosts, &layout.sandbox_known_hosts).with_context(|| {
                format!(
                    "failed to copy local known_hosts into runtime dir: {}",
                    local_known_hosts.display()
                )
            })?;
        } else {
            fs::write(&layout.sandbox_known_hosts, "")
                .context("failed to create known_hosts file")?;
        }
    } else {
        fs::write(&layout.sandbox_known_hosts, "").context("failed to create known_hosts file")?;
    }
    set_file_mode(&layout.sandbox_known_hosts, 0o600)?;
    write_sshfs_command(config, layout)?;
    Ok(())
}

fn persist_ssh_material(layout: &RuntimeLayout) -> Result<()> {
    let Some(local_known_hosts) = local_known_hosts_path() else {
        return Ok(());
    };
    if let Some(parent) = local_known_hosts.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create parent directory for {}",
                local_known_hosts.display()
            )
        })?;
    }
    fs::copy(&layout.sandbox_known_hosts, &local_known_hosts).with_context(|| {
        format!(
            "failed to persist runtime known_hosts to {}",
            local_known_hosts.display()
        )
    })?;
    set_file_mode(&local_known_hosts, 0o600)
}

fn stage_codex_home(layout: &RuntimeLayout) -> Result<()> {
    let target = layout.codex_home.join(".codex");
    fs::create_dir_all(target.join("shell_snapshots")).with_context(|| {
        format!(
            "failed to create {}",
            target.join("shell_snapshots").display()
        )
    })?;
    fs::create_dir_all(target.join("sessions"))
        .with_context(|| format!("failed to create {}", target.join("sessions").display()))?;
    let Some(local_codex_home) = local_codex_home() else {
        return Ok(());
    };
    copy_dir_recursive(&local_codex_home, &target)
}

fn start_ssh_master(config: &TeleportConfig, layout: &RuntimeLayout) -> Result<SshMasterGuard> {
    let mut command = ssh_master_command(config, layout);
    command.arg("-o").arg("ControlMaster=yes");
    command.arg("-o").arg(format!(
        "ControlPath={}",
        layout.ssh_control_socket.display()
    ));
    command.arg("-o").arg("ControlPersist=no");
    command.arg("-N").arg("-f");

    let status = command
        .status()
        .context("failed to start shared SSH master connection")?;
    if !status.success() {
        bail!("failed to establish shared SSH master connection: {status}");
    }

    Ok(SshMasterGuard {
        destination: config.ssh_destination(),
        port: config.port,
        identity_file: config
            .identity_file
            .as_ref()
            .map(|_| layout.sandbox_identity_file.clone()),
        known_hosts: layout.sandbox_known_hosts.clone(),
        control_socket: layout.ssh_control_socket.clone(),
        batch_mode: config.batch_mode,
        ssh_options: config.ssh_options.clone(),
    })
}

fn write_sshfs_command(config: &TeleportConfig, layout: &RuntimeLayout) -> Result<()> {
    let host_ssh = resolve_local_binary(None, "ssh", "could not find local `ssh` binary")?;
    let mut script = String::from("#!/bin/sh\nexec ");
    script.push_str(&shell_escape(host_ssh.to_string_lossy().as_ref()));
    script.push(' ');
    script.push_str(&shell_escape("-F"));
    script.push(' ');
    script.push_str(&shell_escape("/dev/null"));
    script.push(' ');
    script.push_str(&shell_escape("-S"));
    script.push(' ');
    script.push_str(&shell_escape(
        layout.ssh_control_socket.to_string_lossy().as_ref(),
    ));
    script.push(' ');
    script.push_str(&shell_escape("-o"));
    script.push(' ');
    script.push_str(&shell_escape("ControlMaster=auto"));
    script.push(' ');
    script.push_str(&shell_escape("-o"));
    script.push(' ');
    script.push_str(&shell_escape("ControlPersist=no"));
    if config.identity_file.is_some() {
        script.push(' ');
        script.push_str(&shell_escape("-i"));
        script.push(' ');
        script.push_str(&shell_escape(
            layout.sandbox_identity_file.to_string_lossy().as_ref(),
        ));
    }
    if let Some(port) = config.port {
        script.push(' ');
        script.push_str(&shell_escape("-p"));
        script.push(' ');
        script.push_str(&shell_escape(&port.to_string()));
    }
    if config.batch_mode {
        script.push(' ');
        script.push_str(&shell_escape("-o"));
        script.push(' ');
        script.push_str(&shell_escape("BatchMode=yes"));
    }
    script.push(' ');
    script.push_str(&shell_escape("-o"));
    script.push(' ');
    script.push_str(&shell_escape("StrictHostKeyChecking=accept-new"));
    script.push(' ');
    script.push_str(&shell_escape("-o"));
    script.push(' ');
    script.push_str(&shell_escape(&format!(
        "UserKnownHostsFile={}",
        layout.sandbox_known_hosts.display()
    )));
    for ssh_option in &config.ssh_options {
        script.push(' ');
        script.push_str(&shell_escape("-o"));
        script.push(' ');
        script.push_str(&shell_escape(ssh_option));
    }
    script.push_str(" \"$@\"\n");
    fs::write(&layout.sshfs_ssh_command, script).with_context(|| {
        format!(
            "failed to write sshfs ssh helper {}",
            layout.sshfs_ssh_command.display()
        )
    })?;
    set_file_mode(&layout.sshfs_ssh_command, 0o700)
}

fn wrapped_command_names(profile: &RemoteProfile) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    commands.insert(profile.shell_name.clone());
    for names in profile.exec_names_by_dir.values() {
        commands.extend(names.iter().cloned());
    }
    commands.extend(
        SANDBOX_HELPER_COMMANDS
            .iter()
            .map(|name| (*name).to_string()),
    );
    commands
}

fn prepare_codex_args(mut user_args: Vec<String>) -> Vec<String> {
    if !user_args
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
    {
        user_args.insert(0, "--dangerously-bypass-approvals-and-sandbox".to_string());
    }
    user_args
}

fn remote_bind_targets(config: &TeleportConfig, profile: &RemoteProfile) -> Vec<String> {
    let mut targets = base_remote_bind_targets(config);
    for remote_exec_dir in profile.exec_names_by_dir.keys() {
        if !targets
            .iter()
            .any(|prefix| path_is_under(remote_exec_dir, prefix))
        {
            targets.push(remote_exec_dir.clone());
        }
    }
    for remote_bin_dir in &config.remote_bin_dirs {
        if !targets
            .iter()
            .any(|prefix| path_is_under(remote_bin_dir, prefix))
        {
            targets.push(remote_bin_dir.clone());
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

fn base_remote_bind_targets(config: &TeleportConfig) -> Vec<String> {
    let mut targets = REMOTE_BIND_DIRS
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    if !targets
        .iter()
        .any(|prefix| path_is_under(&config.remote_cwd, prefix))
    {
        targets.push(config.remote_cwd.clone());
    }
    targets.sort();
    targets.dedup();
    targets
}

fn overlay_target_dirs(config: &TeleportConfig, profile: &RemoteProfile) -> Vec<String> {
    let mut targets = profile
        .exec_names_by_dir
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for remote_bin_dir in &config.remote_bin_dirs {
        if !targets.iter().any(|target| target == remote_bin_dir) {
            targets.push(remote_bin_dir.clone());
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

fn overlay_source_dir(exec_overlay_root: &Path, target: &str) -> PathBuf {
    let relative = target.trim_start_matches('/');
    exec_overlay_root.join(relative)
}

fn source_for_remote_target(remote_root_mount: &Path, target: &str) -> PathBuf {
    if target == "/" {
        return remote_root_mount.to_path_buf();
    }
    let relative = target.trim_start_matches('/');
    remote_root_mount.join(relative)
}

fn start_dir_for_bwrap(config: &TeleportConfig) -> String {
    if base_remote_bind_targets(config)
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

fn local_known_hosts_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

#[derive(Debug)]
struct SshMasterGuard {
    destination: String,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    known_hosts: PathBuf,
    control_socket: PathBuf,
    batch_mode: bool,
    ssh_options: Vec<String>,
}

impl Drop for SshMasterGuard {
    fn drop(&mut self) {
        let host_ssh = find_in_path("ssh").unwrap_or_else(|| PathBuf::from("ssh"));
        let mut command = Command::new(host_ssh);
        command.arg("-F").arg("/dev/null");
        if let Some(identity_file) = &self.identity_file {
            command.arg("-i").arg(identity_file);
        }
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if self.batch_mode {
            command.arg("-o").arg("BatchMode=yes");
        }
        command
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .arg("-S")
            .arg(&self.control_socket);
        for ssh_option in &self.ssh_options {
            command.arg("-o").arg(ssh_option);
        }
        let _ = command
            .arg("-O")
            .arg("exit")
            .arg(&self.destination)
            .status();
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry_path, &target_path)?;
            continue;
        }
        fs::copy(&entry_path, &target_path).with_context(|| {
            format!(
                "failed to copy {} to {}",
                entry_path.display(),
                target_path.display()
            )
        })?;
    }
    Ok(())
}

fn resolve_local_binary(explicit: Option<&Path>, name: &str, message: &str) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return fs::canonicalize(path)
            .with_context(|| format!("failed to resolve `{}`", path.display()));
    }
    let path = find_in_path(name).ok_or_else(|| anyhow!(message.to_string()))?;
    fs::canonicalize(&path).with_context(|| format!("failed to resolve `{}`", path.display()))
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
            user: Some("root".to_string()),
            port: Some(22),
            identity_file: Some(PathBuf::from("/tmp/id")),
            ssh_options: vec![],
            batch_mode: false,
            remote_cwd: "/workspace/demo".to_string(),
            remote_home: "/root".to_string(),
            remote_shell: None,
            remote_bin_dirs: vec![],
            shared_dir: None,
        };
        let targets = base_remote_bind_targets(&config);
        assert!(targets.contains(&"/workspace/demo".to_string()));
        assert!(targets.contains(&"/root".to_string()));
    }

    #[test]
    fn split_remote_path_uses_fallback_when_empty() {
        let parsed = split_remote_path("");
        assert!(!parsed.is_empty());
        assert!(parsed.iter().any(|entry| entry == "/usr/bin"));
    }

    #[test]
    fn wrapped_commands_follow_remote_profile_only() {
        let mut exec_names_by_dir = BTreeMap::new();
        exec_names_by_dir.insert(
            "/usr/bin".to_string(),
            ["sh", "grep", "ssh"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        let profile = RemoteProfile {
            shell_path: "/bin/sh".to_string(),
            shell_name: "sh".to_string(),
            remote_path: vec!["/usr/bin".to_string()],
            exec_names_by_dir,
            busybox_like: false,
        };

        let wrapped = wrapped_command_names(&profile);
        assert!(wrapped.contains("sh"));
        assert!(wrapped.contains("grep"));
        assert!(wrapped.contains("ssh"));
        assert!(!wrapped.contains("apt-get"));
    }

    #[test]
    fn wrapped_commands_keep_remote_codex_when_present() {
        let mut exec_names_by_dir = BTreeMap::new();
        exec_names_by_dir.insert(
            "/usr/local/bin".to_string(),
            ["codex", "node"].into_iter().map(str::to_string).collect(),
        );
        let profile = RemoteProfile {
            shell_path: "/bin/sh".to_string(),
            shell_name: "sh".to_string(),
            remote_path: vec!["/usr/local/bin".to_string()],
            exec_names_by_dir,
            busybox_like: false,
        };

        let wrapped = wrapped_command_names(&profile);
        assert!(wrapped.contains("codex"));
        assert!(wrapped.contains("node"));
    }

    #[test]
    fn codex_args_get_bypass_flag_by_default() {
        let args = prepare_codex_args(vec![
            "exec".to_string(),
            "-C".to_string(),
            "/root".to_string(),
        ]);
        assert_eq!(args[0], "--dangerously-bypass-approvals-and-sandbox");
    }

    #[test]
    fn codex_args_do_not_duplicate_bypass_flag() {
        let args = prepare_codex_args(vec![
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "exec".to_string(),
        ]);
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--dangerously-bypass-approvals-and-sandbox")
                .count(),
            1
        );
    }
}
