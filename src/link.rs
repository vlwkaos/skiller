use anyhow::{bail, Context, Result};
use std::io::{self, Write};
use std::path::Path;

use crate::config::Config;
use crate::targets::link_status;

pub fn cmd_link(cfg: &Config) -> Result<()> {
    let source = cfg.source_dir()?;
    if cfg.targets.is_empty() {
        bail!("no targets configured — run: skiller target add <type>");
    }
    for t in &cfg.targets {
        link_one(source, &t.path, &t.r#type)?;
    }
    Ok(())
}

fn link_one(source: &Path, target: &Path, label: &str) -> Result<()> {
    match target.symlink_metadata() {
        Err(_) => {
            // path doesn't exist
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(source, target)
                .with_context(|| format!("symlinking {}", target.display()))?;
            println!("{}: linked → {}", label, source.display());
        }
        Ok(m) if m.file_type().is_symlink() => {
            let dst = std::fs::read_link(target)?;
            if dst == source {
                println!("{}: already linked — skipping", label);
            } else {
                println!("{}: conflict — symlink points to {}", label, dst.display());
                match prompt_conflict()? {
                    ConflictChoice::Overwrite => {
                        std::fs::remove_file(target)?;
                        std::os::unix::fs::symlink(source, target)?;
                        println!("{}: relinked → {}", label, source.display());
                    }
                    ConflictChoice::Skip => println!("{}: skipped", label),
                    ConflictChoice::Migrate => bail!(
                        "migrate not applicable for existing symlinks"
                    ),
                }
            }
        }
        Ok(_) => {
            // real directory exists
            println!("{}: conflict — real directory at {}", label, target.display());
            match prompt_conflict()? {
                ConflictChoice::Overwrite => {
                    std::fs::remove_dir_all(target)?;
                    std::os::unix::fs::symlink(source, target)?;
                    println!("{}: overwritten → {}", label, source.display());
                }
                ConflictChoice::Migrate => {
                    migrate_then_link(source, target)?;
                    println!("{}: migrated + linked → {}", label, source.display());
                }
                ConflictChoice::Skip => println!("{}: skipped", label),
            }
        }
    }
    Ok(())
}

fn migrate_then_link(source: &Path, target: &Path) -> Result<()> {
    let existing_names: std::collections::HashSet<_> = std::fs::read_dir(source)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();

    for entry in std::fs::read_dir(target)? {
        let entry = entry?;
        let name = entry.file_name();
        if existing_names.contains(&name) {
            println!("  skip (duplicate): {}", name.to_string_lossy());
            continue;
        }
        let dest = source.join(&name);
        std::fs::rename(entry.path(), &dest)
            .with_context(|| format!("moving {} to source", name.to_string_lossy()))?;
        println!("  migrated: {}", name.to_string_lossy());
    }
    std::fs::remove_dir_all(target)?;
    std::os::unix::fs::symlink(source, target)?;
    Ok(())
}

enum ConflictChoice {
    Overwrite,
    Migrate,
    Skip,
}

fn prompt_conflict() -> Result<ConflictChoice> {
    loop {
        print!("  [o]verwrite  [m]igrate then link  [s]kip: ");
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        match buf.trim() {
            "o" | "O" => return Ok(ConflictChoice::Overwrite),
            "m" | "M" => return Ok(ConflictChoice::Migrate),
            "s" | "S" => return Ok(ConflictChoice::Skip),
            _ => println!("  enter o, m, or s"),
        }
    }
}

pub fn cmd_unlink(cfg: &Config, type_name: Option<&str>) -> Result<()> {
    let source = cfg.source.as_deref();
    let targets: Vec<_> = match type_name {
        Some(name) => cfg.targets.iter()
            .filter(|t| t.r#type == name)
            .collect(),
        None => cfg.targets.iter().collect(),
    };
    if targets.is_empty() {
        bail!("no matching target(s)");
    }
    for t in targets {
        unlink_one(&t.path, &t.r#type, source)?;
    }
    Ok(())
}

fn unlink_one(target: &Path, label: &str, source: Option<&Path>) -> Result<()> {
    let status = link_status(target, source);
    if status == "not linked" {
        println!("{}: not linked — skipping", label);
        return Ok(());
    }
    match target.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::remove_file(target)
                .with_context(|| format!("removing symlink {}", target.display()))?;
            println!("{}: unlinked", label);
        }
        _ => bail!("{}: {} — not a symlink, refusing to remove", label, target.display()),
    }
    Ok(())
}

pub fn cmd_status(cfg: &Config) -> Result<()> {
    let source_display = cfg.source.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not set)".to_owned());
    println!("source: {}", source_display);
    println!();
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
