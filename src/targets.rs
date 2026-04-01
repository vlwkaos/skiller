use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::{Config, LinkMode, Target};

pub fn builtin_paths() -> HashMap<&'static str, PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    HashMap::from([
        ("claude", home.join(".claude/skills")),
        ("codex", home.join(".codex/skills")),
        ("opencode", home.join(".config/opencode/skills")),
        ("openclaw", home.join(".openclaw/skills")),
        ("hermes", home.join(".hermes/skills/common")),
    ])
}

pub fn cmd_add(
    cfg: &mut Config,
    type_name: &str,
    path: Option<&str>,
    mode: LinkMode,
) -> Result<()> {
    if cfg.targets.iter().any(|t| t.r#type == type_name) {
        bail!("target '{}' already exists", type_name);
    }

    let resolved = if let Some(p) = path {
        PathBuf::from(shellexpand::tilde(p).as_ref())
    } else {
        let builtins = builtin_paths();
        builtins.get(type_name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "unknown type '{}' — provide a path or use a built-in: {}",
                type_name,
                builtin_paths()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?
    };

    cfg.targets.push(Target {
        r#type: type_name.to_owned(),
        path: resolved.clone(),
        mode,
    });
    cfg.save()?;
    println!(
        "added target '{}' [{}] → {}",
        type_name,
        mode,
        resolved.display()
    );
    Ok(())
}

pub fn cmd_remove(cfg: &mut Config, type_name: &str) -> Result<()> {
    let before = cfg.targets.len();
    cfg.targets.retain(|t| t.r#type != type_name);
    if cfg.targets.len() == before {
        bail!("target '{}' not found", type_name);
    }
    cfg.save()?;
    println!("removed target '{}'", type_name);
    Ok(())
}

pub fn cmd_list(cfg: &Config) -> Result<()> {
    if cfg.targets.is_empty() {
        println!("no targets configured");
        return Ok(());
    }
    let source = cfg.source.as_deref();
    for t in &cfg.targets {
        let status = link_status(t, source);
        println!(
            "{:<12} {:<10} {}  [{}]",
            t.r#type,
            t.mode,
            t.path.display(),
            status
        );
    }
    Ok(())
}

pub fn link_status(target: &Target, source: Option<&std::path::Path>) -> String {
    match target.mode {
        LinkMode::Folder => folder_link_status(&target.path, source).to_owned(),
        LinkMode::Granular => granular_link_status(target, source),
    }
}

fn folder_link_status(target: &std::path::Path, source: Option<&std::path::Path>) -> &'static str {
    match target.symlink_metadata() {
        Err(_) => "not linked",
        Ok(m) if m.file_type().is_symlink() => {
            if let (Ok(dst), Some(src)) = (std::fs::read_link(target), source) {
                if dst == src {
                    "linked"
                } else {
                    "conflict (wrong link)"
                }
            } else {
                "linked (unknown source)"
            }
        }
        Ok(_) => "conflict (real dir)",
    }
}

fn granular_link_status(target: &Target, source: Option<&std::path::Path>) -> String {
    let Some(source) = source else {
        return "source not set".to_owned();
    };

    match target.path.symlink_metadata() {
        Err(_) => return "not linked".to_owned(),
        Ok(meta) if meta.file_type().is_symlink() => {
            return "conflict (target is symlink; expected dir)".to_owned();
        }
        Ok(meta) if !meta.is_dir() => {
            return "conflict (target is not a dir)".to_owned();
        }
        Ok(_) => {}
    }

    let source_entries = match skill_entries(source) {
        Ok(entries) => entries,
        Err(err) => return format!("error reading source: {err}"),
    };

    if source_entries.is_empty() {
        return "granular 0/0 linked".to_owned();
    }

    let total = source_entries.len();
    let mut linked = 0usize;
    let mut conflicts = 0usize;
    let mut missing = 0usize;

    for src in &source_entries {
        let name = match src.file_name() {
            Some(name) => name,
            None => continue,
        };
        let dest = target.path.join(name);
        match dest.symlink_metadata() {
            Err(_) => missing += 1,
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&dest) {
                Ok(dst) if dst == src.as_path() => linked += 1,
                Ok(_) | Err(_) => conflicts += 1,
            },
            Ok(_) => conflicts += 1,
        }
    }

    format!(
        "granular {linked}/{} linked, {conflicts} conflict, {missing} missing",
        total
    )
}

pub fn skill_entries(source: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let is_skill_tree = if path.is_dir() {
            contains_skill_markdown(&path)
        } else if entry.file_type()?.is_symlink() {
            std::fs::metadata(&path)
                .map(|meta| meta.is_dir() && contains_skill_markdown(&path))
                .unwrap_or(false)
        } else {
            false
        };
        if is_skill_tree {
            entries.push(path);
        }
    }
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(entries)
}

fn contains_skill_markdown(path: &std::path::Path) -> bool {
    if path.join("SKILL.md").exists() {
        return true;
    }

    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };

    for entry in entries.flatten() {
        let child = entry.path();
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir() || ft.is_symlink())
            .unwrap_or(false);
        if is_dir && contains_skill_markdown(&child) {
            return true;
        }
    }

    false
}
