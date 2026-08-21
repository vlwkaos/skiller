use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::catalog::{CatalogIndex, load_global_config, resolve_rename, sync_registered_catalogs};
use crate::installer::{
    InstallScope, TransactionJournal, TransactionPhase, install_paths,
    install_with_catalogs_recovery, list_agent_skill_names, manifest_fingerprint, projection_roots,
    resolve_manifest, validate_agents, validate_owned_state,
};
use crate::model::{
    INSTALLED_STATE_VERSION, InstalledState, ProjectConfig, validate_installed_state,
    validate_schema,
};
use crate::paths::{
    global_config_path, read_json, read_json_or_default, safe_remove_owned_dir,
    validate_managed_json_path, write_global_config, write_json_atomic,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DoctorIssue {
    code: &'static str,
    message: String,
    fixable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport<'a> {
    scope: &'a str,
    healthy: bool,
    issues: &'a [DoctorIssue],
}

struct Inputs {
    config_path: PathBuf,
    global_config: crate::model::GlobalConfig,
    manifest: ProjectConfig,
    catalogs: BTreeMap<String, CatalogIndex>,
}

// ^ README.md#doctor-and-recovery defines the read-only and explicit-repair boundary.
pub fn run(scope: InstallScope, print: bool, repair: bool, yes: bool) -> Result<()> {
    let scope_name = if scope.is_global() {
        "global"
    } else {
        "project"
    };
    let inputs = match load_inputs(&scope) {
        Ok(inputs) => inputs,
        Err(error) => {
            let issues = vec![DoctorIssue {
                code: "configuration",
                message: format!("{error:#}"),
                fixable: false,
            }];
            print_report(scope_name, &issues, print)?;
            if repair {
                bail!("Doctor cannot repair invalid configuration or catalog data");
            }
            return Ok(());
        }
    };
    validate_agents(&inputs.manifest.agents)?;
    let paths = install_paths(&scope)?;
    let mut issues = Vec::new();
    let migrated =
        match migrate_declared_renames(&inputs.manifest, &inputs.catalogs, scope.is_global()) {
            Ok((migrated, rename_messages)) => {
                issues.extend(rename_messages.into_iter().map(|message| DoctorIssue {
                    code: "declared-rename",
                    message,
                    fixable: true,
                }));
                migrated
            }
            Err(error) => {
                issues.push(DoctorIssue {
                    code: "configuration-key",
                    message: error.to_string(),
                    fixable: false,
                });
                inputs.manifest.clone()
            }
        };

    let state_path_safe = match validate_managed_json_path(&paths.state_path) {
        Ok(()) => true,
        Err(error) => {
            issues.push(DoctorIssue {
                code: "installed-state-path",
                message: error.to_string(),
                fixable: false,
            });
            false
        }
    };
    let state = match state_path_safe
        .then(|| read_json_or_default::<InstalledState>(&paths.state_path))
        .transpose()
    {
        Ok(Some(state)) => {
            validate_installed_state(state.version)?;
            validate_owned_state(&state, paths.state_prefix)?;
            if state.version != INSTALLED_STATE_VERSION {
                issues.push(DoctorIssue {
                    code: "legacy-state",
                    message: format!(
                        "{} uses installed-state schema {}; the next repair writes compact schema {}",
                        paths.state_path.display(),
                        state.version,
                        INSTALLED_STATE_VERSION
                    ),
                    fixable: true,
                });
            }
            state
        }
        Ok(None) => InstalledState::default(),
        Err(error) => {
            issues.push(DoctorIssue {
                code: "installed-state",
                message: format!("{error:#}"),
                fixable: false,
            });
            InstalledState::default()
        }
    };

    let journal_path_safe = match validate_managed_json_path(&paths.transaction_path) {
        Ok(()) => true,
        Err(error) => {
            issues.push(DoctorIssue {
                code: "transaction-path",
                message: error.to_string(),
                fixable: false,
            });
            false
        }
    };
    let journal = match journal_path_safe
        .then(|| read_optional_journal(&paths.transaction_path))
        .transpose()
    {
        Ok(journal) => journal.flatten(),
        Err(error) => {
            issues.push(DoctorIssue {
                code: "transaction",
                message: format!("{error:#}"),
                fixable: false,
            });
            None
        }
    };
    let stale_dirs = match stale_work_directories(&paths.work_root) {
        Ok(paths) => paths,
        Err(error) => {
            issues.push(DoctorIssue {
                code: "work-residue",
                message: error.to_string(),
                fixable: false,
            });
            Vec::new()
        }
    };
    for path in &stale_dirs {
        issues.push(DoctorIssue {
            code: "staging-residue",
            message: format!("owned installation residue remains at {}", path.display()),
            fixable: true,
        });
    }

    let resolved = match resolve_manifest(&migrated, &inputs.catalogs, scope.is_global()) {
        Ok(resolved) => Some(resolved),
        Err(error) => {
            issues.push(DoctorIssue {
                code: "desired-state",
                message: error.to_string(),
                fixable: false,
            });
            None
        }
    };

    let journal_authorization = match (&journal, &resolved) {
        (Some(journal), Some(resolved)) => {
            match validate_transaction(journal, scope.is_global(), &migrated, resolved, &state) {
                Ok(names) => {
                    issues.push(DoctorIssue {
                        code: "interrupted-transaction",
                        message: format!(
                            "unfinished {} transaction at phase {}",
                            scope_name,
                            phase_name(journal.phase)
                        ),
                        fixable: true,
                    });
                    Some(names)
                }
                Err(error) => {
                    issues.push(DoctorIssue {
                        code: "invalid-transaction",
                        message: error.to_string(),
                        fixable: false,
                    });
                    None
                }
            }
        }
        (Some(_), None) => {
            issues.push(DoctorIssue {
                code: "invalid-transaction",
                message: "transaction cannot be validated without a valid desired state".to_owned(),
                fixable: false,
            });
            None
        }
        (None, _) => None,
    };

    if let Some(resolved) = &resolved {
        let desired_by_key: BTreeMap<_, _> = resolved
            .iter()
            .map(|skill| (skill.key.as_str(), skill))
            .collect();
        let desired_names: BTreeSet<_> = resolved
            .iter()
            .map(|skill| skill.installed_name.as_str())
            .collect();
        for (key, installed) in &state.skills {
            match desired_by_key.get(key.as_str()) {
                None => issues.push(DoctorIssue {
                    code: "obsolete-owned-skill",
                    message: format!(
                        "owned skill {} ({key}) is no longer desired",
                        installed.installed_name
                    ),
                    fixable: true,
                }),
                Some(desired)
                    if desired.installed_name != installed.installed_name
                        || desired.mode != installed.mode
                        || desired.gitignore != installed.gitignore =>
                {
                    issues.push(DoctorIssue {
                        code: "ownership-drift",
                        message: format!("owned state for {key} differs from desired state"),
                        fixable: true,
                    });
                }
                Some(_) => {}
            }
        }
        for desired in resolved {
            if !state.skills.contains_key(&desired.key) {
                issues.push(DoctorIssue {
                    code: "missing-owned-skill",
                    message: format!("{} is desired but absent from ownership state", desired.key),
                    fixable: true,
                });
            }
        }

        let mut owned_names: BTreeSet<_> = state
            .skills
            .values()
            .map(|skill| skill.installed_name.as_str())
            .collect();
        if let Some(authorized) = &journal_authorization {
            owned_names.extend(authorized.iter().map(String::as_str));
        }
        let agent_snapshots: BTreeMap<_, _> = migrated
            .agents
            .iter()
            .map(|agent| {
                (
                    agent.as_str(),
                    list_agent_skill_names(&paths.command_root, scope.is_global(), agent)
                        .map_err(|error| error.to_string()),
                )
            })
            .collect();
        for name in desired_names {
            for root in projection_roots(&paths.command_root, scope.is_global()) {
                let entry = root.join(name);
                if (entry.exists() || entry.is_symlink()) && !owned_names.contains(name) {
                    issues.push(DoctorIssue {
                        code: "unowned-conflict",
                        message: format!(
                            "refusing to replace unowned projection {}",
                            entry.display()
                        ),
                        fixable: false,
                    });
                }
            }
            for (agent, snapshot) in &agent_snapshots {
                match snapshot {
                    Ok(installed) if installed.contains(name) && !owned_names.contains(name) => {
                        issues.push(DoctorIssue {
                            code: "unowned-conflict",
                            message: format!(
                                "refusing to replace unowned {name} for Vercel agent {agent}"
                            ),
                            fixable: false,
                        });
                    }
                    Ok(installed) if !installed.contains(name) => issues.push(DoctorIssue {
                        code: "projection-drift",
                        message: format!("{name} is missing for Vercel agent {agent}"),
                        fixable: true,
                    }),
                    Ok(_) => {}
                    Err(error) => issues.push(DoctorIssue {
                        code: "agent-target",
                        message: error.clone(),
                        fixable: false,
                    }),
                }
            }
        }
    }

    deduplicate_issues(&mut issues);
    print_report(scope_name, &issues, print)?;
    if !repair || issues.is_empty() {
        return Ok(());
    }
    if issues.iter().any(|issue| !issue.fixable) {
        bail!("Doctor found non-repairable issues; no changes were made");
    }
    let resolved = resolved.context("Doctor cannot repair unresolved desired state")?;
    if !yes && !confirm(scope_name, issues.len())? {
        println!("repair cancelled");
        return Ok(());
    }

    if migrated != inputs.manifest {
        save_manifest(
            &scope,
            &inputs.config_path,
            &inputs.global_config,
            &migrated,
        )?;
    }
    let recovery_owned = journal_authorization.unwrap_or_default();
    for path in stale_dirs {
        safe_remove_owned_dir(&path, &paths.work_root)?;
    }
    drop(resolved);
    install_with_catalogs_recovery(
        scope,
        &migrated,
        &inputs.catalogs,
        &recovery_owned,
        &recovery_owned,
        journal.is_some(),
    )?;
    println!("Doctor repaired {scope_name} Skiller state");
    Ok(())
}

fn load_inputs(scope: &InstallScope) -> Result<Inputs> {
    let global_config = load_global_config()?;
    if global_config.catalogs.is_empty() {
        bail!("no catalogs configured; run `skiller add-catalog <alias> <source>`");
    }
    let catalogs = sync_registered_catalogs(&global_config)?;
    let (config_path, manifest) = match scope {
        InstallScope::Project(project_root) => {
            let path = project_root.join("skiller.config.json");
            let manifest = read_json(&path).with_context(|| "run `skiller config` first")?;
            (path, manifest)
        }
        InstallScope::Global => (
            global_config_path()?,
            ProjectConfig {
                version: global_config.version,
                skills: global_config.skills.clone(),
                agents: global_config.agents.clone(),
            },
        ),
    };
    validate_schema(manifest.version, "skill config")?;
    Ok(Inputs {
        config_path,
        global_config,
        manifest,
        catalogs,
    })
}

fn migrate_declared_renames(
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    global_scope: bool,
) -> Result<(ProjectConfig, Vec<String>)> {
    let mut migrations = Vec::new();
    let mut targets = BTreeMap::<String, String>::new();
    for (key, selection) in &manifest.skills {
        let Some((alias, old_name)) = key.split_once('/') else {
            bail!("invalid catalog skill identifier: {key}");
        };
        let catalog = catalogs
            .get(alias)
            .with_context(|| format!("configuration references unregistered catalog: {alias}"))?;
        if catalog.skills.contains_key(old_name) {
            continue;
        }
        let Some(new_name) = resolve_rename(catalog, old_name) else {
            bail!("catalog {alias} has no skill named {old_name} and declares no rename");
        };
        let target = &catalog.skills[&new_name];
        if target.global != global_scope {
            bail!(
                "declared rename {alias}/{old_name} -> {alias}/{new_name} changes configuration eligibility"
            );
        }
        let new_key = format!("{alias}/{new_name}");
        if manifest.skills.contains_key(&new_key) {
            bail!("declared rename collides with an existing selection: {key} -> {new_key}");
        }
        if let Some(other) = targets.insert(new_key.clone(), key.clone()) {
            bail!("declared renames converge on one selection: {other}, {key} -> {new_key}");
        }
        migrations.push((key.clone(), new_key, selection.clone()));
    }

    let mut migrated = manifest.clone();
    let mut messages = Vec::new();
    for (old_key, new_key, selection) in migrations {
        migrated.skills.remove(&old_key);
        migrated.skills.insert(new_key.clone(), selection);
        messages.push(format!(
            "configuration key {old_key} will migrate to {new_key}"
        ));
    }
    Ok((migrated, messages))
}

fn validate_transaction(
    journal: &TransactionJournal,
    global: bool,
    manifest: &ProjectConfig,
    resolved: &[crate::installer::ResolvedSkill<'_>],
    state: &InstalledState,
) -> Result<BTreeSet<String>> {
    if journal.v != 1 || journal.scope != if global { "g" } else { "p" } {
        bail!("transaction journal has the wrong version or scope");
    }
    if journal.config != manifest_fingerprint(manifest)? {
        bail!("transaction configuration fingerprint does not match current configuration");
    }
    let desired: BTreeSet<_> = resolved
        .iter()
        .map(|skill| skill.installed_name.clone())
        .collect();
    validate_name_list("desired", &journal.desired)?;
    validate_name_list("remove", &journal.remove)?;
    if journal.desired != desired.iter().cloned().collect::<Vec<_>>() {
        bail!("transaction desired names do not match current resolution");
    }
    if journal.remove.iter().any(|name| desired.contains(name)) {
        bail!("transaction cannot both desire and remove the same name");
    }

    let state_is_current = state.skills.len() == resolved.len()
        && resolved.iter().all(|skill| {
            state.skills.get(&skill.key).is_some_and(|installed| {
                installed.installed_name == skill.installed_name
                    && installed.mode == skill.mode
                    && installed.gitignore == skill.gitignore
            })
        });
    if journal.phase == TransactionPhase::C && state_is_current {
        return Ok(desired);
    }

    let expected_remove: BTreeSet<_> = state
        .skills
        .values()
        .filter(|skill| !desired.contains(&skill.installed_name))
        .map(|skill| skill.installed_name.clone())
        .collect();
    if journal.remove != expected_remove.iter().cloned().collect::<Vec<_>>() {
        bail!("transaction removal names do not match prior ownership state");
    }
    Ok(desired.into_iter().chain(expected_remove).collect())
}

fn validate_name_list(kind: &str, names: &[String]) -> Result<()> {
    if names.iter().any(|name| !crate::model::valid_name(name)) {
        bail!("transaction {kind} list contains an invalid skill name");
    }
    let sorted: BTreeSet<_> = names.iter().cloned().collect();
    let normalized: Vec<_> = sorted.into_iter().collect();
    if names != normalized {
        bail!("transaction {kind} list must be sorted and unique");
    }
    Ok(())
}

fn stale_work_directories(work_root: &Path) -> Result<Vec<PathBuf>> {
    let parent = work_root
        .parent()
        .context("managed work root has no parent")?;
    if let Ok(metadata) = std::fs::symlink_metadata(parent)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        bail!(
            "managed work parent must be a real directory: {}",
            parent.display()
        );
    }
    let metadata = match std::fs::symlink_metadata(work_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", work_root.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "managed work root must be a real directory: {}",
            work_root.display()
        );
    }
    let entries =
        std::fs::read_dir(work_root).with_context(|| format!("reading {}", work_root.display()))?;
    let mut stale = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("staging-") && !name.starts_with("prepared-") {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let marker = path.join(".skiller-owned");
        let marker_is_file = std::fs::symlink_metadata(&marker)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        if metadata.file_type().is_symlink() || !metadata.is_dir() || !marker_is_file {
            bail!("refusing unverified work residue: {}", path.display());
        }
        stale.push(path);
    }
    stale.sort();
    Ok(stale)
}

