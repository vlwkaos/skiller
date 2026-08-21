use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::catalog::{
    CatalogAvailability, CatalogIndex, CatalogStatus, load_global_config,
    sync_registered_catalogs_cached, sync_registered_catalogs_resilient,
};
use crate::installer::InstallScope;
use crate::model::{
    EffectiveMode, InstalledState, ProjectConfig, SelectionMode, SkillSelection,
    validate_installed_state, validate_schema,
};
use crate::paths::{
    global_config_path, global_state_path, read_json_or_default, write_global_config,
    write_json_atomic,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigRow {
    pub(crate) key: String,
    pub(crate) catalog: String,
    pub(crate) scope: String,
    pub(crate) scope_order: i32,
    pub(crate) name: String,
    pub(crate) installed_name: String,
    pub(crate) description: String,
    pub(crate) global: bool,
    pub(crate) selected: Option<SelectionMode>,
    pub(crate) gitignore: bool,
    pub(crate) installed: bool,
    pub(crate) installed_mode: Option<EffectiveMode>,
    pub(crate) required_by: Vec<String>,
    pub(crate) read_only: bool,
    pub(crate) status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrintedConfig<'a> {
    scope: &'a str,
    config_path: String,
    agents: &'a [String],
    skills: &'a [ConfigRow],
    catalog_status: Vec<PrintedCatalogStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrintedCatalogStatus {
    alias: String,
    availability: &'static str,
    stale: bool,
    declared_count: usize,
    installed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

pub fn configure(
    scope: InstallScope,
    print_only: bool,
    assignments: &[String],
    gitignore_assignments: &[String],
    agents: &[String],
) -> Result<()> {
    let mut global_config = load_global_config()?;
    if global_config.catalogs.is_empty() {
        anyhow::bail!("no catalogs configured; run `skiller add-catalog <alias> <source>`");
    }
    let sync = if print_only {
        sync_registered_catalogs_cached(&global_config)?
    } else {
        sync_registered_catalogs_resilient(&global_config)?
    };
    let catalogs = sync.catalogs.clone();
    let (config_path, state_path, mut manifest) = match &scope {
        InstallScope::Project(project_root) => {
            let config_path = project_root.join("skiller.config.json");
            let manifest: ProjectConfig = read_json_or_default(&config_path)?;
            (
                config_path,
                project_root.join(".skiller/installed.json"),
                manifest,
            )
        }
        InstallScope::Global => (
            global_config_path()?,
            global_state_path()?,
            ProjectConfig {
                version: global_config.version,
                skills: global_config.skills.clone(),
                agents: global_config.agents.clone(),
            },
        ),
    };
    validate_schema(manifest.version, "skill config")?;
    let state: InstalledState = read_json_or_default(&state_path)?;
    validate_installed_state(state.version)?;
    let stale_aliases: BTreeSet<_> = sync
        .statuses
        .iter()
        .filter(|(_, status)| status.availability == CatalogAvailability::Stale)
        .map(|(alias, _)| alias.clone())
        .collect();
    let mut display_catalogs = catalogs.clone();
    for (alias, status) in &sync.statuses {
        if let Some(catalog) = &status.catalog
            && status.availability == CatalogAvailability::Stale
        {
            display_catalogs.insert(alias.clone(), catalog.clone());
        }
    }
    let rows = config_rows(
        &display_catalogs,
        &state,
        &manifest,
        scope.is_global(),
        &stale_aliases,
    );

    if print_only {
        let output = PrintedConfig {
            scope: if scope.is_global() {
                "global"
            } else {
                "project"
            },
            config_path: config_path.display().to_string(),
            agents: &manifest.agents,
            skills: &rows,
            catalog_status: printed_catalog_statuses(&sync.statuses, &manifest, &state),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if !assignments.is_empty() || !gitignore_assignments.is_empty() || !agents.is_empty() {
        apply_assignments(&mut manifest, &rows, assignments)?;
        apply_gitignore_assignments(
            &mut manifest,
            &rows,
            gitignore_assignments,
            scope.is_global(),
        )?;
        if !agents.is_empty() {
            crate::installer::validate_agents(agents)?;
            manifest.agents = agents.to_vec();
        }
        save_manifest(&scope, &config_path, &manifest, &mut global_config)?;
        println!("saved {}", config_path.display());
        return Ok(());
    }

    match crate::config_tui::run(&rows, &mut manifest, scope.is_global())? {
        crate::config_tui::ConfigTuiResult::Cancel => Ok(()),
        crate::config_tui::ConfigTuiResult::Save => {
            save_manifest(&scope, &config_path, &manifest, &mut global_config)?;
            println!("saved {}", config_path.display());
            maybe_install(scope, &manifest, &catalogs, &sync.unavailable_aliases())
        }
    }
}

fn save_manifest(
    scope: &InstallScope,
    path: &Path,
    manifest: &ProjectConfig,
    global_config: &mut crate::model::GlobalConfig,
) -> Result<()> {
    match scope {
        InstallScope::Project(_) => write_json_atomic(path, manifest),
        InstallScope::Global => {
            global_config.skills = manifest.skills.clone();
            global_config.agents = manifest.agents.clone();
            write_global_config(global_config)
        }
    }
}

fn installed_name_is_current(installed: &crate::model::InstalledSkill, desired_name: &str) -> bool {
    installed.installed_name == desired_name
}

fn config_rows(
    catalogs: &BTreeMap<String, CatalogIndex>,
    state: &InstalledState,
    manifest: &ProjectConfig,
    global_scope: bool,
    stale_aliases: &BTreeSet<String>,
) -> Vec<ConfigRow> {
    let mut rows: Vec<_> = catalogs
        .values()
        .flat_map(|catalog| {
            catalog
                .skills
                .values()
                .filter(move |skill| skill.global == global_scope)
                .map(|skill| {
                    let key = format!("{}/{}", catalog.alias, skill.name);
                    let scope = skill.scope.clone().unwrap_or_else(|| "other".to_owned());
                    let scope_order = catalog
                        .metadata
                        .scopes
                        .get(&scope)
                        .map_or(i32::MAX, |metadata| metadata.order);
                    let installed_name = skill.installed_name.clone();
                    let selection = manifest.skills.get(&key);
                    let mut required_by: Vec<_> = catalog
                        .skills
                        .values()
                        .filter(|candidate| candidate.requires.contains(&skill.name))
                        .map(|candidate| candidate.name.clone())
                        .collect();
                    required_by.sort();
                    let installed = state
                        .skills
                        .get(&key)
                        .filter(|installed| installed_name_is_current(installed, &installed_name));
                    ConfigRow {
                        key: key.clone(),
                        catalog: catalog.alias.clone(),
                        scope,
                        scope_order,
                        name: skill.name.clone(),
                        installed: installed.is_some(),
                        installed_mode: installed.map(|skill| skill.mode),
                        installed_name,
                        description: skill.description.clone(),
                        global: skill.global,
                        selected: selection.map(SkillSelection::mode),
                        gitignore: selection.is_some_and(SkillSelection::gitignore),
                        required_by,
                        read_only: stale_aliases.contains(&catalog.alias),
                        status: stale_aliases
                            .contains(&catalog.alias)
                            .then_some("stale".to_owned()),
                    }
                })
        })
        .collect();
    rows.sort_by(|left, right| {
        left.catalog
            .cmp(&right.catalog)
            .then(left.scope_order.cmp(&right.scope_order))
            .then(left.scope.cmp(&right.scope))
            .then(left.name.cmp(&right.name))
    });
    rows
}

fn printed_catalog_statuses(
    statuses: &BTreeMap<String, CatalogStatus>,
    manifest: &ProjectConfig,
    state: &InstalledState,
) -> Vec<PrintedCatalogStatus> {
    statuses
        .values()
        .map(|status| PrintedCatalogStatus {
            alias: status.alias.clone(),
            availability: match status.availability {
                CatalogAvailability::Available => "available",
                CatalogAvailability::Stale => "stale",
                CatalogAvailability::Unavailable => "unavailable",
            },
            stale: status.availability == CatalogAvailability::Stale,
            declared_count: manifest
                .skills
                .keys()
                .filter(|key| key.starts_with(&format!("{}/", status.alias)))
                .count(),
            installed_count: state
                .skills
                .keys()
                .filter(|key| key.starts_with(&format!("{}/", status.alias)))
                .count(),
            warning: status.warning.clone(),
        })
        .collect()
}

fn apply_assignments(
    manifest: &mut ProjectConfig,
    rows: &[ConfigRow],
    assignments: &[String],
) -> Result<()> {
    let available: BTreeSet<_> = rows
        .iter()
        .filter(|row| !row.read_only)
        .map(|row| row.key.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for assignment in assignments {
        let (key, mode) = assignment.split_once('=').with_context(|| {
            format!("invalid selection {assignment:?}; expected catalog/name=enable|manual|off")
        })?;
        if !available.contains(key) {
            anyhow::bail!("skill is unavailable in this configuration: {key}");
        }
        if !seen.insert(key) {
            anyhow::bail!("duplicate skill selection: {key}");
        }
        let gitignore = manifest
            .skills
            .get(key)
            .is_some_and(SkillSelection::gitignore);
        match mode {
            "enable" => {
                manifest.skills.insert(
                    key.to_owned(),
                    SkillSelection::from_parts(SelectionMode::Enable, gitignore),
                );
            }
            "manual" => {
                manifest.skills.insert(
                    key.to_owned(),
                    SkillSelection::from_parts(SelectionMode::Manual, gitignore),
                );
            }
            "off" => {
                manifest.skills.remove(key);
            }
            _ => anyhow::bail!("invalid mode for {key}: {mode}; expected enable, manual, or off"),
        }
    }
    Ok(())
}

fn apply_gitignore_assignments(
    manifest: &mut ProjectConfig,
    rows: &[ConfigRow],
    assignments: &[String],
    global_scope: bool,
) -> Result<()> {
    if global_scope && !assignments.is_empty() {
        anyhow::bail!("global skill configuration does not support Git ignore state");
    }
    let available: BTreeSet<_> = rows
        .iter()
        .filter(|row| !row.read_only)
        .map(|row| row.key.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for assignment in assignments {
        let (key, value) = assignment.split_once('=').with_context(|| {
            format!("invalid Git ignore state {assignment:?}; expected catalog/name=true|false")
        })?;
        if !available.contains(key) {
            anyhow::bail!("skill is unavailable in this configuration: {key}");
        }
        if !seen.insert(key) {
            anyhow::bail!("duplicate Git ignore state: {key}");
        }
        let gitignore = match value {
            "true" => true,
            "false" => false,
            _ => {
                anyhow::bail!("invalid Git ignore state for {key}: {value}; expected true or false")
            }
        };
        let selection = manifest
            .skills
            .get(key)
            .with_context(|| format!("select {key} before changing its Git ignore state"))?;
        manifest.skills.insert(
            key.to_owned(),
            SkillSelection::from_parts(selection.mode(), gitignore),
        );
    }
    Ok(())
}

pub(crate) fn cycle_selection(manifest: &mut ProjectConfig, key: &str) {
    let next = match manifest.skills.get(key).map(SkillSelection::mode) {
        None => Some(SelectionMode::Enable),
        Some(SelectionMode::Enable) => Some(SelectionMode::Manual),
        Some(SelectionMode::Manual) => None,
    };
    let ignored = manifest
        .skills
        .get(key)
        .is_some_and(SkillSelection::gitignore);
    if let Some(mode) = next {
        manifest
            .skills
            .insert(key.to_owned(), SkillSelection::from_parts(mode, ignored));
    } else {
        manifest.skills.remove(key);
    }
}

pub(crate) fn toggle_gitignore(manifest: &mut ProjectConfig, key: &str) {
    let Some(selection) = manifest.skills.get(key) else {
        println!("select the skill before changing its Git ignore state");
        return;
    };
    let mode = selection.mode();
    let ignored = !selection.gitignore();
    manifest
        .skills
        .insert(key.to_owned(), SkillSelection::from_parts(mode, ignored));
}

fn maybe_install(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    unavailable_aliases: &BTreeSet<String>,
) -> Result<()> {
    let command = if scope.is_global() {
        "skiller install -g"
    } else {
        "skiller install"
    };
    let answer = prompt(&format!("Run `{command}` now? [y/N]: "))?;
    if answer == "y" || answer == "yes" {
        crate::installer::install_with_catalogs_preserving(
            scope,
            manifest,
            catalogs,
            unavailable_aliases,
        )?;
    }
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("reading interactive input")?;
    Ok(input.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogSkill;
    use crate::model::{CatalogMetadata, InstalledSkill, SCHEMA_VERSION};
    use std::path::PathBuf;

    #[test]
    fn selection_cycles_without_persisting_off() {
        let mut manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
            agents: crate::model::default_agents(),
        };
        cycle_selection(&mut manifest, "pyg/develop");
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Enable);
        cycle_selection(&mut manifest, "pyg/develop");
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Manual);
        cycle_selection(&mut manifest, "pyg/develop");
        assert!(!manifest.skills.contains_key("pyg/develop"));
    }

    #[test]
    fn assignments_apply_modes_and_reject_unknown_or_duplicate_skills() {
        let rows = vec![ConfigRow {
            key: "pyg/develop".to_owned(),
            catalog: "pyg".to_owned(),
            scope: "engineering".to_owned(),
            scope_order: 0,
            name: "develop".to_owned(),
            installed_name: "develop".to_owned(),
            description: "Develop".to_owned(),
            global: true,
            selected: None,
            gitignore: false,
            installed: false,
            installed_mode: None,
            required_by: Vec::new(),
            read_only: false,
            status: None,
        }];
        let mut manifest = ProjectConfig::default();
        apply_assignments(&mut manifest, &rows, &["pyg/develop=enable".to_owned()]).unwrap();
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Enable);
        apply_gitignore_assignments(
            &mut manifest,
            &rows,
            &["pyg/develop=true".to_owned()],
            false,
        )
        .unwrap();
        apply_assignments(&mut manifest, &rows, &["pyg/develop=manual".to_owned()]).unwrap();
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Manual);
        assert!(manifest.skills["pyg/develop"].gitignore());
        apply_assignments(&mut manifest, &rows, &["pyg/develop=off".to_owned()]).unwrap();
        assert!(!manifest.skills.contains_key("pyg/develop"));
        assert!(
            apply_assignments(&mut manifest, &rows, &["pyg/missing=enable".to_owned()]).is_err()
        );
        assert!(
            apply_assignments(
                &mut manifest,
                &rows,
                &[
                    "pyg/develop=enable".to_owned(),
                    "pyg/develop=manual".to_owned()
                ]
            )
            .is_err()
        );
        assert!(
            apply_gitignore_assignments(
                &mut manifest,
                &rows,
                &["pyg/develop=true".to_owned()],
                true,
            )
            .is_err()
        );
        assert!(
            apply_gitignore_assignments(
                &mut ProjectConfig::default(),
                &rows,
                &["pyg/develop=true".to_owned()],
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn stale_rows_are_read_only_and_status_counts_include_unavailable_catalogs() {
        let status = CatalogStatus {
            alias: "offline".to_owned(),
            availability: CatalogAvailability::Unavailable,
            warning: Some("network unavailable".to_owned()),
            catalog: None,
        };
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "offline/root".to_owned(),
                SkillSelection::Mode(SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let state = InstalledState {
            version: crate::model::INSTALLED_STATE_VERSION,
            skills: BTreeMap::from([(
                "offline/root".to_owned(),
                InstalledSkill {
                    installed_name: "root".to_owned(),
                    mode: EffectiveMode::Enable,
                    gitignore: false,
                    digest: None,
                    legacy_path: None,
                },
            )]),
        };
        let report = printed_catalog_statuses(
            &BTreeMap::from([("offline".to_owned(), status)]),
            &manifest,
            &state,
        );
        assert_eq!(report[0].availability, "unavailable");
        assert_eq!(
            (report[0].declared_count, report[0].installed_count),
            (1, 1)
        );
    }

    #[test]
    fn stale_postfixed_state_does_not_match_a_clean_name_row() {
        let installed = InstalledSkill {
            installed_name: "develop-engineering".to_owned(),
            mode: EffectiveMode::Enable,
            gitignore: false,
            digest: None,
            legacy_path: None,
        };
        assert!(!installed_name_is_current(&installed, "develop"));
        assert!(installed_name_is_current(&installed, "develop-engineering"));
    }

    #[test]
    fn rows_partition_global_and_project_skills() {
        let catalog = CatalogIndex {
            alias: "pyg".to_owned(),
            source: "test".to_owned(),
            root: PathBuf::from("."),
            revision: None,
            metadata: CatalogMetadata::default(),
            skills: BTreeMap::from([
                (
                    "global".to_owned(),
                    CatalogSkill {
                        name: "global".to_owned(),
                        description: "Global".to_owned(),
                        digest: "global".to_owned(),
                        scope: None,
                        installed_name: "global-scope".to_owned(),
                        global: true,
                        requires: Vec::new(),
                    },
                ),
                (
                    "project".to_owned(),
                    CatalogSkill {
                        name: "project".to_owned(),
                        description: "Project".to_owned(),
                        digest: "project".to_owned(),
                        scope: None,
                        installed_name: "project-scope".to_owned(),
                        global: false,
                        requires: Vec::new(),
                    },
                ),
            ]),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog)]);
        let manifest = ProjectConfig::default();
        let state = InstalledState::default();
        let global = config_rows(
            &catalogs,
            &state,
            &manifest,
            true,
            &BTreeSet::from(["pyg".to_owned()]),
        );
        let project = config_rows(&catalogs, &state, &manifest, false, &BTreeSet::new());
        assert_eq!(global[0].name, "global");
        assert_eq!(global[0].installed_name, "global-scope");
        assert!(global[0].read_only);
        assert_eq!(global[0].status.as_deref(), Some("stale"));
        assert_eq!(project[0].name, "project");
        assert_eq!(project[0].installed_name, "project-scope");
    }
}
