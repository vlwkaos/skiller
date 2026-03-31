mod config;
mod link;
mod targets;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

use config::{Config, LinkMode};
use link::{cmd_link, cmd_status, cmd_unlink};
use targets::{cmd_add, cmd_list, cmd_remove, skill_entries};

#[derive(Parser)]
#[command(
    name = "skiller",
    about = "Central skill management for AI coding tools",
    after_help = "Examples:\n  skiller source ~/.hermes/skills\n  skiller target add claude granular\n  skiller link claude\n  skiller status",
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
    /// Create symlink(s) from source to target(s)
    Link {
        /// Only link this target type
        r#type: Option<String>,
    },
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
        long_about = "Add a target.\n\nBuilt-in types auto-resolve their default path when <TYPE> is one of:\n  claude\n  codex\n  opencode\n  openclaw\n  hermes\n\nMode can be:\n  folder    symlink the whole target skills directory to the source\n  granular  keep the target root real and symlink each source skill into it\n\nExamples:\n  skiller target add claude\n  skiller target add claude granular\n  skiller target add mytool granular ~/.config/mytool/skills"
    )]
    Add {
        #[arg(help = "Target type. Built-in types: claude, codex, opencode, openclaw, hermes")]
        r#type: String,
        #[arg(help = "Optional mode ('folder' or 'granular') or explicit path if omitted mode")]
        mode_or_path: Option<String>,
        #[arg(help = "Optional explicit path when mode is provided")]
        path: Option<String>,
    },
    /// Remove a target from config (does not unlink)
    Remove { r#type: String },
    /// List all targets with link status
    List,
}

fn parse_target_add(
    mode_or_path: Option<String>,
    path: Option<String>,
) -> Result<(LinkMode, Option<String>)> {
    match (mode_or_path, path) {
        (None, None) => Ok((LinkMode::Folder, None)),
        (Some(value), None) => match value.as_str() {
            "folder" => Ok((LinkMode::Folder, None)),
            "granular" => Ok((LinkMode::Granular, None)),
            _ => Ok((LinkMode::Folder, Some(value))),
        },
        (Some(mode), Some(path)) => Ok((
            mode.parse::<LinkMode>().map_err(anyhow::Error::msg)?,
            Some(path),
        )),
        (None, Some(_)) => Err(anyhow::anyhow!("path provided without mode_or_path")),
    }
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
            anyhow::ensure!(
                expanded.exists(),
                "path does not exist: {}",
                expanded.display()
            );
            let expanded = expanded.canonicalize()?;
            let has_skill = !skill_entries(&expanded)?.is_empty();
            anyhow::ensure!(
                has_skill,
                "no skill directories or category trees containing SKILL.md found in {}",
                expanded.display()
            );
            cfg.source = Some(expanded.clone());
            cfg.save()?;
            println!("source set: {}", expanded.display());
        }
        Cmd::Target { sub } => match sub {
            TargetCmd::Add {
                r#type,
                mode_or_path,
                path,
            } => {
                let (mode, path) = parse_target_add(mode_or_path, path)?;
                cmd_add(&mut cfg, &r#type, path.as_deref(), mode)?;
            }
            TargetCmd::Remove { r#type } => {
                cmd_remove(&mut cfg, &r#type)?;
            }
            TargetCmd::List => {
                cmd_list(&cfg)?;
            }
        },
        Cmd::Link { r#type } => cmd_link(&cfg, r#type.as_deref())?,
        Cmd::Unlink { r#type } => cmd_unlink(&cfg, r#type.as_deref())?,
        Cmd::Status => cmd_status(&cfg)?,
    }

    Ok(())
}