fn read_optional_journal(path: &Path) -> Result<Option<TransactionJournal>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("transaction journal is not a real file: {}", path.display())
        }
        Ok(_) => read_json(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn save_manifest(
    scope: &InstallScope,
    path: &Path,
    global_config: &crate::model::GlobalConfig,
    manifest: &ProjectConfig,
) -> Result<()> {
    match scope {
        InstallScope::Project(_) => write_json_atomic(path, manifest),
        InstallScope::Global => {
            let mut config = global_config.clone();
            config.skills = manifest.skills.clone();
            config.agents = manifest.agents.clone();
            write_global_config(&config)
        }
    }
}

fn phase_name(phase: TransactionPhase) -> &'static str {
    match phase {
        TransactionPhase::P => "prepared",
        TransactionPhase::I => "installed",
        TransactionPhase::V => "verified",
        TransactionPhase::C => "cleaned",
    }
}

fn deduplicate_issues(issues: &mut Vec<DoctorIssue>) {
    let mut seen = BTreeSet::new();
    issues.retain(|issue| seen.insert((issue.code, issue.message.clone())));
}

fn print_report(scope: &str, issues: &[DoctorIssue], print: bool) -> Result<()> {
    if print {
        println!(
            "{}",
            serde_json::to_string(&DoctorReport {
                scope,
                healthy: issues.is_empty(),
                issues,
            })?
        );
    } else if issues.is_empty() {
        println!("{scope} Skiller state is healthy");
    } else {
        println!("{scope} Skiller state has {} issue(s):", issues.len());
        for issue in issues {
            println!(
                "- [{}] {}{}",
                issue.code,
                issue.message,
                if issue.fixable { " [repairable]" } else { "" }
            );
        }
    }
    Ok(())
}

