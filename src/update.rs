use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::catalog::{
    load_global_config, sync_registered_authoring_catalogs, sync_registered_catalogs_noninteractive,
};
use crate::installer::{InstallScope, install_paths, install_with_catalogs, resolve_manifest};
use crate::model::{InstalledState, ProjectConfig};
use crate::paths::{read_json, read_json_or_default};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogVersion {
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

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
    catalogs: BTreeMap<String, CatalogVersion>,
    updates: Vec<SkillUpdate>,
    local_drafts: Vec<SkillUpdate>,
}

pub fn run(scope: InstallScope, check: bool, json: bool, yes: bool) -> Result<()> {
    let global = load_global_config()?;
    let catalogs = sync_registered_catalogs_noninteractive(&global)?;
    let manifest = match &scope {
        InstallScope::Project(root) => read_json(&root.join("skiller.config.json"))
            .context("run `skiller config` before checking updates")?,
        InstallScope::Global => ProjectConfig {
            version: global.version,
            skills: global.skills.clone(),
            agents: global.agents.clone(),
        },
    };
    let resolved = resolve_manifest(&manifest, &catalogs, scope.is_global())?;
    let paths = install_paths(&scope)?;
    let installed: InstalledState = read_json_or_default(&paths.state_path)?;
    let mut used_catalogs = BTreeSet::new();
    let mut updates = Vec::new();
    for skill in &resolved {
        used_catalogs.insert(skill.key.split_once('/').expect("resolved key has alias").0);
        if let Some(update) = skill_update(skill, installed.skills.get(&skill.key)) {
            updates.push(update);
        }
    }
    let authoring_catalogs = sync_registered_authoring_catalogs(&global, &catalogs)?;
    let authoring = resolve_manifest(&manifest, &authoring_catalogs, scope.is_global())?;
    let canonical_by_key: BTreeMap<_, _> = resolved
        .iter()
        .map(|skill| (skill.key.as_str(), skill.digest.as_str()))
        .collect();
    let local_drafts = authoring
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
    let catalog_versions = used_catalogs
        .into_iter()
        .filter_map(|alias| {
            let catalog = catalogs.get(alias)?;
            let registration = global.catalogs.get(alias)?;
            Some((
                alias.to_owned(),
                CatalogVersion {
                    source: registration.source.clone(),
                    r#ref: registration.r#ref.clone(),
                    revision: catalog.revision.clone(),
                },
            ))
        })
        .collect();
    let report = UpdateReport {
        scope: if scope.is_global() {
            "global"
        } else {
            "project"
        },
        catalogs: catalog_versions,
        updates,
        local_drafts,
    };
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else if report.updates.is_empty() && report.local_drafts.is_empty() {
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
        if !report.local_drafts.is_empty() {
            println!(
                "{} unpublished authoring change{} detected:",
                report.local_drafts.len(),
                if report.local_drafts.len() == 1 {
                    ""
                } else {
                    "s"
                }
            );
            for update in &report.local_drafts {
                println!("- {}", update.key);
            }
        }
    }
    if check || report.updates.is_empty() {
        return Ok(());
    }
    if !yes && !confirm(report.updates.len())? {
        println!("update cancelled");
        return Ok(());
    }
    drop(resolved);
    install_with_catalogs(scope, &manifest, &catalogs)
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
