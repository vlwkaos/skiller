mod config;
mod link;
mod targets;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

use config::Config;
use link::{cmd_link, cmd_status, cmd_unlink};
use targets::{cmd_add, cmd_list, cmd_remove};

#[derive(Parser)]
#[command(
    name = "skiller",
    about = "Central skill management for AI coding tools",
    after_help = "Examples:\n  skiller source ~/skills\n  skiller target add claude\n  skiller target add codex ~/.codex/skills\n  skiller link\n  skiller status",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
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
    #[command(
        about = "Add a target",
        long_about = "Add a target.\n\nBuilt-in types auto-resolve their default path when <TYPE> is one of:\n  claude\n  codex\n  opencode\n  openclaw\n  hermes\n\nPass <PATH> to override the built-in path or add a custom type."
    )]
    Add {
        #[arg(help = "Target type. Built-in types: claude, codex, opencode, openclaw, hermes")]
        r#type: String,
        #[arg(help = "Optional explicit path. Omit it to use the built-in path for known types")]
        path: Option<String>,
    },
    /// Remove a target from config (does not unlink)
    Remove { r#type: String },
    /// List all targets with link status
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    let mut cfg = Config::load()?;

    match cli.command.expect("checked above") {
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
