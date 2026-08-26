use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::catalog::{
    CatalogAvailability, CatalogIndex, CatalogStatus, load_global_config,
    sync_registered_catalogs_cached,
};
use crate::installer::{
    InstallScope, ProjectionStatus, ResolvedSkill, projection_status, resolve_manifest,
};
use crate::model::{
    EffectiveMode, InstalledState, ProjectConfig, SelectionMode, SkillSelection,
    validate_installed_state, validate_schema,
};
use crate::paths::{
    expand_home_path, global_config_path, global_state_path, read_json_or_default,
    write_global_config, write_json_atomic,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigRow {
    pub(crate) key: String,
    #[serde(skip)]
    pub(crate) catalog: String,
    pub(crate) scope: String,
    pub(crate) scope_order: i32,
    #[serde(skip)]
    pub(crate) name: String,
    pub(crate) installed_name: String,
    pub(crate) description: String,
    #[serde(rename = "want", skip_serializing_if = "Option::is_none")]
    pub(crate) selected: Option<SelectionMode>,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) gitignore: bool,
    #[serde(skip)]
    pub(crate) installed: bool,
    #[serde(rename = "have", skip_serializing_if = "Option::is_none")]
    pub(crate) installed_mode: Option<EffectiveMode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) required_by: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sync: Option<ProjectionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) authoring: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrintedConfig<'a> {
    scope: &'a str,
    #[serde(rename = "config")]
    config_path: String,
    agents: &'a [String],
    summary: ConfigSummary,
    skills: Vec<PrintedSkill<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    catalog_status: Vec<PrintedCatalogStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrintedSkill<'a> {
    key: &'a str,
    scope: &'a str,
    scope_order: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(rename = "want", skip_serializing_if = "Option::is_none")]
    selected: Option<SelectionMode>,
    #[serde(skip_serializing_if = "is_false")]
    gitignore: bool,
    #[serde(rename = "have", skip_serializing_if = "Option::is_none")]
    installed_mode: Option<EffectiveMode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_by: &'a Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "sync_is_default")]
    sync: Option<ProjectionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authoring: Option<&'a str>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn sync_is_default(value: &Option<ProjectionStatus>) -> bool {
    matches!(value, None | Some(ProjectionStatus::Synced))
}

fn printed_skill(row: &ConfigRow) -> PrintedSkill<'_> {
    let attention = !sync_is_default(&row.sync) || row.status.is_some();
    PrintedSkill {
        key: &row.key,
        scope: &row.scope,
        scope_order: row.scope_order,
        installed_name: (row.installed_name != row.name).then_some(row.installed_name.as_str()),
        description: (!row.installed || row.selected.is_none() || attention)
            .then_some(row.description.as_str()),
        selected: row.selected,
        gitignore: row.gitignore,
        installed_mode: row.installed_mode,
        required_by: &row.required_by,
        read_only: row.read_only,
        status: row.status.as_deref(),
        sync: row.sync,
        authoring: row.authoring.as_deref(),
    }
}

#[derive(Serialize)]
struct ConfigSummary {
    selected: usize,
    installed: usize,
    attention: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    authoring_path: Option<String>,
    authoring_is_canonical: bool,
}

