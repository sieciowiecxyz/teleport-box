use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::config::{LOCAL_ONLY_PREFIXES, RuntimeLayout, TeleportConfig};
use crate::util::{file_name, propagate_exit_status};

pub fn maybe_run_wrapper() -> Result<bool> {
    let argv0 = env::args_os().next().unwrap_or_else(|| OsString::from("teleport-box"));
    let name = file_name(&argv0).unwrap_or_else(|| "teleport-box".to_string());
    if name == "teleport-box" || env::var_os("TELEPORT_REMOTE_HOST").is_none() {
        return Ok(false);
    }

    let args = env::args_os().skip(1).collect::<Vec<_>>();
    run_wrapper(&name, args)?;
    Ok(true)
}

pub fn run_wrapper(invoked_as: &str, args: Vec<OsString>) -> Result<()> {
    let host = env_required("TELEPORT_REMOTE_HOST")?;
    let user = env_required("TELEPORT_REMOTE_USER")?;
    let port = env_required("TELEPORT_REMOTE_PORT")?;
    let identity_file = env_required("TELEPORT_IDENTITY_FILE")?;
    let known_hosts = env_required("TELEPORT_KNOWN_HOSTS")?;
    let remote_cwd = env_required("TELEPORT_REMOTE_CWD")?;
    let remote_home = env_required("TELEPORT_REMOTE_HOME")?;
    let cwd = remote_cwd_for_current_dir(&remote_cwd)?;
    let remote_path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    let mut remote_argv = Vec::with_capacity(args.len() + 1);
    remote_argv.push(invoked_as.to_string());
    for arg in args {
        let arg = arg
            .into_string()
            .map_err(|_| anyhow!("wrapper argument is not valid UTF-8"))?;
        remote_argv.push(arg);
    }
    let remote_command = build_remote_command(&cwd, &remote_home, remote_path, &remote_argv);
    let mut ssh = Command::new("/usr/bin/ssh");
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        ssh.arg("-tt");
    }
    ssh.arg("-i")
        .arg(identity_file)
        .arg("-F")
        .arg("/dev/null")
        .arg("-p")
        .arg(port)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={known_hosts}"))
        .arg(format!("{user}@{host}"))
        .arg("--")
        .arg("/bin/bash")
        .arg("-lc")
        .arg(remote_command);

    let status = ssh.status().context("failed to execute ssh wrapper")?;
    propagate_exit_status(status)
}

pub fn ssh_command(config: &TeleportConfig, layout: &RuntimeLayout) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command
        .arg("-i")
        .arg(&layout.sandbox_identity_file)
        .arg("-F")
        .arg("/dev/null")
        .arg("-p")
        .arg(config.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            layout.sandbox_known_hosts.display()
        ))
        .arg(format!("{}@{}", config.user, config.host));
    command
}

fn build_remote_command(cwd: &str, remote_home: &str, remote_path: &str, argv: &[String]) -> String {
    let mut command = String::from("cd -- ");
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

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            escaped.push_str("'\"'\"'");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

fn env_required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("missing required environment variable `{name}`"))
}

fn remote_cwd_for_current_dir(default_remote_cwd: &str) -> Result<String> {
    let cwd = env::current_dir().context("failed to resolve current dir")?;
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| anyhow!("current dir is not valid UTF-8"))?;
    if LOCAL_ONLY_PREFIXES
        .iter()
        .any(|prefix| cwd_str == *prefix || cwd_str.starts_with(&format!("{prefix}/")))
    {
        return Ok(default_remote_cwd.to_string());
    }
    Ok(cwd_str.to_string())
}

#[cfg(test)]
mod tests {
    use crate::config::LOCAL_ONLY_PREFIXES;

    #[test]
    fn local_control_plane_paths_fall_back_to_default_remote_cwd() {
        let cwd = "/codex/home";
        assert!(LOCAL_ONLY_PREFIXES
            .iter()
            .any(|prefix| cwd == *prefix || cwd.starts_with(&format!("{prefix}/"))));
    }
}
