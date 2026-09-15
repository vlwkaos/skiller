use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::model::{ProjectConfig, validate_schema};
use crate::paths::{
    copy_tree, ensure_real_dir, project_config_path, project_state_root, read_json,
    read_json_or_default, safe_remove_owned_dir, validate_managed_json_path, write_json_atomic,
};

pub(crate) struct ProjectConfigLoad {
    pub(crate) path: PathBuf,
    pub(crate) manifest: ProjectConfig,
    pub(crate) migration: Vec<String>,
    pub(crate) legacy: bool,
}

pub(crate) fn load_project_config(
    project_root: &Path,
    migrate: bool,
    required: bool,
) -> Result<ProjectConfigLoad> {
    let path = project_config_path(project_root)?;
    let legacy_path = project_root.join("skiller.config.json");
    let legacy = !path.exists() && legacy_path.exists();
    let migration = if migrate {
        migrate_project_storage(project_root)?
    } else {
        Vec::new()
    };
    let source = if path.exists() {
        path.as_path()
    } else if legacy_path.exists() {
        legacy_path.as_path()
    } else if required {
        bail!("run `skiller config` before installing")
    } else {
        let manifest = ProjectConfig::default();
        return Ok(ProjectConfigLoad {
            path,
            manifest,
            migration,
            legacy: false,
        });
    };
    let manifest: ProjectConfig = read_json_or_default(source)?;
    validate_schema(manifest.version, "skill config")?;
    Ok(ProjectConfigLoad {
        path,
        manifest,
        migration,
        legacy,
    })
}

pub(crate) fn migrate_project_storage(project_root: &Path) -> Result<Vec<String>> {
    let config_path = project_config_path(project_root)?;
    let state_root = project_state_root(project_root)?;
    let legacy_config = project_root.join("skiller.config.json");
    let legacy_state_root = project_root.join(".skiller");
    let mut messages = Vec::new();

    if legacy_config.exists() {
        let metadata = std::fs::symlink_metadata(&legacy_config)
            .with_context(|| format!("inspecting {}", legacy_config.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("legacy project configuration must be a real file");
        }
        let legacy: ProjectConfig = read_json(&legacy_config)?;
        validate_schema(legacy.version, "legacy project skill config")?;
        if config_path.exists() {
            let current: ProjectConfig = read_json(&config_path)?;
            if current == legacy && !legacy_config_is_tracked(project_root)? {
                std::fs::remove_file(&legacy_config)
                    .with_context(|| format!("removing {}", legacy_config.display()))?;
                messages.push("Removed matching legacy project configuration".to_owned());
            } else {
                messages.push(
                    "Legacy tracked or divergent skiller.config.json is ignored; remove it after review"
                        .to_owned(),
                );
            }
        } else {
            validate_managed_json_path(&config_path)?;
            write_json_atomic(&config_path, &legacy)?;
            messages.push(format!(
                "Migrated project configuration to {}",
                config_path.display()
            ));
            if legacy_config_is_tracked(project_root)? {
                messages.push(
                    "Legacy tracked skiller.config.json is now ignored; remove it from the repository after review"
                        .to_owned(),
                );
            } else {
                std::fs::remove_file(&legacy_config)
                    .with_context(|| format!("removing {}", legacy_config.display()))?;
                messages.push("Removed legacy project configuration".to_owned());
            }
        }
    }

    if legacy_state_root.exists() {
        let metadata = std::fs::symlink_metadata(&legacy_state_root)
            .with_context(|| format!("inspecting {}", legacy_state_root.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("legacy project state must be a real directory");
        }
        ensure_real_dir(&state_root)?;
        for name in ["installed.json", "transaction.json"] {
            migrate_entry(&legacy_state_root, &state_root, name, &mut messages)?;
        }
        let prepared_names = std::fs::read_dir(&legacy_state_root)
            .with_context(|| format!("reading {}", legacy_state_root.display()))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()?;
        for name in prepared_names
            .into_iter()
            .filter(|name| name.starts_with("prepared-"))
        {
            let source = legacy_state_root.join(&name);
            if owned_prepared_directory(&source)? {
                migrate_entry(&legacy_state_root, &state_root, &name, &mut messages)?;
            } else {
                messages.push(format!(
                    "Legacy project state {name} is not ownership-marked and was left for review"
                ));
            }
        }
        if std::fs::read_dir(&legacy_state_root)?.next().is_none() {
            std::fs::remove_dir(&legacy_state_root)
                .with_context(|| format!("removing {}", legacy_state_root.display()))?;
            messages.push("Removed legacy project state directory".to_owned());
        }
    }

    Ok(messages)
}

fn migrate_entry(
    legacy_root: &Path,
    state_root: &Path,
    name: &str,
    messages: &mut Vec<String>,
) -> Result<()> {
    let source = legacy_root.join(name);
    let destination = state_root.join(name);
    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "legacy project state contains a symlink: {}",
            source.display()
        );
    }
    if let Ok(destination_metadata) = std::fs::symlink_metadata(&destination) {
        if destination_metadata.file_type().is_symlink() {
            bail!(
                "current project state contains a symlink: {}",
                destination.display()
            );
        }
        if metadata.is_file() && destination_metadata.is_file() {
            let source_value: serde_json::Value = read_json(&source)?;
            let destination_value: serde_json::Value = read_json(&destination)?;
            if source_value == destination_value {
                std::fs::remove_file(&source)?;
                messages.push(format!("Removed matching legacy project state {name}"));
                return Ok(());
            }
        }
        messages.push(format!(
            "Legacy project state {name} differs from current state and was left for review"
        ));
        return Ok(());
    }
    if metadata.is_file() {
        let value: serde_json::Value = read_json(&source)?;
        write_json_atomic(&destination, &value)?;
        std::fs::remove_file(&source)?;
    } else if metadata.is_dir() {
        if let Err(error) = copy_tree(&source, &destination) {
            let _ = safe_remove_owned_dir(&destination, state_root);
            return Err(error);
        }
        safe_remove_owned_dir(&source, legacy_root)?;
    } else {
        bail!(
            "legacy project state is not a file or directory: {}",
            source.display()
        );
    }
    messages.push(format!("Migrated project state {name}"));
    Ok(())
}