pub fn configure(
    scope: InstallScope,
    machine: bool,
    assignments: &[String],
    agents: &[String],
) -> Result<()> {
    let mut global_config = load_global_config()?;
    if global_config.catalogs.is_empty() {
        anyhow::bail!("no catalogs configured; run `skiller catalog configure <alias> <source>`");
    }
    let read_only = machine && assignments.is_empty() && agents.is_empty();
    let sync = sync_registered_catalogs_cached(&global_config)?;
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
    let mut active_manifest = manifest.clone();
    active_manifest.skills.retain(|key, _| {
        key.split_once('/')
            .is_some_and(|(alias, _)| catalogs.contains_key(alias))
    });
    let desired = resolve_manifest(&active_manifest, &catalogs, scope.is_global())?;
    let desired_by_key: BTreeMap<_, _> = desired
        .iter()
        .map(|skill| (skill.key.as_str(), skill))
        .collect();
    let project_root = match &scope {
        InstallScope::Project(root) => Some(root.as_path()),
        InstallScope::Global => None,
    };
    let mut rows = config_rows(
        &display_catalogs,
        &state,
        &manifest,
        scope.is_global(),
        project_root,
        &desired_by_key,
        &stale_aliases,
    )?;
    for row in &mut rows {
        if matches!(
            row.sync,
            Some(
                ProjectionStatus::KeepLocal
                    | ProjectionStatus::Conflict
                    | ProjectionStatus::OrphanedLocal
            )
        ) {
            row.authoring = global_config
                .catalogs
                .get(&row.catalog)
                .and_then(authoring_root_path)
                .map(|root| root.join("skills").join(&row.name).display().to_string());
        }
    }

    if read_only {
        let output = PrintedConfig {
            scope: if scope.is_global() {
                "global"
            } else {
                "project"
            },
            config_path: config_path.display().to_string(),
            agents: &manifest.agents,
            summary: ConfigSummary {
                selected: manifest.skills.len(),
                installed: rows.iter().filter(|row| row.installed).count(),
                attention: rows
                    .iter()
                    .filter(|row| {
                        !matches!(row.sync, None | Some(ProjectionStatus::Synced))
                            || row.status.is_some()
                    })
                    .count(),
            },
            skills: rows.iter().map(printed_skill).collect(),
            catalog_status: printed_catalog_statuses(
                &sync.statuses,
                &manifest,
                &state,
                &global_config.catalogs,
            ),
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    if !assignments.is_empty() || !agents.is_empty() {
        apply_assignments(&mut manifest, &rows, assignments, scope.is_global())?;
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
    project_root: Option<&Path>,
    desired: &BTreeMap<&str, &ResolvedSkill<'_>>,
    stale_aliases: &BTreeSet<String>,
) -> Result<Vec<ConfigRow>> {
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
                        selected: selection.map(SkillSelection::mode),
                        gitignore: selection.is_some_and(SkillSelection::gitignore),
                        required_by,
                        read_only: stale_aliases.contains(&catalog.alias),
                        status: stale_aliases
                            .contains(&catalog.alias)
                            .then_some("stale".to_owned()),
                        sync: None,
                        authoring: None,
                    }
                })
        })
        .collect();
    if let Some(project_root) = project_root {
        for (key, installed) in &state.skills {
            if rows.iter().any(|row| &row.key == key) {
                continue;
            }
            let actual = project_root
                .join(".agents/skills")
                .join(&installed.installed_name);
            if !matches!(
                projection_status(false, installed, None, &actual)?,
                ProjectionStatus::KeepLocal
                    | ProjectionStatus::Conflict
                    | ProjectionStatus::Unknown
            ) {
                continue;
            }
            let Some((catalog, name)) = key.split_once('/') else {
                continue;
            };
            let selection = manifest.skills.get(key);
            rows.push(ConfigRow {
                key: key.clone(),
                catalog: catalog.to_owned(),
                scope: "other".to_owned(),
                scope_order: i32::MAX,
                name: name.to_owned(),
                installed_name: installed.installed_name.clone(),
                description: "Project override whose catalog entry is unavailable.".to_owned(),
                selected: selection.map(SkillSelection::mode),
                gitignore: selection.is_some_and(SkillSelection::gitignore),
                installed: true,
                installed_mode: Some(installed.mode),
                required_by: Vec::new(),
                read_only: true,
                status: Some("orphaned".to_owned()),
                sync: Some(ProjectionStatus::OrphanedLocal),
                authoring: None,
            });
        }
    }
    let projection_root = project_root
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(crate::paths::global_skills_root)?;
    for row in &mut rows {
        let Some(installed) = state.skills.get(&row.key) else {
            continue;
        };
        row.sync = Some(projection_status(
            global_scope,
            installed,
            desired.get(row.key.as_str()).copied(),
            &projection_root.join(&installed.installed_name),
        )?);
    }
    rows.sort_by(|left, right| {
        left.catalog
            .cmp(&right.catalog)
            .then(left.scope_order.cmp(&right.scope_order))
            .then(left.scope.cmp(&right.scope))
            .then(left.name.cmp(&right.name))
    });
    Ok(rows)
}

fn authoring_root_path(registration: &crate::model::CatalogRegistration) -> Option<PathBuf> {
    let configured = registration
        .authoring_root
        .as_deref()
        .or((registration.r#ref.is_none()).then_some(registration.source.as_str()))?;
    expand_home_path(configured)
        .ok()
        .filter(|path| path.is_dir())
}

fn printed_catalog_statuses(
    statuses: &BTreeMap<String, CatalogStatus>,
    manifest: &ProjectConfig,
    state: &InstalledState,
    registrations: &BTreeMap<String, crate::model::CatalogRegistration>,
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
            authoring_path: registrations
                .get(&status.alias)
                .and_then(authoring_root_path)
                .map(|path| path.display().to_string()),
            authoring_is_canonical: registrations.get(&status.alias).is_some_and(|registration| {
                if registration.r#ref.is_some() {
                    return false;
                }
                let authoring = registration.authoring_root.as_deref().unwrap_or(&registration.source);
                let source = expand_home_path(&registration.source)
                    .and_then(|path| path.canonicalize().context("resolving source"));
                let authoring = expand_home_path(authoring)
                    .and_then(|path| path.canonicalize().context("resolving authoring root"));
                matches!((source, authoring), (Ok(source), Ok(authoring)) if source == authoring)
            }),
        })
        .collect()
}

