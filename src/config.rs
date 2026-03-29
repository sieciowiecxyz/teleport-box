use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tempfile::TempDir;

pub const SEED_WRAPPED_COMMANDS: &[&str] = &[
    "sh", "bash", "python", "python3", "git", "rg", "ls", "cat", "find", "grep", "sed", "awk",
    "env", "which", "pwd", "uname", "ip", "apt", "apt-get", "dpkg", "mkdir", "rm", "cp", "mv",
    "touch", "tar", "tee", "head", "tail", "wc", "sort",
];

pub const ABSOLUTE_WRAP_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
];

pub const ABSOLUTE_WRAP_COMMANDS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "env", "python", "python3", "git", "rg", "ls", "cat",
    "find", "grep", "sed", "awk", "uname", "ip", "apt", "apt-get", "dpkg",
];

pub const LOCAL_ONLY_PREFIXES: &[&str] = &[
    "/codex", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/sys", "/dev",
    "/run", "/tmp",
];

pub const REMOTE_BIND_DIRS: &[&str] = &[
    "/root", "/home", "/opt", "/srv", "/mnt", "/media", "/usr/local", "/var/tmp",
];

#[derive(Parser, Debug)]
#[command(name = "teleport-box")]
#[command(about = "Run local binaries inside a sandbox backed by a remote host over SSH + SSHFS.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Exec(ExecArgs),
    Codex(CodexArgs),
}

#[derive(Args, Clone, Debug)]
pub struct ConnectionArgs {
    #[arg(long)]
    pub host: String,
    #[arg(long)]
    pub user: String,
    #[arg(long, default_value_t = 22)]
    pub port: u16,
    #[arg(long)]
    pub identity_file: PathBuf,
    #[arg(long, default_value = "/root")]
    pub remote_cwd: String,
    #[arg(long, default_value = "/root")]
    pub remote_home: String,
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
    pub user: String,
    pub port: u16,
    pub identity_file: PathBuf,
    pub remote_cwd: String,
    pub remote_home: String,
}

impl TeleportConfig {
    pub fn from_connection(connection: ConnectionArgs) -> Result<Self> {
        if !connection.remote_cwd.starts_with('/') {
            bail!("--remote-cwd must be an absolute path");
        }
        if !connection.remote_home.starts_with('/') {
            bail!("--remote-home must be an absolute path");
        }
        let identity_file = fs::canonicalize(&connection.identity_file).with_context(|| {
            format!(
                "failed to resolve identity file: {}",
                connection.identity_file.display()
            )
        })?;
        Ok(Self {
            host: connection.host,
            user: connection.user,
            port: connection.port,
            identity_file,
            remote_cwd: connection.remote_cwd,
            remote_home: connection.remote_home,
        })
    }
}

pub enum LaunchMode {
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
    pub ssh_dir: PathBuf,
    pub sandbox_identity_file: PathBuf,
    pub sandbox_known_hosts: PathBuf,
}

#[derive(Debug)]
pub struct MountGuard {
    pub mountpoint: PathBuf,
}
