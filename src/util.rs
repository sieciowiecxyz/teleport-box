use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use anyhow::{Context, Result, bail};

pub fn ensure_binary_exists(name: &str) -> Result<()> {
    if find_in_path(name).is_none() {
        bail!("required binary not found on PATH: {name}");
    }
    Ok(())
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() && candidate.exists() {
        return Some(candidate.to_path_buf());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.exists())
}

pub fn path_is_under(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub fn file_name(path: &OsStr) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_string)
}

pub fn prepare_bind_dir(command: &mut Command, target: &Path) {
    let mut prefixes = Vec::new();
    let mut current = PathBuf::new();
    for component in target.components() {
        current.push(component.as_os_str());
        prefixes.push(current.clone());
    }
    for prefix in prefixes {
        if prefix == Path::new("/") {
            continue;
        }
        command.arg("--dir").arg(prefix);
    }
}

pub fn prepare_bind_parent(command: &mut Command, target: &Path) {
    if let Some(parent) = target.parent() {
        prepare_bind_dir(command, parent);
    }
}

pub fn add_ro_bind_if_exists(command: &mut Command, source: &str, target: &str) {
    if Path::new(source).exists() {
        prepare_bind_dir(command, Path::new(target));
        command.arg("--ro-bind").arg(source).arg(target);
    }
}

pub fn bind_absolute_wrapper_if_exists(command: &mut Command, source: &Path, target: &str) {
    if source.exists() && Path::new(target).exists() {
        prepare_bind_parent(command, Path::new(target));
        command.arg("--ro-bind").arg(source).arg(target);
    }
}

pub fn is_executable_path(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    metadata.permissions().mode() & 0o111 != 0
}

pub fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

pub fn propagate_exit_status(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    if let Some(signal) = status.signal() {
        std::process::exit(128 + signal);
    }
    bail!("process exited without code or signal")
}

pub fn exit_code_from_status(status: ExitStatus) -> Result<ExitCode> {
    if let Some(code) = status.code() {
        return Ok(ExitCode::from(code as u8));
    }
    if let Some(signal) = status.signal() {
        return Ok(ExitCode::from((128 + signal) as u8));
    }
    bail!("process exited without code or signal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_under_matches_exact_and_nested() {
        assert!(path_is_under("/root/project", "/root"));
        assert!(path_is_under("/root", "/root"));
        assert!(!path_is_under("/rooted", "/root"));
    }
}
