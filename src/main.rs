mod catalog;
mod config_tui;
mod config_ui;
mod doctor;
mod installer;
mod manual;
mod model;
mod paths;

use anyhow::Result;
use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "skiller",
    version,
    about = "Declaratively manage project and global skills from registered catalogs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register a skill catalog in the global Skiller configuration
    AddCatalog {
        /// Stable lowercase alias used by project manifests
        alias: String,
        /// GitHub owner/repo, Git URL, or local catalog path
        source: String,
    },
    /// Mutate an explicitly selected authoring catalog checkout
    Catalog {
        #[command(subcommand)]
        command: CatalogCommand,
    },
    /// Inspect or interactively edit catalog skill selections
    Config {
        /// Configure the global selection instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Print machine-readable catalog, selection, and installed state
        #[arg(long, conflicts_with_all = ["set", "set_gitignore"])]
        print: bool,
        /// Set one or more selections as catalog/name=enable|manual|off
        #[arg(long, value_name = "SKILL=MODE")]
        set: Vec<String>,
        /// Set project Git-ignore state as catalog/name=true|false
        #[arg(long, value_name = "SKILL=BOOL")]
        set_gitignore: Vec<String>,
    },
    /// Diagnose and explicitly repair catalog-managed configuration and installations
    Doctor {
        /// Diagnose global state instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Print a compact machine-readable report
        #[arg(long, conflicts_with = "repair")]
        print: bool,
        /// Repair only deterministic Skiller-owned state
        #[arg(long)]
        repair: bool,
        /// Confirm repair without prompting
        #[arg(long, requires = "repair")]
        yes: bool,
    },
    /// Reconcile catalog-managed skills through Vercel Skills
    Install {
        /// Install globally instead of into the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Adopt and replace same-name legacy installations after staging succeeds
        #[arg(long)]
        migrate: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// Copy one skill into a catalog and register its scope and eligibility
    AddSkill(AddSkillArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("eligibility")
        .required(true)
        .multiple(false)
        .args(["global", "project"])
))]
struct AddSkillArgs {
    /// Explicit writable catalog checkout
    #[arg(long)]
    root: PathBuf,
    /// Skill directory containing SKILL.md
    #[arg(long)]
    source: PathBuf,
    /// Existing semantic scope in skiller.json
    #[arg(long)]
    scope: String,
    /// Permit global selection and installation
    #[arg(long)]
    global: bool,
    /// Permit project selection and installation
    #[arg(long)]
    project: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::AddCatalog { alias, source } => catalog::add_catalog(&alias, &source),
        Command::Catalog {
            command: CatalogCommand::AddSkill(args),
        } => catalog::add_skill(
            &args.root,
            &args.source,
            &args.scope,
            if args.global {
                catalog::CatalogEligibility::Global
            } else {
                debug_assert!(args.project);
                catalog::CatalogEligibility::Project
            },
        ),
        Command::Config {
            global,
            print,
            set,
            set_gitignore,
        } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            config_ui::configure(scope, print, &set, &set_gitignore)
        }
        Command::Doctor {
            global,
            print,
            repair,
            yes,
        } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            doctor::run(scope, print, repair, yes)
        }
        Command::Install { global, migrate } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            installer::install(scope, migrate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_requires_repair_for_yes_and_separates_print() {
        assert!(Cli::try_parse_from(["skiller", "doctor", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["skiller", "doctor", "--repair", "--print"]).is_err());
        assert!(Cli::try_parse_from(["skiller", "doctor", "-g", "--repair", "--yes"]).is_ok());
    }

    #[test]
    fn catalog_add_skill_requires_one_eligibility_flag() {
        let base = [
            "skiller",
            "catalog",
            "add-skill",
            "--root",
            "catalog",
            "--source",
            "candidate",
            "--scope",
            "learning",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--global", "--project"])).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--global"])).is_ok());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--project"])).is_ok());
    }
}
