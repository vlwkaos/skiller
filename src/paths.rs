use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn config_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("SKILLER_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("skiller"));
    }
    Ok(home_dir()?.join(".config/skiller"))
}

pub fn cache_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("SKILLER_CACHE_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("skiller"));
    }
    Ok(home_dir()?.join(".cache/skiller"))
}

pub fn state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("SKILLER_STATE_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("skiller"));
    }
    Ok(home_dir()?.join(".local/state/skiller"))
}

pub fn global_config_path() -> Result<PathBuf> {
    Ok(config_root()?.join("config.json"))
}

pub fn global_state_path() -> Result<PathBuf> {
    Ok(state_root()?.join("installed.json"))
}

pub fn global_skills_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(".agents/skills"))
}

pub fn project_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("reading current directory")?;
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !value.is_empty() {
            return PathBuf::from(value)
                .canonicalize()
                .context("resolving Git project root");
        }
    }
    cwd.canonicalize().context("resolving project root")
}

pub fn read_json_or_default<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned + Default,
{
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    ensure_real_dir(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing to replace symlinked file: {}", path.display());
    }
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(&bytes)
            .and_then(|_| file.write_all(b"\n"))
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("committing {}", path.display()));
    }
    Ok(())
}

pub fn write_global_config(value: &crate::model::GlobalConfig) -> Result<()> {
    let path = global_config_path()?;
    let destination = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(&path)
                .with_context(|| format!("reading global config symlink {}", path.display()))?;
            if target.is_absolute() {
                target
            } else {
                path.parent()
                    .context("global config symlink has no parent")?
                    .join(target)
            }
        }
        Ok(_) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(error) => return Err(error).context("inspecting global config path"),
    };
    write_json_atomic(&destination, value)
}

pub fn ensure_real_dir(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("expected a real directory: {}", path.display());
        }
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_real_dir(parent)?;
    }
    std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
}

pub fn safe_remove_owned_dir(path: &Path, allowed_parent: &Path) -> Result<()> {
    let parent = path.parent().context("owned path has no parent")?;
    if parent != allowed_parent || path.file_name().is_none() {
        bail!(
            "refusing to remove path outside managed root: {}",
            path.display()
        );
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to remove symlinked managed path: {}",
                path.display()
            )
        }
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => bail!("managed path is not a directory: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspecting {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "skill source must be a real directory: {}",
            source.display()
        );
    }
    ensure_real_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "skill contains an unsupported symlink: {}",
                source_path.display()
            );
        }
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &destination_path)
                .with_context(|| format!("copying {}", source_path.display()))?;
        }
    }
    Ok(())
}

pub fn sanitize_child_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '�'
            }
        })
        .collect()
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not configured")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_output_strips_terminal_controls() {
        assert_eq!(
            sanitize_child_output(b"ok\n\x1b]8;;bad\x07link"),
            "ok\n�]8;;bad�link"
        );
    }
}
