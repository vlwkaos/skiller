use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::{Config, Target};

pub fn builtin_paths() -> HashMap<&'static str, PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    HashMap::from([
        ("claude",    home.join(".claude/skills")),
        ("codex",     home.join(".codex/skills")),
        ("opencode",  home.join(".config/opencode/skills")),
        ("openclaw",  home.join(".openclaw/skills")),
    ])
}

pub fn cmd_add(cfg: &mut Config, type_name: &str, path: Option<&str>) -> Result<()> {
    if cfg.targets.iter().any(|t| t.r#type == type_name) {
        bail!("target '{}' already exists", type_name);
    }

    let resolved = if let Some(p) = path {
        PathBuf::from(shellexpand::tilde(p).as_ref())
    } else {
        let builtins = builtin_paths();
        builtins
            .get(type_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(
                "unknown type '{}' — provide a path or use a built-in: {}",
                type_name,
                builtin_paths().keys().cloned().collect::<Vec<_>>().join(", ")
            ))?
    };

    cfg.targets.push(Target { r#type: type_name.to_owned(), path: resolved.clone() });
    cfg.save()?;
    println!("added target '{}' → {}", type_name, resolved.display());
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
        let status = link_status(&t.path, source);
        println!("{:<12} {}  [{}]", t.r#type, t.path.display(), status);
    }
    Ok(())
}

pub fn link_status(target: &std::path::Path, source: Option<&std::path::Path>) -> &'static str {
    match target.symlink_metadata() {
        Err(_) => "not linked",
        Ok(m) if m.file_type().is_symlink() => {
            if let (Ok(dst), Some(src)) = (std::fs::read_link(target), source) {
                if dst == src { "linked" } else { "conflict (wrong link)" }
            } else {
                "linked (unknown source)"
            }
        }
        Ok(_) => "conflict (real dir)",
    }
}
