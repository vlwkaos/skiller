use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::catalog::{CatalogIndex, load_global_config, sync_registered_catalogs};
use crate::installer::{InstallScope, install_with_catalogs};
use crate::model::{
    EffectiveMode, InstalledState, ProjectConfig, SelectionMode, SkillSelection,
    validate_installed_state, validate_schema,
};
use crate::paths::{
    global_config_path, global_skills_root, global_state_path, read_json_or_default,
    write_global_config, write_json_atomic,
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrintedConfig<'a> {
    scope: &'a str,
    config_path: String,
    skills: &'a [ConfigRow],
}

pub fn configure(
    scope: InstallScope,
    print_only: bool,
    assignments: &[String],
    gitignore_assignments: &[String],
) -> Result<()> {
    let mut global_config = load_global_config()?;
    if global_config.catalogs.is_empty() {
        anyhow::bail!("no catalogs configured; run `skiller add-catalog <alias> <source>`");
    }
    let catalogs = sync_registered_catalogs(&global_config)?;
    let (config_path, state_path, target_root, mut manifest) = match &scope {
        InstallScope::Project(project_root) => {
            let config_path = project_root.join("skiller.config.json");
            let manifest: ProjectConfig = read_json_or_default(&config_path)?;
            (
                config_path,
                project_root.join(".skiller/installed.json"),
                project_root.join(".agents/skills"),
                manifest,
            )
        }
        InstallScope::Global => (
            global_config_path()?,
            global_state_path()?,
            global_skills_root()?,
            ProjectConfig {
                version: global_config.version,
                skills: global_config.skills.clone(),
            },
        ),
    };
    validate_schema(manifest.version, "skill config")?;
    let state: InstalledState = read_json_or_default(&state_path)?;
    validate_installed_state(state.version)?;
    let rows = config_rows(
        &catalogs,
        &state,
        &target_root,
        &manifest,
        scope.is_global(),
    );

    if print_only {
        let output = PrintedConfig {
            scope: if scope.is_global() {
                "global"
            } else {
                "project"
            },
            config_path: config_path.display().to_string(),
            skills: &rows,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if !assignments.is_empty() || !gitignore_assignments.is_empty() {
        apply_assignments(&mut manifest, &rows, assignments)?;
        apply_gitignore_assignments(
            &mut manifest,
            &rows,
            gitignore_assignments,
            scope.is_global(),
        )?;
        save_manifest(&scope, &config_path, &manifest, &mut global_config)?;
        println!("saved {}", config_path.display());
        return Ok(());
    }

    match crate::config_tui::run(&rows, &mut manifest, scope.is_global())? {
        crate::config_tui::ConfigTuiResult::Cancel => Ok(()),
        crate::config_tui::ConfigTuiResult::Save => {
            save_manifest(&scope, &config_path, &manifest, &mut global_config)?;
            println!("saved {}", config_path.display());
            maybe_install(scope, &manifest, &catalogs)
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
    target_root: &Path,
    manifest: &ProjectConfig,
    global_scope: bool,
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
                    let installed = state.skills.get(&key).filter(|installed| {
                        installed_name_is_current(installed, &installed_name)
                            && target_root
                                .join(&installed.installed_name)
                                .join("SKILL.md")
                                .is_file()
                    });
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

fn apply_assignments(
    manifest: &mut ProjectConfig,
    rows: &[ConfigRow],
    assignments: &[String],
) -> Result<()> {
    let available: BTreeSet<_> = rows.iter().map(|row| row.key.as_str()).collect();
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
    let available: BTreeSet<_> = rows.iter().map(|row| row.key.as_str()).collect();
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
) -> Result<()> {
    let command = if scope.is_global() {
        "skiller install -g"
    } else {
        "skiller install"
    };
    let answer = prompt(&format!("Run `{command}` now? [y/N]: "))?;
    if answer == "y" || answer == "yes" {
        install_with_catalogs(scope, manifest, catalogs, false)?;
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
    fn stale_unpostfixed_state_does_not_match_a_postfixed_row() {
        let installed = InstalledSkill {
            installed_name: "develop".to_owned(),
            mode: EffectiveMode::Enable,
            gitignore: false,
            legacy_path: None,
        };
        assert!(!installed_name_is_current(
            &installed,
            "develop-engineering"
        ));
        assert!(installed_name_is_current(&installed, "develop"));
    }

    #[test]
    fn rows_partition_global_and_project_skills() {
        let catalog = CatalogIndex {
            alias: "pyg".to_owned(),
            source: "test".to_owned(),
            root: PathBuf::from("."),
            metadata: CatalogMetadata::default(),
            skills: BTreeMap::from([
                (
                    "global".to_owned(),
                    CatalogSkill {
                        name: "global".to_owned(),
                        description: "Global".to_owned(),
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
        let global = config_rows(&catalogs, &state, Path::new("."), &manifest, true);
        let project = config_rows(&catalogs, &state, Path::new("."), &manifest, false);
        assert_eq!(global[0].name, "global");
        assert_eq!(global[0].installed_name, "global-scope");
        assert_eq!(project[0].name, "project");
        assert_eq!(project[0].installed_name, "project-scope");
    }
}
