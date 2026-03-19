mod config;
mod link;
mod targets;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use config::Config;
use link::{cmd_link, cmd_status, cmd_unlink};
use targets::{cmd_add, cmd_list, cmd_remove};

#[derive(Parser)]
#[command(name = "skiller", about = "Central skill management for AI coding tools")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set source skills directory
    Source { path: PathBuf },
    /// Manage targets
    Target {
        #[command(subcommand)]
        sub: TargetCmd,
    },
    /// Create symlinks from source to all targets
    Link,
    /// Remove symlink(s)
    Unlink {
        /// Only unlink this target type
        r#type: Option<String>,
    },
    /// Show source and target states
    Status,
}

#[derive(Subcommand)]
enum TargetCmd {
    /// Add a target (built-in types auto-resolve path)
    Add {
        r#type: String,
        path: Option<String>,
    },
    /// Remove a target from config (does not unlink)
    Remove { r#type: String },
    /// List all targets with link status
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load()?;

    match cli.command {
        Cmd::Source { path } => {
            let expanded = PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref());
            anyhow::ensure!(expanded.exists(), "path does not exist: {}", expanded.display());
            let expanded = expanded.canonicalize()?;
            // Validate contains at least one */SKILL.md
            let has_skill = std::fs::read_dir(&expanded)?
                .filter_map(|e| e.ok())
                .any(|e| e.path().join("SKILL.md").exists());
            anyhow::ensure!(has_skill, "no skill directories (*/SKILL.md) found in {}", expanded.display());
            cfg.source = Some(expanded.clone());
            cfg.save()?;
            println!("source set: {}", expanded.display());
        }
        Cmd::Target { sub } => match sub {
            TargetCmd::Add { r#type, path } => {
                cmd_add(&mut cfg, &r#type, path.as_deref())?;
            }
            TargetCmd::Remove { r#type } => {
                cmd_remove(&mut cfg, &r#type)?;
            }
            TargetCmd::List => {
                cmd_list(&cfg)?;
            }
        },
        Cmd::Link => cmd_link(&cfg)?,
        Cmd::Unlink { r#type } => cmd_unlink(&cfg, r#type.as_deref())?,
        Cmd::Status => cmd_status(&cfg)?,
    }

    Ok(())
}