pub(crate) fn row_editable(row: &ConfigRow) -> bool {
    !row.read_only
        && !matches!(
            row.sync,
            Some(
                ProjectionStatus::KeepLocal
                    | ProjectionStatus::Conflict
                    | ProjectionStatus::OrphanedLocal
                    | ProjectionStatus::Unknown
            )
        )
}

fn apply_assignments(
    manifest: &mut ProjectConfig,
    rows: &[ConfigRow],
    assignments: &[String],
    global_scope: bool,
) -> Result<()> {
    let available: BTreeSet<_> = rows
        .iter()
        .filter(|row| row_editable(row))
        .map(|row| row.key.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for assignment in assignments {
        let (key, state) = assignment.split_once('=').with_context(|| {
            format!("invalid selection {assignment:?}; expected catalog/name=STATE")
        })?;
        if !available.contains(key) {
            anyhow::bail!("skill is unavailable in this configuration: {key}");
        }
        if !seen.insert(key) {
            anyhow::bail!("duplicate skill selection: {key}");
        }
        let selection = match state {
            "enable" => Some((SelectionMode::Enable, false)),
            "manual" => Some((SelectionMode::Manual, false)),
            "enable-ignored" if !global_scope => Some((SelectionMode::Enable, true)),
            "manual-ignored" if !global_scope => Some((SelectionMode::Manual, true)),
            "off" => None,
            _ => anyhow::bail!(
                "invalid state for {key}: {state}; expected enable, manual, enable-ignored, manual-ignored, or off"
            ),
        };
        if let Some((mode, ignored)) = selection {
            manifest
                .skills
                .insert(key.to_owned(), SkillSelection::from_parts(mode, ignored));
        } else {
            manifest.skills.remove(key);
        }
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
            selected: None,
            gitignore: false,
            installed: false,
            installed_mode: None,
            required_by: Vec::new(),
            read_only: false,
            status: None,
            sync: None,
            authoring: None,
        }];
        let mut manifest = ProjectConfig::default();
        apply_assignments(
            &mut manifest,
            &rows,
            &["pyg/develop=enable-ignored".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Enable);
        assert!(manifest.skills["pyg/develop"].gitignore());
        apply_assignments(
            &mut manifest,
            &rows,
            &["pyg/develop=manual".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Manual);
        assert!(!manifest.skills["pyg/develop"].gitignore());
        apply_assignments(&mut manifest, &rows, &["pyg/develop=off".to_owned()], false).unwrap();
        assert!(!manifest.skills.contains_key("pyg/develop"));
        assert!(
            apply_assignments(
                &mut manifest,
                &rows,
                &["pyg/missing=enable".to_owned()],
                false,
            )
            .is_err()
        );
        assert!(
            apply_assignments(
                &mut manifest,
                &rows,
                &[
                    "pyg/develop=enable".to_owned(),
                    "pyg/develop=manual".to_owned(),
                ],
                false,
            )
            .is_err()
        );
        assert!(
            apply_assignments(
                &mut manifest,
                &rows,
                &["pyg/develop=enable-ignored".to_owned()],
                true,
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
                    content_digest: None,
                    legacy_path: None,
                },
            )]),
        };
        let report = printed_catalog_statuses(
            &BTreeMap::from([("offline".to_owned(), status)]),
            &manifest,
            &state,
            &BTreeMap::new(),
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
            content_digest: None,
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
            None,
            &BTreeMap::new(),
            &BTreeSet::from(["pyg".to_owned()]),
        )
        .unwrap();
        let project = config_rows(
            &catalogs,
            &state,
            &manifest,
            false,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(global[0].name, "global");
        assert_eq!(global[0].installed_name, "global-scope");
        assert!(global[0].read_only);
        assert_eq!(global[0].status.as_deref(), Some("stale"));
        assert_eq!(project[0].name, "project");
        assert_eq!(project[0].installed_name, "project-scope");
    }
}
