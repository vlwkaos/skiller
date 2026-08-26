use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::catalog::{
    CatalogAvailability, CatalogStatus, load_global_config,
    sync_registered_authoring_catalogs_resilient, sync_registered_catalogs_resilient,
};
use crate::installer::{InstallScope, install_paths, resolve_manifest};
use crate::model::{InstalledState, ProjectConfig};
use crate::paths::{read_json, read_json_or_default};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillUpdate {
    key: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_digest: Option<String>,
    available_digest: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateReport {
    scope: &'static str,
    updates: Vec<SkillUpdate>,
    drafts: Vec<SkillUpdate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<UpdateCatalogStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCatalogStatus {
    alias: String,
    availability: &'static str,
    stale: bool,
    declared_count: usize,
    installed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_availability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_warning: Option<String>,
}

pub fn run(scope: InstallScope, machine: bool, yes: bool) -> Result<()> {
    let global = load_global_config()?;
    let sync = sync_registered_catalogs_resilient(&global)?;
    for status in sync
        .statuses
        .values()
        .filter(|status| status.warning.is_some())
    {
        eprintln!(
            "warning: catalog {} is unavailable: {}",
            status.alias,
            status.warning.as_deref().unwrap_or_default()
        );
    }
    let unavailable_aliases = sync.unavailable_aliases();
    let catalogs = sync.catalogs;
    let manifest = match &scope {
        InstallScope::Project(root) => read_json(&root.join("skiller.config.json"))
            .context("run `skiller config` before checking updates")?,
        InstallScope::Global => ProjectConfig {
            version: global.version,
            skills: global.skills.clone(),
            agents: global.agents.clone(),
        },
    };
    let mut active_manifest = manifest.clone();
    active_manifest.skills.retain(|key, _| {
        key.split_once('/')
            .is_none_or(|(alias, _)| !unavailable_aliases.contains(alias))
    });
    let resolved = resolve_manifest(&active_manifest, &catalogs, scope.is_global())?;
    let paths = install_paths(&scope)?;
    let installed: InstalledState = read_json_or_default(&paths.state_path)?;
    let mut updates = Vec::new();
    for skill in &resolved {
        if let Some(update) = skill_update(skill, installed.skills.get(&skill.key)) {
            updates.push(update);
        }
    }
    let (authoring_catalogs, draft_warnings) =
        sync_registered_authoring_catalogs_resilient(&global, &catalogs);
    let authoring = resolve_manifest(&active_manifest, &authoring_catalogs, scope.is_global())?;
    let canonical_by_key: BTreeMap<_, _> = resolved
        .iter()
        .map(|skill| (skill.key.as_str(), skill.digest.as_str()))
        .collect();
    let drafts = authoring
        .iter()
        .filter(|skill| {
            global
                .catalogs
                .get(skill.key.split_once('/').expect("resolved key has alias").0)
                .and_then(|registration| registration.authoring_root.as_ref())
                .is_some()
                && canonical_by_key.get(skill.key.as_str()).copied() != Some(skill.digest.as_str())
        })
        .filter_map(|skill| skill_update(skill, installed.skills.get(&skill.key)))
        .map(|mut update| {
            update.status = "local-draft";
            update
        })
        .collect();
    let warnings = update_catalog_statuses(
        &sync.statuses,
        &global,
        &manifest,
        &installed,
        &draft_warnings,
    );
    let report = UpdateReport {
        scope: if scope.is_global() {
            "global"
        } else {
            "project"
        },
        updates,
        drafts,
        warnings,
    };
    if machine {
        println!("{}", serde_json::to_string(&report)?);
    } else if report.updates.is_empty() && report.drafts.is_empty() {
        println!("{} skills are current", report.scope);
    } else {
        if !report.updates.is_empty() {
            println!(
                "{} published skill update{} available:",
                report.updates.len(),
                if report.updates.len() == 1 { "" } else { "s" }
            );
            for update in &report.updates {
                println!("- {} ({})", update.key, update.status);
            }
        }
        if !report.drafts.is_empty() {
            println!(
                "{} unpublished authoring change{} detected:",
                report.drafts.len(),
                if report.drafts.len() == 1 { "" } else { "s" }
            );
            for update in &report.drafts {
                println!("- {}", update.key);
            }
        }
    }
    if (machine && !yes) || report.updates.is_empty() {
        return Ok(());
    }
    if !yes && !confirm(report.updates.len())? {
        println!("update cancelled");
        return Ok(());
    }
    drop(resolved);
    crate::installer::install_with_catalogs_preserving(
        scope,
        &manifest,
        &catalogs,
        &unavailable_aliases,
    )
}

fn update_catalog_statuses(
    statuses: &BTreeMap<String, CatalogStatus>,
    global: &crate::model::GlobalConfig,
    manifest: &ProjectConfig,
    installed: &InstalledState,
    draft_warnings: &BTreeMap<String, String>,
) -> Vec<UpdateCatalogStatus> {
    statuses
        .values()
        .filter(|status| {
            status.availability != CatalogAvailability::Available
                || draft_warnings.contains_key(&status.alias)
        })
        .map(|status| UpdateCatalogStatus {
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
            installed_count: installed
                .skills
                .keys()
                .filter(|key| key.starts_with(&format!("{}/", status.alias)))
                .count(),
            warning: status.warning.clone(),
            draft_availability: global
                .catalogs
                .get(&status.alias)
                .and_then(|registration| registration.authoring_root.as_ref())
                .map(|_| {
                    if draft_warnings.contains_key(&status.alias) {
                        "unavailable"
                    } else {
                        "available"
                    }
                }),
            draft_warning: draft_warnings.get(&status.alias).cloned(),
        })
        .collect()
}

fn skill_update(
    skill: &crate::installer::ResolvedSkill<'_>,
    current: Option<&crate::model::InstalledSkill>,
) -> Option<SkillUpdate> {
    let status = match current {
        None => Some("not-installed"),
        Some(current)
            if current.installed_name != skill.installed_name
                || current.mode != skill.mode
                || current.gitignore != skill.gitignore =>
        {
            Some("reconcile")
        }
        Some(current) if current.digest.as_deref() != Some(&skill.digest) => {
            Some(if current.digest.is_some() {
                "updated"
            } else {
                "version-untracked"
            })
        }
        Some(_) => None,
    }?;
    Some(SkillUpdate {
        key: skill.key.clone(),
        status,
        installed_digest: current.and_then(|value| value.digest.clone()),
        available_digest: skill.digest.clone(),
    })
}

fn confirm(count: usize) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("noninteractive update requires --yes");
    }
    print!(
        "Install and verify {count} update{}? [y/N] ",
        if count == 1 { "" } else { "s" }
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogAvailability, CatalogStatus};
    use crate::model::{EffectiveMode, InstalledSkill};

    #[test]
    fn unavailable_catalog_is_reported_without_an_update() {
        let statuses = BTreeMap::from([(
            "offline".to_owned(),
            CatalogStatus {
                alias: "offline".to_owned(),
                availability: CatalogAvailability::Unavailable,
                warning: Some("network unavailable".to_owned()),
                catalog: None,
            },
        )]);
        let manifest = ProjectConfig {
            version: crate::model::SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "offline/root".to_owned(),
                crate::model::SkillSelection::Mode(crate::model::SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let installed = InstalledState {
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
        let report = update_catalog_statuses(
            &statuses,
            &crate::model::GlobalConfig::default(),
            &manifest,
            &installed,
            &BTreeMap::new(),
        );
        assert_eq!(report[0].availability, "unavailable");
        assert_eq!(
            (report[0].declared_count, report[0].installed_count),
            (1, 1)
        );
    }
}
