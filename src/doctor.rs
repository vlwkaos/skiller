use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::catalog::{
    CatalogAvailability, CatalogIndex, CatalogStatus, load_global_config, resolve_rename,
    sync_registered_catalogs_cached,
};
use crate::installer::{
    InstallScope, ProjectionStatus, TransactionJournal, TransactionPhase, install_paths,
    install_with_catalogs_recovery, list_agent_skill_names, manifest_fingerprint, projection_roots,
    projection_status, resolve_manifest, validate_agents, validate_owned_state,
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
    ok: bool,
    issues: &'a [DoctorIssue],
    #[serde(skip_serializing_if = "<[DoctorCatalogStatus]>::is_empty")]
    warnings: &'a [DoctorCatalogStatus],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCatalogStatus {
    alias: String,
    availability: &'static str,
    stale: bool,
    declared_count: usize,
    installed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

struct Inputs {
    config_path: PathBuf,
    global_config: crate::model::GlobalConfig,
    manifest: ProjectConfig,
    catalogs: BTreeMap<String, CatalogIndex>,
    statuses: BTreeMap<String, CatalogStatus>,
    unavailable_aliases: BTreeSet<String>,
    declared_counts: BTreeMap<String, usize>,
}

// ^ README.md#doctor-and-recovery defines the read-only and explicit-repair boundary.
pub fn run(scope: InstallScope, machine: bool, repair: bool, yes: bool) -> Result<()> {
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
            print_report(scope_name, &issues, &[], machine)?;
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
            if key
                .split_once('/')
                .is_some_and(|(alias, _)| inputs.unavailable_aliases.contains(alias))
            {
                continue;
            }
            match desired_by_key.get(key.as_str()) {
                None => {
                    let actual = paths
                        .command_root
                        .join(".agents/skills")
                        .join(&installed.installed_name);
                    let local = !scope.is_global()
                        && matches!(
                            projection_status(false, installed, None, &actual)?,
                            ProjectionStatus::KeepLocal
                                | ProjectionStatus::Conflict
                                | ProjectionStatus::Unknown
                        );
                    issues.push(DoctorIssue {
                        code: if local {
                            "orphaned-local"
                        } else {
                            "obsolete-owned-skill"
                        },
                        message: if local {
                            format!(
                                "{key} was removed from the catalog but has project changes; keeping {} unchanged",
                                installed.installed_name
                            )
                        } else {
                            format!(
                                "owned skill {} ({key}) is no longer desired",
                                installed.installed_name
                            )
                        },
                        fixable: !local,
                    });
                }
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
                Some(desired) if installed.digest.as_deref() != Some(&desired.digest) => {
                    issues.push(DoctorIssue {
                        code: "update-available",
                        message: format!("{key} differs from the current catalog version"),
                        fixable: true,
                    });
                }
                Some(_) => {}
            }
            if !scope.is_global()
                && let Some(desired) = desired_by_key.get(key.as_str())
            {
                let actual = paths
                    .command_root
                    .join(".agents/skills")
                    .join(&installed.installed_name);
                match projection_status(false, installed, Some(desired), &actual)? {
                    ProjectionStatus::KeepLocal => issues.push(DoctorIssue {
                        code: "project-override",
                        message: format!(
                            "{key} has project changes and is being kept unchanged"
                        ),
                        fixable: false,
                    }),
                    ProjectionStatus::Conflict => issues.push(DoctorIssue {
                        code: "project-conflict",
                        message: format!(
                            "{key} has both project and catalog changes; merge or promote it manually"
                        ),
                        fixable: false,
                    }),
                    ProjectionStatus::Unknown => issues.push(DoctorIssue {
                        code: "project-baseline",
                        message: format!("{key} needs a schema-4 project content baseline"),
                        fixable: true,
                    }),
                    _ => {}
                }
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
    let catalog_status = doctor_catalog_statuses(&inputs, &state);
    print_report(scope_name, &issues, &catalog_status, machine)?;
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

    if migrated != inputs.manifest && inputs.unavailable_aliases.is_empty() {
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
        &inputs.unavailable_aliases,
    )?;
    println!("Doctor repaired {scope_name} Skiller state");
    Ok(())
}

fn load_inputs(scope: &InstallScope) -> Result<Inputs> {
    let global_config = load_global_config()?;
    if global_config.catalogs.is_empty() {
        bail!("no catalogs configured; run `skiller catalog configure <alias> <source>`");
    }
    let sync = sync_registered_catalogs_cached(&global_config)?;
    let unavailable_aliases = sync.unavailable_aliases();
    let catalogs = sync.catalogs;
    let (config_path, mut manifest) = match scope {
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
    let declared_counts = global_config
        .catalogs
        .keys()
        .map(|alias| {
            (
                alias.clone(),
                manifest
                    .skills
                    .keys()
                    .filter(|key| key.starts_with(&format!("{alias}/")))
                    .count(),
            )
        })
        .collect();
    manifest.skills.retain(|key, _| {
        key.split_once('/')
            .is_none_or(|(alias, _)| !unavailable_aliases.contains(alias))
    });
    Ok(Inputs {
        config_path,
        global_config,
        manifest,
        catalogs,
        statuses: sync.statuses,
        unavailable_aliases,
        declared_counts,
    })
}

pub(crate) fn migrate_declared_renames(
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
    if journal.desired.iter().any(|name| !desired.contains(name)) {
        bail!("transaction desired names are not a safe subset of current resolution");
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
                    && installed.digest.as_deref() == Some(&skill.digest)
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

fn doctor_catalog_statuses(inputs: &Inputs, state: &InstalledState) -> Vec<DoctorCatalogStatus> {
    inputs
        .statuses
        .values()
        .filter(|status| status.availability != CatalogAvailability::Available)
        .map(|status| DoctorCatalogStatus {
            alias: status.alias.clone(),
            availability: match status.availability {
                CatalogAvailability::Available => "available",
                CatalogAvailability::Stale => "stale",
                CatalogAvailability::Unavailable => "unavailable",
            },
            stale: status.availability == CatalogAvailability::Stale,
            declared_count: inputs
                .declared_counts
                .get(&status.alias)
                .copied()
                .unwrap_or_default(),
            installed_count: state
                .skills
                .keys()
                .filter(|key| key.starts_with(&format!("{}/", status.alias)))
                .count(),
            warning: status.warning.clone(),
        })
        .collect()
}

fn print_report(
    scope: &str,
    issues: &[DoctorIssue],
    catalog_status: &[DoctorCatalogStatus],
    machine: bool,
) -> Result<()> {
    if machine {
        println!(
            "{}",
            serde_json::to_string(&DoctorReport {
                scope,
                ok: issues.is_empty(),
                issues,
                warnings: catalog_status,
            })?
        );
        return Ok(());
    }

    let output = crate::output::HumanOutput::stdout();
    if issues.is_empty() {
        println!(
            "{}",
            output.success(&format!("{scope} Skiller state is healthy"))
        );
    } else {
        println!(
            "{}",
            output.heading(&format!("{scope} diagnosis · {} issue(s)", issues.len()))
        );
        for issue in issues {
            let message = format!(
                "[{}] {}{}",
                issue.code,
                issue.message,
                if issue.fixable { " [repairable]" } else { "" }
            );
            println!(
                "{}",
                if issue.fixable {
                    output.warning(&message)
                } else {
                    output.error(&message)
                }
            );
        }
    }
    for status in catalog_status {
        println!(
            "{}",
            output.warning(&format!(
                "catalog {} is {}{}",
                status.alias,
                status.availability,
                status
                    .warning
                    .as_deref()
                    .map_or(String::new(), |warning| format!(": {warning}"))
            ))
        );
    }
    let recommendations = recommendation_commands(scope, issues, catalog_status);
    if !recommendations.is_empty() {
        println!("{}", output.heading("Recommended next steps"));
        for recommendation in recommendations {
            println!("{}", output.info(&recommendation));
        }
    }
    Ok(())
}

fn recommendation_commands(
    scope: &str,
    issues: &[DoctorIssue],
    catalog_status: &[DoctorCatalogStatus],
) -> Vec<String> {
    let scope_flag = if scope == "global" { " -g" } else { "" };
    let mut recommendations = Vec::new();
    let update_codes = ["update-available"];
    let install_codes = ["missing-owned-skill", "ownership-drift", "projection-drift"];
    if !catalog_status.is_empty()
        || issues
            .iter()
            .any(|issue| update_codes.contains(&issue.code))
    {
        recommendations.push(format!(
            "Run `skiller update{scope_flag}` to refresh and review catalog updates."
        ));
    }
    if issues
        .iter()
        .any(|issue| install_codes.contains(&issue.code))
    {
        recommendations.push(format!(
            "Run `skiller install{scope_flag}` to reconcile desired projections."
        ));
    }
    if issues.iter().any(|issue| {
        issue.fixable && !update_codes.contains(&issue.code) && !install_codes.contains(&issue.code)
    }) {
        recommendations.push(format!(
            "Run `skiller doctor{scope_flag} --repair` to repair Skiller-owned state."
        ));
    }
    recommendations
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
                    digest: "learn".to_owned(),
                    scope: Some("learning".to_owned()),
                    installed_name: "learn".to_owned(),
                    global: true,
                    requires: Vec::new(),
                },
            )]),
        }
    }

    #[test]
    fn doctor_status_reports_unavailable_catalog_without_configuration_failure() {
        let inputs = Inputs {
            config_path: PathBuf::from("test"),
            global_config: crate::model::GlobalConfig::default(),
            manifest: ProjectConfig::default(),
            catalogs: BTreeMap::new(),
            statuses: BTreeMap::from([(
                "offline".to_owned(),
                CatalogStatus {
                    alias: "offline".to_owned(),
                    availability: CatalogAvailability::Unavailable,
                    warning: Some("network unavailable".to_owned()),
                    catalog: None,
                },
            )]),
            unavailable_aliases: BTreeSet::from(["offline".to_owned()]),
            declared_counts: BTreeMap::from([("offline".to_owned(), 1)]),
        };
        let state = InstalledState {
            version: INSTALLED_STATE_VERSION,
            skills: BTreeMap::from([(
                "offline/root".to_owned(),
                crate::model::InstalledSkill {
                    installed_name: "root".to_owned(),
                    mode: crate::model::EffectiveMode::Enable,
                    gitignore: false,
                    digest: None,
                    content_digest: None,
                    legacy_path: None,
                },
            )]),
        };
        let report = doctor_catalog_statuses(&inputs, &state);
        assert_eq!(report[0].availability, "unavailable");
        assert_eq!(
            (report[0].declared_count, report[0].installed_count),
            (1, 1)
        );
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
    fn diagnosis_recommends_commands_without_mutating_or_prompting() {
        let issues = vec![
            DoctorIssue {
                code: "update-available",
                message: "published change".to_owned(),
                fixable: true,
            },
            DoctorIssue {
                code: "projection-drift",
                message: "missing projection".to_owned(),
                fixable: true,
            },
            DoctorIssue {
                code: "staging-residue",
                message: "residue".to_owned(),
                fixable: true,
            },
        ];
        assert_eq!(
            recommendation_commands("global", &issues, &[]),
            vec![
                "Run `skiller update -g` to refresh and review catalog updates.",
                "Run `skiller install -g` to reconcile desired projections.",
                "Run `skiller doctor -g --repair` to repair Skiller-owned state.",
            ]
        );
    }

    #[test]
    fn unavailable_catalog_recommends_update_without_changing_json_contract() {
        let status = DoctorCatalogStatus {
            alias: "offline".to_owned(),
            availability: "unavailable",
            stale: false,
            declared_count: 1,
            installed_count: 1,
            warning: Some("network unavailable".to_owned()),
        };
        assert_eq!(
            recommendation_commands("project", &[], &[status]),
            vec!["Run `skiller update` to refresh and review catalog updates."]
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