fn confirm(scope: &str, issue_count: usize) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("noninteractive Doctor repair requires --yes");
    }
    print!("Repair {issue_count} {scope} issue(s)? [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("reading confirmation")?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogSkill;
    use crate::model::{
        CatalogMetadata, CatalogSkillMetadata, SCHEMA_VERSION, SelectionMode, SkillSelection,
    };

    fn renamed_catalog() -> CatalogIndex {
        CatalogIndex {
            alias: "pyg".to_owned(),
            source: "test".to_owned(),
            root: PathBuf::from("."),
            metadata: CatalogMetadata {
                version: SCHEMA_VERSION,
                scopes: BTreeMap::new(),
                skills: BTreeMap::from([(
                    "learn".to_owned(),
                    CatalogSkillMetadata {
                        scope: Some("learning".to_owned()),
                        global: true,
                    },
                )]),
                renames: BTreeMap::from([("digest".to_owned(), "learn".to_owned())]),
            },
            skills: BTreeMap::from([(
                "learn".to_owned(),
                CatalogSkill {
                    name: "learn".to_owned(),
                    description: "Learn".to_owned(),
                    scope: Some("learning".to_owned()),
                    installed_name: "learn".to_owned(),
                    global: true,
                    requires: Vec::new(),
                },
            )]),
        }
    }

    #[test]
    fn declared_rename_preserves_selection_mode() {
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "pyg/digest".to_owned(),
                SkillSelection::Mode(SelectionMode::Manual),
            )]),
            agents: crate::model::default_agents(),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), renamed_catalog())]);
        let (migrated, messages) = migrate_declared_renames(&manifest, &catalogs, true).unwrap();
        assert!(!migrated.skills.contains_key("pyg/digest"));
        assert_eq!(
            migrated.skills["pyg/learn"],
            SkillSelection::Mode(SelectionMode::Manual)
        );
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn transaction_cannot_claim_unowned_removals() {
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "pyg/learn".to_owned(),
                SkillSelection::Mode(SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), renamed_catalog())]);
        let resolved = resolve_manifest(&manifest, &catalogs, true).unwrap();
        let journal = TransactionJournal {
            v: 1,
            scope: "g".to_owned(),
            phase: TransactionPhase::P,
            config: manifest_fingerprint(&manifest).unwrap(),
            desired: vec!["learn".to_owned()],
            remove: vec!["victim".to_owned()],
        };
        assert!(
            validate_transaction(
                &journal,
                true,
                &manifest,
                &resolved,
                &InstalledState::default()
            )
            .unwrap_err()
            .to_string()
            .contains("prior ownership")
        );
    }

    #[test]
    fn converging_declared_renames_are_rejected() {
        let mut catalog = renamed_catalog();
        catalog
            .metadata
            .renames
            .insert("digest-two".to_owned(), "learn".to_owned());
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([
                (
                    "pyg/digest".to_owned(),
                    SkillSelection::Mode(SelectionMode::Manual),
                ),
                (
                    "pyg/digest-two".to_owned(),
                    SkillSelection::Mode(SelectionMode::Enable),
                ),
            ]),
            agents: crate::model::default_agents(),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog)]);
        assert!(
            migrate_declared_renames(&manifest, &catalogs, true)
                .unwrap_err()
                .to_string()
                .contains("converge")
        );
    }

    #[test]
    fn declared_rename_rejects_selection_collisions() {
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([
                (
                    "pyg/digest".to_owned(),
                    SkillSelection::Mode(SelectionMode::Manual),
                ),
                (
                    "pyg/learn".to_owned(),
                    SkillSelection::Mode(SelectionMode::Enable),
                ),
            ]),
            agents: crate::model::default_agents(),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), renamed_catalog())]);
        assert!(migrate_declared_renames(&manifest, &catalogs, true).is_err());
    }
}
