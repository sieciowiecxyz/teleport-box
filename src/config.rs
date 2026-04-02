use std::fs;
use std::path::PathBuf;
use std::{collections::BTreeMap, collections::BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use tempfile::TempDir;

pub const LOCAL_ONLY_PREFIXES: &[&str] = &[
    "/codex", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/sys", "/dev", "/run",
    "/tmp", "/shared",
];

pub const REMOTE_BIND_DIRS: &[&str] = &[
    "/root",
    "/home",
    "/opt",
    "/srv",
    "/mnt",
    "/media",
    "/usr/local",
    "/var/tmp",
];

pub const SANDBOX_HELPER_COMMANDS: &[&str] = &["teleport-pull", "teleport-push"];

const HELP_TEMPLATE: &str = "\
{before-help}{name} {version}
{about}

Usage:
  {usage}

Commands:
{subcommands}

Options:
  {options}

{after-help}
";

const AFTER_HELP: &str = "\
Examples:
  teleport-box doctor root@host --identity-file ~/.ssh/id_ed25519
  teleport-box shell root@host
  teleport-box exec root@host -- sh -c 'uname -a'
  teleport-box codex root@host -- --dangerously-bypass-approvals-and-sandbox exec -C /root \"Run uname -a\"
";

#[derive(Parser, Debug)]
#[command(name = "teleport-box")]
#[command(about = "Run local binaries inside a sandbox backed by a remote host over SSH + SSHFS.")]
#[command(help_template = HELP_TEMPLATE)]
#[command(after_help = AFTER_HELP)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Probe the remote host profile and validate SSHFS mounting.")]
    Doctor(DoctorArgs),
    #[command(about = "Launch an interactive remote-flavored shell inside the teleport sandbox.")]
    Shell(ShellArgs),
    #[command(about = "Run an arbitrary command inside the teleport sandbox.")]
    Exec(ExecArgs),
    #[command(about = "Run the local Codex CLI inside the teleport sandbox.")]
    Codex(CodexArgs),
}

#[derive(Args, Clone, Debug)]
pub struct ConnectionArgs {
    #[arg(
        value_name = "TARGET",
        help = "[user@]host[:port] or ssh://user@host:port"
    )]
    pub target: Option<String>,
    #[arg(long, hide = true)]
    pub host: Option<String>,
    #[arg(long, hide = true)]
    pub user: Option<String>,
    #[arg(long, hide = true)]
    pub port: Option<u16>,
    #[arg(long)]
    pub identity_file: Option<PathBuf>,
    #[arg(long, action = ArgAction::Append)]
    pub ssh_option: Vec<String>,
    #[arg(long)]
    pub batch_mode: bool,
    #[arg(long, default_value = "/root")]
    pub remote_cwd: String,
    #[arg(long, default_value = "/root")]
    pub remote_home: String,
    #[arg(long)]
    pub remote_shell: Option<String>,
    #[arg(long, action = ArgAction::Append)]
    pub remote_bin_dir: Vec<String>,
    #[arg(long)]
    pub shared_dir: Option<PathBuf>,
}

#[derive(Args, Clone, Debug)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Args, Clone, Debug)]
pub struct ShellArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,
    #[arg(last = true, allow_hyphen_values = true)]
    pub shell_args: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct ExecArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct CodexArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,
    #[arg(long)]
    pub codex_binary: Option<PathBuf>,
    #[arg(last = true, allow_hyphen_values = true)]
    pub codex_args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TeleportConfig {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    pub ssh_options: Vec<String>,
    pub batch_mode: bool,
    pub remote_cwd: String,
    pub remote_home: String,
    pub remote_shell: Option<String>,
    pub remote_bin_dirs: Vec<String>,
    pub shared_dir: Option<PathBuf>,
}

impl TeleportConfig {
    pub fn from_connection(connection: ConnectionArgs) -> Result<Self> {
        if !connection.remote_cwd.starts_with('/') {
            bail!("--remote-cwd must be an absolute path");
        }
        if !connection.remote_home.starts_with('/') {
            bail!("--remote-home must be an absolute path");
        }
        for remote_bin_dir in &connection.remote_bin_dir {
            if !remote_bin_dir.starts_with('/') {
                bail!("--remote-bin-dir must be an absolute path");
            }
        }

        let (target_user, target_host, target_port) = resolve_target_parts(&connection)?;
        let identity_file = match connection.identity_file {
            Some(identity_file) => Some(fs::canonicalize(&identity_file).with_context(|| {
                format!(
                    "failed to resolve identity file: {}",
                    identity_file.display()
                )
            })?),
            None => None,
        };
        let shared_dir = match connection.shared_dir {
            Some(shared_dir) => Some(canonicalize_or_create_dir(shared_dir)?),
            None => None,
        };

        Ok(Self {
            host: target_host,
            user: target_user,
            port: target_port,
            identity_file,
            ssh_options: connection.ssh_option,
            batch_mode: connection.batch_mode,
            remote_cwd: connection.remote_cwd,
            remote_home: connection.remote_home,
            remote_shell: connection.remote_shell,
            remote_bin_dirs: connection.remote_bin_dir,
            shared_dir,
        })
    }

    pub fn ssh_destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    pub fn sshfs_source_root(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match &self.user {
            Some(user) => format!("{user}@{host}:/"),
            None => format!("{host}:/"),
        }
    }
}

fn resolve_target_parts(
    connection: &ConnectionArgs,
) -> Result<(Option<String>, String, Option<u16>)> {
    if connection.target.is_some()
        && (connection.host.is_some() || connection.user.is_some() || connection.port.is_some())
    {
        bail!("do not mix TARGET with legacy --host/--user/--port");
    }

    if let Some(target) = &connection.target {
        return parse_target(target);
    }

    let host = connection
        .host
        .clone()
        .ok_or_else(|| anyhow!("missing TARGET or --host"))?;
    Ok((connection.user.clone(), host, connection.port))
}

