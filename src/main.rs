mod catalog;
mod config_tui;
mod config_ui;
mod doctor;
mod installer;
mod manual;
mod migration;
mod model;
mod paths;
mod update;

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
        #[arg(long, conflicts_with_all = ["set", "set_gitignore", "agent"])]
        print: bool,
        /// Set one or more selections as catalog/name=enable|manual|off
        #[arg(long, value_name = "SKILL=MODE")]
        set: Vec<String>,
        /// Set project Git-ignore state as catalog/name=true|false
        #[arg(long, value_name = "SKILL=BOOL")]
        set_gitignore: Vec<String>,
        /// Replace Vercel installation targets; repeat for multiple agents
        #[arg(long, value_name = "AGENT")]
        agent: Vec<String>,
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
    /// Guide legacy skills into a catalog, configuration, and verified installation
    Migrate {
        /// Create an editable migration-plan template
        #[arg(long, value_name = "PATH", conflicts_with_all = ["plan", "check", "apply", "yes"])]
        init: Option<PathBuf>,
        /// Read a deterministic migration plan
        #[arg(long, value_name = "PATH", conflicts_with = "init")]
        plan: Option<PathBuf>,
        /// Validate and print the plan without mutation
        #[arg(long, requires = "plan", conflicts_with = "apply")]
        check: bool,
        /// Apply the validated plan
        #[arg(long, requires = "plan", conflicts_with = "check")]
        apply: bool,
        /// Confirm noninteractive application
        #[arg(long, requires = "apply")]
        yes: bool,
    },
    /// Check for or explicitly install catalog skill updates
    Update {
        /// Check global skills instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Refresh catalogs and report updates without installing
        #[arg(long, conflicts_with = "yes")]
        check: bool,
        /// Print a compact machine-readable update report
        #[arg(long, requires = "check")]
        json: bool,
        /// Confirm update installation without prompting
        #[arg(long, conflicts_with = "check")]
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
    /// Copy one skill into a catalog and register its scope and eligibility
    AddSkill(AddSkillArgs),
    /// Configure canonical and writable sources for a registered catalog
    Configure(CatalogConfigureArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("catalog_change")
        .required(true)
        .multiple(true)
        .args(["source", "ref", "clear_ref", "authoring_root", "clear_authoring_root"])
))]
struct CatalogConfigureArgs {
    /// Registered catalog alias
    alias: String,
    /// Canonical Git or local source used by consumers
    #[arg(long)]
    source: Option<String>,
    /// Branch, tag, or commit selected from the canonical source
    #[arg(long, value_name = "REF", conflicts_with = "clear_ref")]
    r#ref: Option<String>,
    /// Remove the configured canonical ref
    #[arg(long)]
    clear_ref: bool,
    /// Explicit writable checkout used only for catalog authoring
    #[arg(long, conflicts_with = "clear_authoring_root")]
    authoring_root: Option<PathBuf>,
    /// Remove the configured writable authoring checkout
    #[arg(long)]
    clear_authoring_root: bool,
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
        Command::Catalog { command } => match command {
            CatalogCommand::AddSkill(args) => catalog::add_skill(
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
            CatalogCommand::Configure(args) => catalog::configure_catalog(
                &args.alias,
                args.source.as_deref(),
                args.r#ref.as_deref(),
                args.clear_ref,
                args.authoring_root.as_deref(),
                args.clear_authoring_root,
            ),
        },
        Command::Config {
            global,
            print,
            set,
            set_gitignore,
            agent,
        } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            config_ui::configure(scope, print, &set, &set_gitignore, &agent)
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
        Command::Migrate {
            init,
            plan,
            check: _,
            apply,
            yes,
        } => match (init, plan) {
            (Some(path), None) => migration::initialize(&path),
            (None, Some(path)) => migration::run_plan(&path, apply, yes),
            (None, None) => migration::interactive(),
            (Some(_), Some(_)) => unreachable!("Clap rejects conflicting migration inputs"),
        },
        Command::Update {
            global,
            check,
            json,
            yes,
        } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            update::run(scope, check, json, yes)
        }
        Command::Install { global } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            installer::install(scope)
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
    fn migration_cli_separates_check_apply_and_noninteractive_confirmation() {
        assert!(Cli::try_parse_from(["skiller", "migrate", "--yes"]).is_err());
        assert!(
            Cli::try_parse_from([
                "skiller",
                "migrate",
                "--plan",
                "plan.json",
                "--check",
                "--apply"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "skiller",
                "migrate",
                "--plan",
                "plan.json",
                "--apply",
                "--yes"
            ])
            .is_ok()
        );
    }

    #[test]
    fn update_check_is_read_only_and_json_is_check_only() {
        assert!(Cli::try_parse_from(["skiller", "update", "--check", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["skiller", "update", "--check", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["skiller", "update", "--json"]).is_err());
    }

    #[test]
    fn catalog_configure_unifies_registration_and_owner_checkout() {
        assert!(
            Cli::try_parse_from([
                "skiller",
                "catalog",
                "configure",
                "private",
                "--source",
                "git@example/catalog.git",
                "--ref",
                "main",
                "--authoring-root",
                "/catalog"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["skiller", "catalog", "configure", "private"]).is_err());
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
