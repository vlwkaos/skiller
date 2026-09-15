mod catalog;
mod config_tui;
mod config_ui;
mod doctor;
mod installer;
mod manual;
mod model;
mod output;
mod paths;
mod project_store;
mod update;

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use installer::InstallScope;

#[derive(Debug, Parser)]
#[command(
    name = "skiller",
    version,
    about = "Declaratively manage project and global skills from registered catalogs"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Configure or author registered skill catalogs
    Catalog {
        #[command(subcommand)]
        command: CatalogCommand,
    },
    /// Inspect or edit catalog skill selections
    Config {
        /// Use global configuration instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Set catalog/name=enable|manual|enable-ignored|manual-ignored|off
        #[arg(long, value_name = "SKILL=STATE")]
        set: Vec<String>,
        /// Replace installation targets with a comma-separated list
        #[arg(long, value_name = "AGENT,...")]
        agents: Option<String>,
    },
    /// Diagnose or repair managed state
    Doctor {
        /// Diagnose global state instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Repair deterministic Skiller-owned state
        #[arg(long)]
        repair: bool,
        /// Confirm repair without prompting
        #[arg(long, requires = "repair")]
        yes: bool,
    },
    /// Check for updates, or install them with --yes
    Update {
        /// Check global skills instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Install available updates without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Reconcile catalog-managed skills through Vercel Skills
    Install {
        /// Install globally instead of into the current project
        #[arg(short = 'g', long)]
        global: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// Add a skill through a catalog's validated authoring checkout
    AddSkill(AddSkillArgs),
    /// Replace a catalog's canonical and optional authoring source
    Configure(CatalogConfigureArgs),
}

#[derive(Debug, Args)]
struct CatalogConfigureArgs {
    /// Registered catalog alias
    alias: String,
    /// GitHub owner/repo, Git URL, or local catalog path
    source: String,
    /// Branch, tag, or commit selected from the canonical source
    #[arg(long, value_name = "REF")]
    r#ref: Option<String>,
    /// Explicit writable checkout used for catalog authoring
    #[arg(long)]
    authoring_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AddSkillArgs {
    /// Registered catalog alias
    alias: String,
    /// Skill directory containing SKILL.md
    source: PathBuf,
    /// Existing semantic scope in skiller.json
    scope: String,
    /// Permit global selection; project-only is the safe default
    #[arg(long)]
    global: bool,
}

fn scope(global: bool) -> Result<installer::InstallScope> {
    Ok(if global {
        installer::InstallScope::Global
    } else {
        installer::InstallScope::Project(paths::project_root()?)
    })
}

fn parse_agents(value: Option<String>) -> Result<Vec<String>> {
    let agents: Vec<String> = value
        .map(|value| value.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    if !agents.is_empty() {
        installer::validate_agents(&agents)?;
    }
    Ok(agents)
}

fn main() -> Result<()> {
    let machine = !io::stdout().is_terminal();
    match Cli::parse().command {
        None if machine => anyhow::bail!(
            "interactive configuration requires a terminal; run `skiller config` or another command"
        ),
        None => match paths::project_root() {
            Err(_) => config_ui::configure(InstallScope::Global, false, &[], &[]),
            Ok(project_root) => match config_tui::choose_configuration_scope()? {
                Some(config_tui::ConfigurationScope::Project) => {
                    config_ui::configure(InstallScope::Project(project_root), false, &[], &[])
                }
                Some(config_tui::ConfigurationScope::Global) => {
                    config_ui::configure(InstallScope::Global, false, &[], &[])
                }
                None => Ok(()),
            },
        },
        Some(Command::Catalog { command }) => match command {
            CatalogCommand::AddSkill(args) => catalog::add_skill(
                &args.alias,
                &args.source,
                &args.scope,
                if args.global {
                    catalog::CatalogEligibility::Global
                } else {
                    catalog::CatalogEligibility::Project
                },
            ),
            CatalogCommand::Configure(args) => catalog::configure_catalog(
                &args.alias,
                &args.source,
                args.r#ref.as_deref(),
                args.authoring_root.as_deref(),
            ),
        },
        Some(Command::Config {
            global,
            set,
            agents,
        }) => config_ui::configure(scope(global)?, machine, &set, &parse_agents(agents)?),
        Some(Command::Doctor {
            global,
            repair,
            yes,
        }) => doctor::run(scope(global)?, machine, repair, yes),
        Some(Command::Update { global, yes }) => update::run(scope(global)?, machine, yes),
        Some(Command::Install { global }) => installer::install(scope(global)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_cli_selects_interactive_configuration() {
        assert!(Cli::try_parse_from(["skiller"]).unwrap().command.is_none());
    }

    #[test]
    fn lean_cli_rejects_removed_compatibility_surface() {
        for args in [
            vec!["skiller", "add-catalog", "pyg", "source"],
            vec!["skiller", "migrate"],
            vec!["skiller", "config", "--print"],
            vec!["skiller", "doctor", "--print"],
            vec!["skiller", "update", "--check"],
            vec!["skiller", "update", "--json"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn doctor_requires_repair_for_yes() {
        assert!(Cli::try_parse_from(["skiller", "doctor", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["skiller", "doctor", "-g", "--repair", "--yes"]).is_ok());
    }

    #[test]
    fn catalog_commands_use_alias_owned_authoring() {
        assert!(
            Cli::try_parse_from([
                "skiller",
                "catalog",
                "configure",
                "private",
                "git@example/catalog.git",
                "--ref",
                "main",
                "--authoring-root",
                "/catalog",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "skiller",
                "catalog",
                "add-skill",
                "private",
                "/candidate",
                "learning",
            ])
            .is_ok()
        );
    }

    #[test]
    fn config_has_one_complete_selection_state() {
        assert!(
            Cli::try_parse_from([
                "skiller",
                "config",
                "--set",
                "pyg/release=enable-ignored",
                "--agents",
                "universal,pi",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["skiller", "config", "--set-gitignore", "x=true"]).is_err());
    }
}