fn parse_target(raw: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("TARGET must not be empty");
    }

    let without_scheme = trimmed.strip_prefix("ssh://").unwrap_or(trimmed);
    if let Some((user, host, port)) = parse_bracketed_target(without_scheme)? {
        return Ok((user, host, port));
    }

    let (user, rest) = match without_scheme.split_once('@') {
        Some((user, rest)) if !user.is_empty() && !rest.is_empty() => {
            (Some(user.to_string()), rest.to_string())
        }
        Some(_) => bail!("invalid TARGET: {trimmed}"),
        None => (None, without_scheme.to_string()),
    };

    let (host, port) = split_host_and_port(&rest)?;
    Ok((user, host, port))
}

fn parse_bracketed_target(raw: &str) -> Result<Option<(Option<String>, String, Option<u16>)>> {
    let (user, rest) = match raw.split_once('@') {
        Some((user, rest)) => (Some(user.to_string()), rest),
        None => (None, raw),
    };
    if !rest.starts_with('[') {
        return Ok(None);
    }
    let end = rest
        .find(']')
        .ok_or_else(|| anyhow!("invalid bracketed TARGET: {raw}"))?;
    let host = rest[1..end].to_string();
    let suffix = &rest[end + 1..];
    let port = if suffix.is_empty() {
        None
    } else if let Some(port_text) = suffix.strip_prefix(':') {
        Some(
            port_text
                .parse::<u16>()
                .with_context(|| format!("invalid TARGET port: {port_text}"))?,
        )
    } else {
        bail!("invalid bracketed TARGET suffix: {suffix}");
    };
    Ok(Some((user, host, port)))
}

fn split_host_and_port(raw: &str) -> Result<(String, Option<u16>)> {
    if let Some((host, port_text)) = raw.rsplit_once(':') {
        if port_text.chars().all(|ch| ch.is_ascii_digit()) && !host.contains(':') {
            let port = port_text
                .parse::<u16>()
                .with_context(|| format!("invalid TARGET port: {port_text}"))?;
            return Ok((host.to_string(), Some(port)));
        }
    }
    Ok((raw.to_string(), None))
}

fn canonicalize_or_create_dir(path: PathBuf) -> Result<PathBuf> {
    let absolute = absolutize(path)?;
    fs::create_dir_all(&absolute)
        .with_context(|| format!("failed to create shared dir {}", absolute.display()))?;
    fs::canonicalize(&absolute)
        .with_context(|| format!("failed to resolve shared dir {}", absolute.display()))
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current dir")?
        .join(path))
}

pub enum LaunchMode {
    Shell(Vec<String>),
    Arbitrary(Vec<String>),
    Codex {
        codex_package_root: PathBuf,
        node_binary: PathBuf,
        apply_patch_binary: Option<PathBuf>,
        codex_args: Vec<String>,
    },
}

pub enum LaunchSpec {
    Arbitrary {
        program: String,
        args: Vec<String>,
        apply_patch_binary: Option<PathBuf>,
    },
    Codex {
        codex_package_root: PathBuf,
        node_binary: PathBuf,
        apply_patch_binary: Option<PathBuf>,
        codex_args: Vec<String>,
    },
}

#[derive(Debug)]
pub struct RuntimeLayout {
    pub _tempdir: TempDir,
    pub self_binary: PathBuf,
    pub remote_root_mount: PathBuf,
    pub codex_home: PathBuf,
    pub wrapper_dir: PathBuf,
    pub exec_overlay_root: PathBuf,
    pub ssh_control_dir: PathBuf,
    pub ssh_control_socket: PathBuf,
    pub sandbox_identity_file: PathBuf,
    pub sandbox_known_hosts: PathBuf,
    pub sshfs_ssh_command: PathBuf,
}

#[derive(Debug)]
pub struct MountGuard {
    pub mountpoint: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RemoteProfile {
    pub shell_path: String,
    pub shell_name: String,
    pub remote_path: Vec<String>,
    pub exec_names_by_dir: BTreeMap<String, BTreeSet<String>>,
    pub busybox_like: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_target() {
        let (user, host, port) = parse_target("root@example.com:2222").unwrap();
        assert_eq!(user.as_deref(), Some("root"));
        assert_eq!(host, "example.com");
        assert_eq!(port, Some(2222));
    }

    #[test]
    fn parses_alias_target() {
        let (user, host, port) = parse_target("prod-box").unwrap();
        assert_eq!(user, None);
        assert_eq!(host, "prod-box");
        assert_eq!(port, None);
    }

    #[test]
    fn parses_bracketed_ipv6_target() {
        let (user, host, port) = parse_target("root@[2001:db8::1]:2200").unwrap();
        assert_eq!(user.as_deref(), Some("root"));
        assert_eq!(host, "2001:db8::1");
        assert_eq!(port, Some(2200));
    }

    #[test]
    fn sshfs_source_rewraps_ipv6_literals() {
        let config = TeleportConfig {
            host: "2001:db8::1".to_string(),
            user: Some("root".to_string()),
            port: Some(2200),
            identity_file: None,
            ssh_options: vec![],
            batch_mode: false,
            remote_cwd: "/root".to_string(),
            remote_home: "/root".to_string(),
            remote_shell: None,
            remote_bin_dirs: vec![],
            shared_dir: None,
        };

        assert_eq!(config.sshfs_source_root(), "root@[2001:db8::1]:/");
    }
}