pub(crate) fn legacy_state_is_fully_migratable(project_root: &Path) -> Result<bool> {
    let root = project_root.join(".skiller");
    let metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", root.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("legacy project state must be a real directory");
    }
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "installed.json" || name == "transaction.json" {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || read_json::<serde_json::Value>(&entry.path()).is_err()
            {
                return Ok(false);
            }
            continue;
        }
        if !name.starts_with("prepared-") || !owned_prepared_directory(&entry.path())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn owned_prepared_directory(path: &Path) -> Result<bool> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let marker = path.join(".skiller-owned");
    if !std::fs::symlink_metadata(marker)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    {
        return Ok(false);
    }
    copyable_tree(path)
}

fn copyable_tree(path: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        if metadata.is_dir() {
            if !copyable_tree(&entry.path())? {
                return Ok(false);
            }
        } else if !metadata.is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn legacy_config_is_tracked(project_root: &Path) -> Result<bool> {
    let status = Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", "skiller.config.json"])
        .current_dir(project_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("checking whether legacy project configuration is tracked")?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::InstalledState;

    fn git_repository(name: &str) -> PathBuf {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        root
    }

    #[test]
    fn migration_moves_untracked_policy_and_worktree_state_once() {
        let root = git_repository("project-store-migration");
        write_json_atomic(&root.join("skiller.config.json"), &ProjectConfig::default()).unwrap();
        write_json_atomic(
            &root.join(".skiller/installed.json"),
            &InstalledState::default(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".skiller/prepared-current/skills/demo")).unwrap();
        std::fs::write(root.join(".skiller/prepared-current/.skiller-owned"), "1\n").unwrap();
        std::fs::write(
            root.join(".skiller/prepared-current/skills/demo/SKILL.md"),
            "demo\n",
        )
        .unwrap();

        let messages = migrate_project_storage(&root).unwrap();
        assert!(project_config_path(&root).unwrap().is_file());
        assert!(
            project_state_root(&root)
                .unwrap()
                .join("installed.json")
                .is_file()
        );
        assert!(
            project_state_root(&root)
                .unwrap()
                .join("prepared-current/skills/demo/SKILL.md")
                .is_file()
        );
        assert!(!root.join("skiller.config.json").exists());
        assert!(!root.join(".skiller").exists());
        assert!(!messages.is_empty());
        assert!(migrate_project_storage(&root).unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migration_retry_compares_existing_json_semantically() {
        let root = git_repository("project-store-semantic-retry");
        let legacy = root.join(".skiller/installed.json");
        ensure_real_dir(legacy.parent().unwrap()).unwrap();
        let state = InstalledState::default();
        std::fs::write(&legacy, serde_json::to_string(&state).unwrap()).unwrap();
        let current = project_state_root(&root).unwrap().join("installed.json");
        write_json_atomic(&current, &state).unwrap();

        let messages = migrate_project_storage(&root).unwrap();
        assert!(!legacy.exists());
        assert!(
            messages
                .iter()
                .any(|message| message.contains("Removed matching"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn legacy_state_reports_unknown_content_as_manual_review() {
        let root = git_repository("project-store-unknown-state");
        std::fs::create_dir_all(root.join(".skiller")).unwrap();
        std::fs::write(root.join(".skiller/unowned.txt"), "keep\n").unwrap();
        assert!(!legacy_state_is_fully_migratable(&root).unwrap());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_state_fixability_rejects_symlinks_before_repair() {
        use std::os::unix::fs::symlink;

        let root = git_repository("project-store-unrepairable-symlinks");
        let legacy_root = root.join(".skiller");
        ensure_real_dir(&legacy_root).unwrap();
        let outside = root.join("outside.json");
        std::fs::write(&outside, "{}\n").unwrap();
        symlink(&outside, legacy_root.join("installed.json")).unwrap();
        assert!(!legacy_state_is_fully_migratable(&root).unwrap());

        std::fs::remove_file(legacy_root.join("installed.json")).unwrap();
        let prepared = legacy_root.join("prepared-current");
        ensure_real_dir(&prepared).unwrap();
        std::fs::write(prepared.join(".skiller-owned"), "1\n").unwrap();
        symlink(&outside, prepared.join("nested.json")).unwrap();
        assert!(!legacy_state_is_fully_migratable(&root).unwrap());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_a_symlinked_current_state_destination() {
        use std::os::unix::fs::symlink;

        let root = git_repository("project-store-state-symlink");
        let legacy = root.join(".skiller/installed.json");
        write_json_atomic(&legacy, &InstalledState::default()).unwrap();
        let state_root = project_state_root(&root).unwrap();
        ensure_real_dir(&state_root).unwrap();
        let outside = root.join("outside.json");
        std::fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, state_root.join("installed.json")).unwrap();

        assert!(migrate_project_storage(&root).is_err());
        assert!(legacy.is_file());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migration_preserves_tracked_legacy_policy_without_reading_it_again() {
        let root = git_repository("project-store-tracked-policy");
        write_json_atomic(&root.join("skiller.config.json"), &ProjectConfig::default()).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "skiller.config.json"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );

        migrate_project_storage(&root).unwrap();
        assert!(root.join("skiller.config.json").is_file());
        let central = project_config_path(&root).unwrap();
        let changed = ProjectConfig {
            agents: vec!["pi".to_owned()],
            ..ProjectConfig::default()
        };
        write_json_atomic(&central, &changed).unwrap();
        assert_eq!(
            load_project_config(&root, false, true).unwrap().manifest,
            changed
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
