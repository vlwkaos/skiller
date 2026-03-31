use anyhow::{Context, Result, bail};
use std::io::{self, Write};
use std::path::Path;

use crate::config::{Config, LinkMode, Target};
use crate::targets::{link_status, skill_entries};

pub fn cmd_link(cfg: &Config, type_name: Option<&str>) -> Result<()> {
    let source = cfg.source_dir()?;
    let targets: Vec<_> = match type_name {
        Some(name) => cfg.targets.iter().filter(|t| t.r#type == name).collect(),
        None => cfg.targets.iter().collect(),
    };
    if targets.is_empty() {
        bail!("no matching target(s)");
    }
    for t in targets {
        link_one(source, t)?;
    }
    Ok(())
}

fn link_one(source: &Path, target: &Target) -> Result<()> {
    match target.mode {
        LinkMode::Folder => link_folder(source, &target.path, &target.r#type),
        LinkMode::Granular => link_granular(source, &target.path, &target.r#type),
    }
}

fn link_folder(source: &Path, target: &Path, label: &str) -> Result<()> {
    println!(
        "warning: {} [folder] mode is not reversible — unlink only removes the symlink and will not restore any prior target contents",
        label
    );
    match target.symlink_metadata() {
        Err(_) => {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(source, target)
                .with_context(|| format!("symlinking {}", target.display()))?;
            println!("{} [folder]: linked → {}", label, source.display());
        }
        Ok(m) if m.file_type().is_symlink() => {
            let dst = std::fs::read_link(target)?;
            if dst == source {
                println!("{} [folder]: already linked — skipping", label);
            } else {
                println!(
                    "{} [folder]: conflict — symlink points to {}",
                    label,
                    dst.display()
                );
                match prompt_folder_conflict()? {
                    ConflictChoice::Overwrite => {
                        std::fs::remove_file(target)?;
                        std::os::unix::fs::symlink(source, target)?;
                        println!("{} [folder]: relinked → {}", label, source.display());
                    }
                    ConflictChoice::Skip => println!("{} [folder]: skipped", label),
                    ConflictChoice::Migrate => {
                        bail!("migrate not applicable for existing symlinks")
                    }
                }
            }
        }
        Ok(_) => {
            println!(
                "{} [folder]: conflict — real directory at {}",
                label,
                target.display()
            );
            match prompt_folder_conflict()? {
                ConflictChoice::Overwrite => {
                    std::fs::remove_dir_all(target)?;
                    std::os::unix::fs::symlink(source, target)?;
                    println!("{} [folder]: overwritten → {}", label, source.display());
                }
                ConflictChoice::Migrate => {
                    migrate_then_link(source, target)?;
                    println!(
                        "{} [folder]: migrated + linked → {}",
                        label,
                        source.display()
                    );
                }
                ConflictChoice::Skip => println!("{} [folder]: skipped", label),
            }
        }
    }
    Ok(())
}

fn link_granular(source: &Path, target: &Path, label: &str) -> Result<()> {
    if !ensure_granular_target_dir(target, label)? {
        return Ok(());
    }

    let source_entries = skill_entries(source)?;
    if source_entries.is_empty() {
        println!(
            "{} [granular]: no source skills found — nothing to link",
            label
        );
        return Ok(());
    }

    let mut linked = 0usize;
    let mut skipped = 0usize;

    for src in source_entries {
        let name = match src.file_name() {
            Some(name) => name,
            None => continue,
        };
        let dest = target.join(name);
        match link_granular_entry(&src, &dest, label)? {
            EntryAction::Linked | EntryAction::AlreadyLinked => linked += 1,
            EntryAction::Skipped => skipped += 1,
        }
    }

    println!(
        "{} [granular]: done — {} linked/already-linked, {} skipped",
        label, linked, skipped
    );
    Ok(())
}

fn ensure_granular_target_dir(target: &Path, label: &str) -> Result<bool> {
    match target.symlink_metadata() {
        Err(_) => {
            std::fs::create_dir_all(target)?;
            Ok(true)
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            let dst = std::fs::read_link(target)?;
            println!(
                "{} [granular]: conflict — target root is a symlink to {}",
                label,
                dst.display()
            );
            match prompt_entry_conflict(true)? {
                EntryConflictChoice::Overwrite => {
                    std::fs::remove_file(target)?;
                    std::fs::create_dir_all(target)?;
                    println!(
                        "{} [granular]: replaced root symlink with real directory",
                        label
                    );
                    Ok(true)
                }
                EntryConflictChoice::Skip => {
                    println!("{} [granular]: skipped", label);
                    Ok(false)
                }
            }
        }
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => bail!(
            "{} [granular]: target root is not a directory: {}",
            label,
            target.display()
        ),
    }
}

enum EntryAction {
    Linked,
    AlreadyLinked,
    Skipped,
}

fn link_granular_entry(
    source_entry: &Path,
    target_entry: &Path,
    label: &str,
) -> Result<EntryAction> {
    let name = target_entry
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target_entry.display().to_string());

    match target_entry.symlink_metadata() {
        Err(_) => {
            std::os::unix::fs::symlink(source_entry, target_entry)
                .with_context(|| format!("symlinking {}", target_entry.display()))?;
            println!("{} [granular]: linked {}", label, name);
            Ok(EntryAction::Linked)
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            let dst = std::fs::read_link(target_entry)?;
            if dst == source_entry {
                println!("{} [granular]: already linked {}", label, name);
                Ok(EntryAction::AlreadyLinked)
            } else {
                println!(
                    "{} [granular]: conflict — {} points to {}",
                    label,
                    name,
                    dst.display()
                );
                match prompt_entry_conflict(false)? {
                    EntryConflictChoice::Overwrite => {
                        std::fs::remove_file(target_entry)?;
                        std::os::unix::fs::symlink(source_entry, target_entry)?;
                        println!("{} [granular]: relinked {}", label, name);
                        Ok(EntryAction::Linked)
                    }
                    EntryConflictChoice::Skip => {
                        println!("{} [granular]: skipped {}", label, name);
                        Ok(EntryAction::Skipped)
                    }
                }
            }
        }
        Ok(_) => {
            println!(
                "{} [granular]: conflict — real entry exists at {}",
                label,
                target_entry.display()
            );
            match prompt_entry_conflict(false)? {
                EntryConflictChoice::Overwrite => {
                    if target_entry.is_dir() {
                        std::fs::remove_dir_all(target_entry)?;
                    } else {
                        std::fs::remove_file(target_entry)?;
                    }
                    std::os::unix::fs::symlink(source_entry, target_entry)?;
                    println!("{} [granular]: overwritten {}", label, name);
                    Ok(EntryAction::Linked)
                }
                EntryConflictChoice::Skip => {
                    println!("{} [granular]: skipped {}", label, name);
                    Ok(EntryAction::Skipped)
                }
            }
        }
    }
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

fn prompt_folder_conflict() -> Result<ConflictChoice> {
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

enum EntryConflictChoice {
    Overwrite,
    Skip,
}

fn prompt_entry_conflict(root: bool) -> Result<EntryConflictChoice> {
    loop {
        if root {
            print!("  [o]verwrite target root with real dir  [s]kip: ");
        } else {
            print!("  [o]verwrite  [s]kip: ");
        }
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        match buf.trim() {
            "o" | "O" => return Ok(EntryConflictChoice::Overwrite),
            "s" | "S" => return Ok(EntryConflictChoice::Skip),
            _ => println!("  enter o or s"),
        }
    }
}

pub fn cmd_unlink(cfg: &Config, type_name: Option<&str>) -> Result<()> {
    let source = cfg.source.as_deref();
    let targets: Vec<_> = match type_name {
        Some(name) => cfg.targets.iter().filter(|t| t.r#type == name).collect(),
        None => cfg.targets.iter().collect(),
    };
    if targets.is_empty() {
        bail!("no matching target(s)");
    }
    for t in targets {
        unlink_one(t, source)?;
    }
    Ok(())
}

fn unlink_one(target: &Target, source: Option<&Path>) -> Result<()> {
    match target.mode {
        LinkMode::Folder => unlink_folder(&target.path, &target.r#type, source),
        LinkMode::Granular => unlink_granular(&target.path, &target.r#type, source),
    }
}

fn unlink_folder(target: &Path, label: &str, source: Option<&Path>) -> Result<()> {
    println!(
        "warning: {} [folder] unlink only removes the symlink and will not restore any prior target contents",
        label
    );
    let status = link_status(
        &Target {
            r#type: label.to_owned(),
            path: target.to_path_buf(),
            mode: LinkMode::Folder,
        },
        source,
    );
    if status == "not linked" {
        println!("{} [folder]: not linked — skipping", label);
        return Ok(());
    }
    match target.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::remove_file(target)
                .with_context(|| format!("removing symlink {}", target.display()))?;
            println!("{} [folder]: unlinked", label);
        }
        _ => bail!(
            "{} [folder]: {} — not a symlink, refusing to remove",
            label,
            target.display()
        ),
    }
    Ok(())
}

fn unlink_granular(target: &Path, label: &str, source: Option<&Path>) -> Result<()> {
    let Some(source) = source else {
        bail!("{} [granular]: source not configured", label);
    };

    match target.symlink_metadata() {
        Err(_) => {
            println!(
                "{} [granular]: target root missing — nothing to unlink",
                label
            );
            return Ok(());
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "{} [granular]: target root is a symlink, refusing to remove entire directory",
                label
            );
        }
        Ok(meta) if !meta.is_dir() => {
            bail!("{} [granular]: target root is not a directory", label);
        }
        Ok(_) => {}
    }

    let mut removed = 0usize;
    let mut skipped = 0usize;

    for src in skill_entries(source)? {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = target.join(name);
        match dest.symlink_metadata() {
            Err(_) => skipped += 1,
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&dest) {
                Ok(dst) if dst == src => {
                    std::fs::remove_file(&dest)?;
                    println!("{} [granular]: unlinked {}", label, name.to_string_lossy());
                    removed += 1;
                }
                _ => skipped += 1,
            },
            Ok(_) => skipped += 1,
        }
    }

    println!(
        "{} [granular]: done — {} unlinked, {} untouched",
        label, removed, skipped
    );
    Ok(())
}

pub fn cmd_status(cfg: &Config) -> Result<()> {
    let source_display = cfg
        .source
        .as_ref()
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
