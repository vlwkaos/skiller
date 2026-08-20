mod catalog;
mod config_tui;
mod config_ui;
mod installer;
mod manual;
mod model;
mod paths;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Inspect or interactively edit catalog skill selections
    Config {
        /// Configure the global selection instead of the current project
        #[arg(short = 'g', long)]
        global: bool,
        /// Print machine-readable catalog, selection, and installed state
        #[arg(long, conflicts_with = "set")]
        print: bool,
        /// Set one or more selections as catalog/name=enable|manual|off
        #[arg(long, value_name = "SKILL=MODE")]
        set: Vec<String>,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::AddCatalog { alias, source } => catalog::add_catalog(&alias, &source),
        Command::Config { global, print, set } => {
            let scope = if global {
                installer::InstallScope::Global
            } else {
                installer::InstallScope::Project(paths::project_root()?)
            };
            config_ui::configure(scope, print, &set)
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
