use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::catalog::{
    CatalogIndex, directory_digest, load_global_config, sync_registered_catalogs_resilient,
};
use crate::manual::{apply_invocation_mode, apply_projected_identity};
use crate::model::{
    EffectiveMode, INSTALLED_STATE_VERSION, InstalledSkill, InstalledState, ProjectConfig,
    SelectionMode, validate_installed_state, validate_schema,
};
use crate::paths::{
    cache_root, copy_tree, ensure_real_dir, global_skills_root, global_state_path, output_bounded,
    project_state_root, read_json, read_json_or_default, safe_remove_owned_dir,
    sanitize_child_output, validate_managed_json_path, write_global_config, write_json_atomic,
    write_json_atomic_compact, write_json_exclusive_compact,
};

const VERCEL_SKILLS_PACKAGE: &str = "skills@1.5.23";
const IGNORE_START: &str = "# skiller:start";
const IGNORE_END: &str = "# skiller:end";
const MAX_PROJECT_SKILLS_LOCK_BYTES: u64 = 2_000_000;

#[derive(Debug, Clone)]
pub enum InstallScope {
    Project(PathBuf),
    Global,
}

impl InstallScope {
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedSkill<'a> {
    pub(crate) key: String,
    catalog: &'a CatalogIndex,
    source_name: String,
    pub(crate) installed_name: String,
    pub(crate) mode: EffectiveMode,
    pub(crate) gitignore: bool,
    pub(crate) digest: String,
}

pub(crate) struct InstallPaths {
    pub(crate) state_path: PathBuf,
    pub(crate) transaction_path: PathBuf,
    pub(crate) work_root: PathBuf,
    pub(crate) command_root: PathBuf,
    pub(crate) state_prefix: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TransactionPhase {
    P,
    I,
    V,
    C,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TransactionJournal {
    pub(crate) v: u32,
    pub(crate) scope: String,
    pub(crate) phase: TransactionPhase,
    pub(crate) config: String,
    pub(crate) desired: Vec<String>,
    pub(crate) remove: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProjectionStatus {
    Synced,
    Missing,
    Drift,
    Incoming,
}

impl ProjectionStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Synced => "SYNCED",
            Self::Missing => "MISSING",
            Self::Drift => "DRIFT",
            Self::Incoming => "UPDATE",
        }
    }
}

struct ReconcileSelection {
    install_names: BTreeSet<String>,
    complete_names: BTreeSet<String>,
    blocked: Vec<String>,
    conflicts: Vec<UnownedConflict>,
}

// ^ README.md#project-reconciliation defines the unowned-name boundary an interactive replace may cross.
#[derive(Debug, Clone)]
struct UnownedConflict {
    key: String,
    installed_name: String,
    message: String,
}

pub fn install(scope: InstallScope) -> Result<()> {
    let output = crate::output::HumanOutput::stdout();
    let error_output = crate::output::HumanOutput::stderr();
    let global_config = load_global_config()?;
    let sync = sync_registered_catalogs_resilient(&global_config)?;
    for status in sync
        .statuses
        .values()
        .filter(|status| status.warning.is_some())
    {
        eprintln!(
            "{}",
            error_output.warning(&format!(
                "catalog {} is unavailable: {}",
                status.alias,
                status.warning.as_deref().unwrap_or_default()
            ))
        );
    }
    let unavailable_aliases = sync.unavailable_aliases();
    let catalogs = sync.catalogs;
    let (config_path, manifest) = match &scope {
        InstallScope::Project(project_root) => {
            let loaded = crate::project_store::load_project_config(project_root, true, true)?;
            for message in loaded.migration {
                println!("{}", output.info(&message));
            }
            (Some(loaded.path), loaded.manifest)
        }
        InstallScope::Global => (
            None,
            ProjectConfig {
                version: global_config.version,
                skills: global_config.skills.clone(),
                agents: global_config.agents.clone(),
            },
        ),
    };
    let active = manifest_without_unavailable(&manifest, &unavailable_aliases);
    let (migrated_active, messages) =
        crate::doctor::migrate_declared_renames(&active, &catalogs, scope.is_global())?;
    let mut migrated = manifest.clone();
    migrated
        .skills
        .retain(|key, _| key_alias(key).is_some_and(|alias| unavailable_aliases.contains(alias)));
    migrated.skills.extend(migrated_active.skills);
    if migrated != manifest {
        match &scope {
            InstallScope::Project(_) => write_json_atomic(
                config_path
                    .as_deref()
                    .context("project config path is missing")?,
                &migrated,
            )?,
            InstallScope::Global => {
                let mut updated = global_config;
                updated.skills = migrated.skills.clone();
                updated.agents = migrated.agents.clone();
                write_global_config(&updated)?;
            }
        }
        for message in messages {
            println!("{}", output.success(&format!("Repaired · {message}")));
        }
    }
    install_with_catalogs_preserving(scope, &migrated, &catalogs, &unavailable_aliases)
}

pub(crate) fn install_with_catalogs_preserving(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    unavailable_aliases: &BTreeSet<String>,
) -> Result<()> {
    install_with_catalogs_recovery(
        scope,
        manifest,
        catalogs,
        &BTreeSet::new(),
        &BTreeSet::new(),
        false,
        unavailable_aliases,
    )
}

pub(crate) fn install_with_catalogs_recovery(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    replacement_owned: &BTreeSet<String>,
    cleanup_owned: &BTreeSet<String>,
    recovering: bool,
    unavailable_aliases: &BTreeSet<String>,
) -> Result<()> {
    let output = crate::output::HumanOutput::stdout();
    validate_schema(manifest.version, "skill config")?;
    validate_agents(&manifest.agents)?;
    let paths = install_paths(&scope)?;
    validate_managed_json_path(&paths.state_path)?;
    validate_managed_json_path(&paths.transaction_path)?;
    let pending_journal = if paths.transaction_path.exists() && !recovering {
        Some(
            read_json::<TransactionJournal>(&paths.transaction_path).with_context(|| {
                format!(
                    "reading unfinished transaction {}",
                    paths.transaction_path.display()
                )
            })?,
        )
    } else {
        None
    };
    let previous: InstalledState = read_json_or_default(&paths.state_path)?;
    validate_installed_state(previous.version)?;
    validate_owned_state(&previous, paths.state_prefix)?;
    let active_manifest = manifest_without_unavailable(manifest, unavailable_aliases);
    let resolved = resolve_manifest(&active_manifest, catalogs, scope.is_global())?;
    let plan = install_plan(
        &active_manifest,
        catalogs,
        &resolved,
        &previous,
        unavailable_aliases,
    )?;
    if plan.rows.is_empty() && plan.removals.is_empty() {
        println!("{}", output.info("Install plan"));
        println!("  (no configured skills)");
    }
    if !plan.rows.is_empty() {
        println!("{}", output.info(&plan.heading()));
        for line in render_install_plan(&plan.rows, output) {
            println!("  {line}");
        }
    }
    if !plan.removals.is_empty() {
        println!(
            "{}",
            output.info(&format!(
                "Remove  {} skill{}",
                plan.removals.len(),
                if plan.removals.len() == 1 { "" } else { "s" }
            ))
        );
        for name in &plan.removals {
            println!("  {}", output.tone(name, crate::output::Tone::Muted));
        }
    }
    let mut effective_replacement_owned = replacement_owned.clone();
    let mut effective_recovering = recovering;
    if let Some(journal) = &pending_journal {
        let scope_code = if scope.is_global() { "g" } else { "p" };
        let desired_names: BTreeSet<_> = resolved
            .iter()
            .map(|skill| skill.installed_name.as_str())
            .collect();
        if journal.v != 1
            || journal.scope != scope_code
            || journal.config != manifest_fingerprint(&active_manifest)?
            || journal
                .desired
                .iter()
                .any(|name| !desired_names.contains(name.as_str()))
        {
            bail!(
                "unfinished transaction is not safe for inline recovery; run `skiller doctor{}`",
                if scope.is_global() { " -g" } else { "" }
            );
        }
        effective_replacement_owned.extend(journal.desired.iter().cloned());
        effective_replacement_owned.extend(journal.remove.iter().cloned());
        effective_recovering = true;
        println!(
            "{}",
            output.info("Recovery · resuming a validated interrupted installation")
        );
    }
    let preserved: BTreeMap<_, _> = previous
        .skills
        .iter()
        .filter(|(key, _)| key_alias(key).is_some_and(|alias| unavailable_aliases.contains(alias)))
        .map(|(key, skill)| (key.clone(), skill.clone()))
        .collect();
    let reserved_names: BTreeSet<_> = preserved
        .values()
        .map(|skill| skill.installed_name.as_str())
        .collect();
    if let Some(skill) = resolved
        .iter()
        .find(|skill| reserved_names.contains(skill.installed_name.as_str()))
    {
        bail!(
            "installed skill name collision: unavailable catalog owns {}",
            skill.installed_name
        );
    }

    let environment_issues = preflight_environment(&paths, scope.is_global())?;
    if !environment_issues.is_empty() {
        println!(
            "{}",
            output.error(&format!(
                "Install preflight · {} environment issue(s)",
                environment_issues.len()
            ))
        );
        for issue in &environment_issues {
            println!(
                "{}",
                output.item(&format!("[environment-permission] {issue}"))
            );
        }
        bail!("install preflight failed before projection mutation");
    }
    ensure_real_dir(
        paths
            .work_root
            .parent()
            .context("managed work root has no parent")?,
    )?;
    ensure_real_dir(&paths.work_root)?;
    let prepared_root = paths
        .work_root
        .join(format!("prepared-{}", std::process::id()));
    let stable_prepared_root = paths.work_root.join("prepared-current");
    let setup = (|| -> Result<()> {
        safe_remove_owned_dir(&prepared_root, &paths.work_root)?;
        ensure_real_dir(&prepared_root.join("skills"))?;
        write_work_marker(&prepared_root)?;
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = safe_remove_owned_dir(&prepared_root, &paths.work_root);
        return Err(error);
    }

    let mut changed = false;
    let result = (|| -> Result<Vec<String>> {
        prepare_skills(&resolved, &prepared_root)?;
        let mut selection = classify_reconciliation(
            &paths.command_root,
            scope.is_global(),
            &previous,
            &resolved,
            &effective_replacement_owned,
            &manifest.agents,
            &prepared_root,
        )?;
        resolve_unowned_overrides(&mut selection)?;
        let lock_owned_names: BTreeSet<_> = previous
            .skills
            .values()
            .map(|skill| skill.installed_name.clone())
            .chain(resolved.iter().map(|skill| skill.installed_name.clone()))
            .chain(effective_replacement_owned.iter().cloned())
            .chain(cleanup_owned.iter().cloned())
            .collect();
        let project_root = match &scope {
            InstallScope::Project(project_root) => Some(project_root.as_path()),
            InstallScope::Global => None,
        };
        if let Some(project_root) = project_root {
            changed |= partition_project_skills_lock(project_root, &lock_owned_names)?;
        }
        let eligible: Vec<_> = resolved
            .iter()
            .filter(|skill| selection.install_names.contains(&skill.installed_name))
            .cloned()
            .collect();
        let desired_names: BTreeSet<_> = eligible
            .iter()
            .map(|skill| skill.installed_name.clone())
            .chain(selection.complete_names.iter().cloned())
            .collect();
        let removed: BTreeSet<_> = previous
            .skills
            .iter()
            .filter(|(key, skill)| {
                !preserved.contains_key(*key)
                    && !resolved
                        .iter()
                        .any(|desired| desired.installed_name == skill.installed_name)
            })
            .map(|(_, skill)| skill.installed_name.clone())
            .chain(cleanup_owned.iter().cloned())
            .collect();
        let mut complete_names = selection.complete_names;
        if !eligible.is_empty() || !removed.is_empty() {
            changed = true;
            let mut journal = TransactionJournal {
                v: 1,
                scope: if scope.is_global() { "g" } else { "p" }.to_owned(),
                phase: TransactionPhase::P,
                config: manifest_fingerprint(&active_manifest)?,
                desired: desired_names.iter().cloned().collect(),
                remove: removed.iter().cloned().collect(),
            };
            if effective_recovering {
                write_json_atomic_compact(&paths.transaction_path, &journal)?;
            } else {
                write_json_exclusive_compact(&paths.transaction_path, &journal)?;
            }
            safe_remove_owned_dir(&stable_prepared_root, &paths.work_root)?;
            std::fs::rename(&prepared_root, &stable_prepared_root).with_context(|| {
                format!(
                    "promoting prepared installation source to {}",
                    stable_prepared_root.display()
                )
            })?;
            let batch_error =
                run_vercel_partitioning_project_lock(project_root, &lock_owned_names, || {
                    run_vercel_install(
                        &paths.command_root,
                        &stable_prepared_root,
                        &eligible,
                        &manifest.agents,
                        scope.is_global(),
                    )
                })
                .err()
                .map(|error| format!("{error:#}"));
            journal.phase = TransactionPhase::I;
            write_json_atomic_compact(&paths.transaction_path, &journal)?;
            let (verified, verification_issues) = verified_skill_names(
                &paths.command_root,
                scope.is_global(),
                &eligible,
                &manifest.agents,
                &stable_prepared_root,
            );
            complete_names.extend(verified);
            let transaction_complete = verification_issues.is_empty();
            if transaction_complete {
                run_vercel_partitioning_project_lock(project_root, &lock_owned_names, || {
                    run_vercel_remove(
                        &paths.command_root,
                        &removed.iter().cloned().collect::<Vec<_>>(),
                        scope.is_global(),
                    )
                })?;
                journal.phase = TransactionPhase::C;
                write_json_atomic_compact(&paths.transaction_path, &journal)?;
            }
            let mut issues = selection.blocked;
            issues.extend(verification_issues);
            if let Some(error) = batch_error
                && eligible
                    .iter()
                    .any(|skill| !complete_names.contains(&skill.installed_name))
            {
                issues.push(error);
            }

            let next = checkpoint_state(
                &previous,
                &resolved,
                &complete_names,
                &removed,
                &stable_prepared_root,
            )?;
            write_json_atomic_compact(&paths.state_path, &next)?;
            if let InstallScope::Project(project_root) = &scope {
                update_gitignore(project_root, &next)?;
            }
            if transaction_complete {
                remove_transaction(&paths.transaction_path)?;
            }
            return Ok(issues);
        }

        let next = checkpoint_state(
            &previous,
            &resolved,
            &complete_names,
            &BTreeSet::new(),
            &prepared_root,
        )?;
        if next != previous {
            changed = true;
            write_json_atomic_compact(&paths.state_path, &next)?;
            if let InstallScope::Project(project_root) = &scope {
                update_gitignore(project_root, &next)?;
            }
        }
        if effective_recovering && selection.blocked.is_empty() {
            remove_transaction(&paths.transaction_path)?;
        }
        Ok(selection.blocked)
    })();

    let prepared_cleanup = safe_remove_owned_dir(&prepared_root, &paths.work_root);
    let stable_prepared_cleanup = safe_remove_owned_dir(&stable_prepared_root, &paths.work_root);
    let issues = result?;
    prepared_cleanup?;
    stable_prepared_cleanup?;
    if !issues.is_empty() {
        println!(
            "{}",
            output.error("Install made maximal safe progress · remaining blockers")
        );
        for issue in &issues {
            println!("{}", output.item(issue));
        }
        bail!("install remains incomplete for {} item(s)", issues.len());
    }

    let manual_count = resolved
        .iter()
        .filter(|skill| skill.mode == EffectiveMode::Manual)
        .count();
    let dependency_count = resolved
        .iter()
        .filter(|skill| skill.mode == EffectiveMode::Dependency)
        .count();
    if !changed {
        println!(
            "{}",
            output.success(&format!(
                "{} managed {} skill{} already converged",
                resolved.len(),
                if scope.is_global() {
                    "global"
                } else {
                    "project"
                },
                if resolved.len() == 1 { "" } else { "s" }
            ))
        );
        return Ok(());
    }
    println!(
        "{}",
        output.success(&format!(
            "Reconciled {} managed {} skill{} through Vercel Skills",
            resolved.len(),
            if scope.is_global() {
                "global"
            } else {
                "project"
            },
            if resolved.len() == 1 { "" } else { "s" }
        ))
    );
    if manual_count > 0 {
        println!(
            "{}",
            output.warning("Manual mode is enforced by Pi, Claude Code, Cursor, and Codex; OpenCode and Gemini CLI may still expose these skills to the model")
        );
    }
    if dependency_count > 0 {
        println!(
            "{}",
            output.warning("Dependency-only user hiding is enforced by Claude Code and Pygmalion; other agents may expose exact invocation")
        );
    }
    Ok(())
}

fn writable_ancestor_probe(path: &Path, label: &str) -> Result<()> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .with_context(|| format!("{label} has no existing writable ancestor"))?;
    }
    let metadata = std::fs::symlink_metadata(ancestor)
        .with_context(|| format!("inspecting {label} at {}", ancestor.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{label} writable ancestor is not a real directory: {}",
            ancestor.display()
        );
    }
    let probe = ancestor.join(format!(".skiller-write-probe-{}", std::process::id()));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|file| file.sync_all());
    let cleanup = std::fs::remove_file(&probe);
    result.with_context(|| format!("{label} is not writable at {}", ancestor.display()))?;
    cleanup.with_context(|| format!("removing {label} writability probe"))?;
    Ok(())
}

fn preflight_environment(paths: &InstallPaths, global_scope: bool) -> Result<Vec<String>> {
    let npm_cache = cache_root()?.join("npm");
    let canonical_projection = projection_roots(&paths.command_root, global_scope)
        .into_iter()
        .next()
        .context("canonical projection root is missing")?;
    let state_parent = paths
        .state_path
        .parent()
        .context("state path has no parent")?;
    let targets = [
        (state_parent, "state"),
        (paths.work_root.as_path(), "work"),
        (npm_cache.as_path(), "npm cache"),
        (canonical_projection.as_path(), "canonical projection"),
    ];
    let mut issues = Vec::new();
    for (path, label) in targets {
        if let Err(error) = writable_ancestor_probe(path, label) {
            issues.push(format!("{error:#}"));
        }
    }
    Ok(issues)
}

pub(crate) fn install_paths(scope: &InstallScope) -> Result<InstallPaths> {
    match scope {
        InstallScope::Project(project_root) => {
            let state_root = project_state_root(project_root)?;
            Ok(InstallPaths {
                state_path: state_root.join("installed.json"),
                transaction_path: state_root.join("transaction.json"),
                work_root: state_root,
                command_root: project_root.clone(),
                state_prefix: ".agents/skills",
            })
        }
        InstallScope::Global => {
            let home = global_skills_root()?
                .parent()
                .and_then(Path::parent)
                .context("global skills root has no home directory")?
                .to_owned();
            Ok(InstallPaths {
                state_path: global_state_path()?,
                transaction_path: crate::paths::state_root()?.join("transaction.json"),
                work_root: cache_root()?.join("install"),
                command_root: home,
                state_prefix: ".agents/skills",
            })
        }
    }
}

fn manifest_without_unavailable(
    manifest: &ProjectConfig,
    unavailable_aliases: &BTreeSet<String>,
) -> ProjectConfig {
    let mut active = manifest.clone();
    active
        .skills
        .retain(|key, _| key_alias(key).is_none_or(|alias| !unavailable_aliases.contains(alias)));
    active
}

fn key_alias(key: &str) -> Option<&str> {
    key.split_once('/').map(|(alias, _)| alias)
}

pub(crate) fn resolve_manifest<'a>(
    manifest: &ProjectConfig,
    catalogs: &'a BTreeMap<String, CatalogIndex>,
    global_scope: bool,
) -> Result<Vec<ResolvedSkill<'a>>> {
    let mut selected = BTreeMap::<String, (Option<SelectionMode>, bool, bool)>::new();
    for (key, selection) in &manifest.skills {
        selected.insert(
            key.clone(),
            (Some(selection.mode()), selection.gitignore(), false),
        );
    }

    let roots: Vec<_> = selected.keys().cloned().collect();
    let mut visited = BTreeSet::new();
    for key in roots {
        add_dependency_closure(&key, catalogs, global_scope, &mut selected, &mut visited)?;
    }

    let mut installed_names = BTreeMap::<String, String>::new();
    let mut resolved = Vec::new();
    for (key, (selected_mode, gitignore, required)) in selected {
        let (alias, source_name) = split_key(&key)?;
        let source_name = source_name.to_owned();
        let catalog = catalogs
            .get(alias)
            .with_context(|| format!("configuration references unregistered catalog: {alias}"))?;
        let skill = catalog
            .skills
            .get(&source_name)
            .with_context(|| format!("catalog {alias} has no skill named {source_name}"))?;
        if selected_mode.is_some() && skill.global != global_scope {
            bail!(
                "{} skill {key} cannot be selected in {} configuration",
                if skill.global { "global" } else { "project" },
                if global_scope { "global" } else { "project" }
            );
        }
        if global_scope && gitignore {
            bail!("global skill {key} cannot use project Git ignore state");
        }
        let installed_name = skill.installed_name.clone();
        let mode = match (selected_mode, required) {
            (Some(SelectionMode::Enable), _) | (Some(SelectionMode::Manual), true) => {
                EffectiveMode::Enable
            }
            (Some(SelectionMode::Manual), false) => EffectiveMode::Manual,
            (None, true) => EffectiveMode::Dependency,
            (None, false) => unreachable!("resolved skill is neither selected nor required"),
        };
        if let Some(other) = installed_names.insert(installed_name.clone(), key.clone()) {
            bail!("installed skill name collision: {other} and {key} both become {installed_name}");
        }
        let digest = projected_digest(skill, &installed_name, mode);
        resolved.push(ResolvedSkill {
            key,
            catalog,
            source_name,
            installed_name,
            mode,
            gitignore,
            digest,
        });
    }
    Ok(resolved)
}

fn projected_digest(
    skill: &crate::catalog::CatalogSkill,
    installed_name: &str,
    mode: EffectiveMode,
) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    let mode = match mode {
        EffectiveMode::Enable => "enable",
        EffectiveMode::Manual => "manual",
        EffectiveMode::Dependency => "dependency",
    };
    for value in [
        skill.digest.as_str(),
        installed_name,
        skill.scope.as_deref().unwrap_or(""),
        mode,
    ] {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn add_dependency_closure(
    key: &str,
    catalogs: &BTreeMap<String, CatalogIndex>,
    global_scope: bool,
    selected: &mut BTreeMap<String, (Option<SelectionMode>, bool, bool)>,
    visited: &mut BTreeSet<String>,
) -> Result<()> {
    if !visited.insert(key.to_owned()) {
        return Ok(());
    }
    let (alias, source_name) = split_key(key)?;
    let catalog = catalogs
        .get(alias)
        .with_context(|| format!("configuration references unregistered catalog: {alias}"))?;
    let skill = catalog
        .skills
        .get(source_name)
        .with_context(|| format!("catalog {alias} has no skill named {source_name}"))?;
    for dependency in &skill.requires {
        let dependency_skill = &catalog.skills[dependency];
        if global_scope && !dependency_skill.global {
            bail!(
                "global skill {key} requires project-only skill {alias}/{dependency}; mark its dependency closure global"
            );
        }
        if !global_scope && dependency_skill.global {
            continue;
        }
        let dependency_key = format!("{alias}/{dependency}");
        selected
            .entry(dependency_key.clone())
            .and_modify(|entry| entry.2 = true)
            .or_insert((None, false, true));
        add_dependency_closure(&dependency_key, catalogs, global_scope, selected, visited)?;
    }
    Ok(())
}

fn split_key(key: &str) -> Result<(&str, &str)> {
    key.split_once('/')
        .filter(|(alias, name)| !alias.is_empty() && !name.is_empty() && !name.contains('/'))
        .with_context(|| format!("invalid catalog skill identifier: {key}"))
}

/// One pre-install plan row. `branch` holds only the tree glyphs before the name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallPlanRow {
    branch: String,
    name: String,
    state: InstallPlanState,
    change: InstallPlanChange,
}

/// What installing this row will do, derived from the recorded digest baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallPlanChange {
    New,
    Update,
    Current,
}

impl InstallPlanChange {
    fn label(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Update => "update",
            Self::Current => "current",
        }
    }

    fn tone(self) -> crate::output::Tone {
        match self {
            Self::New => crate::output::Tone::Success,
            Self::Update => crate::output::Tone::Warning,
            Self::Current => crate::output::Tone::Muted,
        }
    }
}

struct InstallPlan {
    rows: Vec<InstallPlanRow>,
    removals: Vec<String>,
}

impl InstallPlan {
    /// Counts every installed skill once, then the non-zero change totals.
    fn heading(&self) -> String {
        let count = |change| self.rows.iter().filter(|row| row.change == change).count();
        let mut parts = vec![format!(
            "{} skill{}",
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" }
        )];
        let new_count = count(InstallPlanChange::New);
        if new_count > 0 {
            parts.push(format!("{new_count} new"));
        }
        let updates = count(InstallPlanChange::Update);
        if updates > 0 {
            parts.push(format!(
                "{updates} update{}",
                if updates == 1 { "" } else { "s" }
            ));
        }
        format!("Install plan  {}", parts.join("  "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallPlanState {
    Configured(SelectionMode),
    Dependency,
}

impl InstallPlanState {
    fn label(self) -> String {
        match self {
            Self::Configured(mode) => selection_mode_label(mode).to_owned(),
            Self::Dependency => "dependency".to_owned(),
        }
    }

    fn tone(self) -> crate::output::Tone {
        match self {
            Self::Configured(SelectionMode::Enable) => crate::output::Tone::Success,
            Self::Configured(SelectionMode::Manual) => crate::output::Tone::Warning,
            Self::Dependency => crate::output::Tone::Muted,
        }
    }
}

/// A skill is `update` when its recorded identity, mode, or digest differs.
fn install_change(previous: &InstalledState, skill: &ResolvedSkill<'_>) -> InstallPlanChange {
    match previous.skills.get(&skill.key) {
        None => InstallPlanChange::New,
        Some(installed)
            if installed.installed_name != skill.installed_name
                || installed.mode != skill.mode
                || installed.gitignore != skill.gitignore
                || installed.digest.as_deref() != Some(skill.digest.as_str()) =>
        {
            InstallPlanChange::Update
        }
        Some(_) => InstallPlanChange::Current,
    }
}

// ^ README.md#configuration documents the install plan shown before projection mutation.
fn install_plan(
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    resolved: &[ResolvedSkill<'_>],
    previous: &InstalledState,
    unavailable_aliases: &BTreeSet<String>,
) -> Result<InstallPlan> {
    let resolved_by_key: BTreeMap<_, _> = resolved
        .iter()
        .map(|skill| (skill.key.as_str(), skill))
        .collect();
    let context = PlanContext {
        manifest,
        catalogs,
        resolved: &resolved_by_key,
        previous,
    };
    let mut rows = Vec::new();
    for (key, selection) in &manifest.skills {
        let Some(root) = resolved_by_key.get(key.as_str()) else {
            continue;
        };
        rows.push(InstallPlanRow {
            branch: String::new(),
            name: root.installed_name.clone(),
            state: InstallPlanState::Configured(selection.mode()),
            change: install_change(previous, root),
        });
        append_dependency_rows(&context, &root.key, root, "", &mut rows)?;
    }
    let removals = previous
        .skills
        .iter()
        .filter(|(key, _)| {
            !key_alias(key).is_some_and(|alias| unavailable_aliases.contains(alias))
                && !resolved_by_key.contains_key(key.as_str())
        })
        .map(|(_, skill)| skill.installed_name.clone())
        .collect();
    Ok(InstallPlan { rows, removals })
}

/// Read-only inputs shared by every recursion step.
struct PlanContext<'a, 'c> {
    manifest: &'a ProjectConfig,
    catalogs: &'a BTreeMap<String, CatalogIndex>,
    resolved: &'a BTreeMap<&'a str, &'a ResolvedSkill<'c>>,
    previous: &'a InstalledState,
}

fn append_dependency_rows(
    context: &PlanContext<'_, '_>,
    parent_key: &str,
    parent: &ResolvedSkill<'_>,
    prefix: &str,
    rows: &mut Vec<InstallPlanRow>,
) -> Result<()> {
    let (alias, _) = split_key(parent_key)?;
    let catalog = context
        .catalogs
        .get(alias)
        .with_context(|| format!("configuration references unregistered catalog: {alias}"))?;
    let dependencies: Vec<_> = catalog.skills[&parent.source_name]
        .requires
        .iter()
        .map(|name| format!("{alias}/{name}"))
        .filter(|key| context.resolved.contains_key(key.as_str()))
        .collect();
    for (index, dependency_key) in dependencies.iter().enumerate() {
        let last = index + 1 == dependencies.len();
        let child = context.resolved[dependency_key.as_str()];
        rows.push(InstallPlanRow {
            branch: format!("{prefix}{}", if last { "└─ " } else { "├─ " }),
            name: child.installed_name.clone(),
            state: match context.manifest.skills.get(dependency_key) {
                Some(selection) => InstallPlanState::Configured(selection.mode()),
                None => InstallPlanState::Dependency,
            },
            change: install_change(context.previous, child),
        });
        append_dependency_rows(
            context,
            dependency_key,
            child,
            &format!("{prefix}{}", if last { "   " } else { "│  " }),
            rows,
        )?;
    }
    Ok(())
}

/// Aligns the name column and colors only the state column, so names stay scannable.
fn render_install_plan(rows: &[InstallPlanRow], output: crate::output::HumanOutput) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let labels: Vec<_> = rows
        .iter()
        .map(|row| format!("{}{}", row.branch, row.name))
        .collect();
    let states: Vec<_> = rows.iter().map(|row| row.state.label()).collect();
    let width = labels.iter().map(|label| label.width()).max().unwrap_or(0);
    let state_width = states.iter().map(|state| state.width()).max().unwrap_or(0);
    labels
        .into_iter()
        .zip(states)
        .zip(rows)
        .map(|((label, state), row)| {
            let state = format!(
                "{state}{}",
                " ".repeat(state_width.saturating_sub(state.width()))
            );
            format!(
                "{label}{}  {}  {}",
                " ".repeat(width.saturating_sub(label.width())),
                output.tone(&state, row.state.tone()),
                output.tone(row.change.label(), row.change.tone())
            )
        })
        .collect()
}

fn selection_mode_label(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Enable => "Agent + Human",
        SelectionMode::Manual => "Human only",
    }
}

fn prepare_skills(resolved: &[ResolvedSkill<'_>], prepared_root: &Path) -> Result<()> {
    for skill in resolved {
        let source = skill.catalog.root.join("skills").join(&skill.source_name);
        let destination = prepared_root.join("skills").join(&skill.installed_name);
        copy_tree(&source, &destination)?;
        apply_projected_identity(
            &destination,
            &skill.installed_name,
            skill.catalog.skills[&skill.source_name].scope.as_deref(),
            &skill.catalog.skills[&skill.source_name].description,
        )?;
        apply_invocation_mode(&destination, skill.mode)?;
    }
    Ok(())
}

fn project_skills_lock_path(project_root: &Path) -> PathBuf {
    project_root.join("skills-lock.json")
}

fn read_project_skills_lock(project_root: &Path) -> Result<Option<serde_json::Value>> {
    let path = project_skills_lock_path(project_root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "project Skills lock must be a real file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_PROJECT_SKILLS_LOCK_BYTES {
        bail!("project Skills lock exceeds 2 MB: {}", path.display());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(value))
}

fn managed_lock_sources(project_root: &Path) -> Result<BTreeSet<String>> {
    let current = project_state_root(project_root)?.join("prepared-current");
    let mut sources = BTreeSet::from([".skiller/prepared-current".to_owned()]);
    sources.insert(current.to_string_lossy().replace('\\', "/"));
    if let Ok(relative) = current.strip_prefix(project_root) {
        sources.insert(relative.to_string_lossy().replace('\\', "/"));
    }
    Ok(sources)
}

fn is_skiller_lock_entry(entry: &serde_json::Value, sources: &BTreeSet<String>) -> bool {
    if entry.get("sourceType").and_then(serde_json::Value::as_str) != Some("local") {
        return false;
    }
    let Some(source) = entry.get("source").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let normalized = source.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    sources.contains(normalized)
}

fn managed_lock_names_in(
    value: &serde_json::Value,
    owned_names: &BTreeSet<String>,
    sources: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let skills = value
        .get("skills")
        .and_then(serde_json::Value::as_object)
        .context("project Skills lock has no skills object")?;
    Ok(skills
        .iter()
        .filter(|(name, entry)| {
            owned_names.contains(*name) && is_skiller_lock_entry(entry, sources)
        })
        .map(|(name, _)| name.clone())
        .collect())
}

pub(crate) fn managed_project_lock_names(
    project_root: &Path,
    owned_names: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let Some(value) = read_project_skills_lock(project_root)? else {
        return Ok(BTreeSet::new());
    };
    managed_lock_names_in(&value, owned_names, &managed_lock_sources(project_root)?)
}

fn partition_project_skills_lock(
    project_root: &Path,
    owned_names: &BTreeSet<String>,
) -> Result<bool> {
    let Some(mut value) = read_project_skills_lock(project_root)? else {
        return Ok(false);
    };
    let managed = managed_lock_names_in(&value, owned_names, &managed_lock_sources(project_root)?)?;
    if managed.is_empty() {
        return Ok(false);
    }
    let skills = value
        .get_mut("skills")
        .and_then(serde_json::Value::as_object_mut)
        .context("project Skills lock has no skills object")?;
    for name in &managed {
        skills.remove(name);
    }
    let path = project_skills_lock_path(project_root);
    if skills.is_empty() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        write_json_atomic(&path, &value)?;
    }
    Ok(true)
}

fn run_vercel_partitioning_project_lock(
    project_root: Option<&Path>,
    owned_names: &BTreeSet<String>,
    run: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let result = run();
    let cleanup = project_root
        .map(|root| partition_project_skills_lock(root, owned_names))
        .transpose();
    match (result, cleanup) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "project Skills lock cleanup also failed: {cleanup:#}"
        ))),
    }
}

// ^ README.md#project-skills-lock partitions Skiller ownership from native Skills entries.
fn run_vercel_install(
    command_root: &Path,
    prepared_root: &Path,
    resolved: &[ResolvedSkill<'_>],
    agents: &[String],
    global_scope: bool,
) -> Result<()> {
    if resolved.is_empty() {
        return Ok(());
    }
    let mut command = vercel_command();
    command.arg("add").arg(prepared_root);
    for skill in resolved {
        command.args(["--skill", &skill.installed_name]);
    }
    // ^ skills@1.5.23 owns placement for every persisted agent target.
    append_vercel_install_targets(&mut command, agents);
    if global_scope {
        command.arg("--global");
    }
    command.current_dir(command_root);
    run_command(command, "installing prepared skills")
}

fn append_vercel_install_targets(command: &mut Command, agents: &[String]) {
    command.arg("--agent").args(agents).arg("--yes");
}

pub(crate) fn validate_agents(agents: &[String]) -> Result<()> {
    if agents.is_empty() {
        bail!("skill configuration must select at least one Vercel agent");
    }
    let mut seen = BTreeSet::new();
    for agent in agents {
        if agent.trim().is_empty() || agent != agent.trim() || agent.chars().any(char::is_control) {
            bail!("invalid empty or whitespace Vercel agent name");
        }
        if !seen.insert(agent) {
            bail!("duplicate Vercel agent name: {agent}");
        }
    }
    Ok(())
}

fn run_vercel_remove(command_root: &Path, names: &[String], global_scope: bool) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut command = vercel_command();
    command.arg("remove");
    for name in names {
        command.arg(name);
    }
    // ^ skills@1.5.23 remove cleans all agent links only when --agent is omitted.
    command.arg("--yes");
    if global_scope {
        command.arg("--global");
    }
    command.current_dir(command_root);
    run_command(command, "removing obsolete managed skills")
}

fn vercel_command() -> Command {
    let mut command = Command::new("npx");
    command
        .args(["--yes", VERCEL_SKILLS_PACKAGE])
        .env("npm_config_ignore_scripts", "true")
        .env("NO_COLOR", "1");
    if let Ok(root) = cache_root() {
        command.env("npm_config_cache", root.join("npm"));
    }
    command
}

pub(crate) fn manifest_fingerprint(manifest: &ProjectConfig) -> Result<String> {
    let bytes = serde_json::to_vec(manifest)?;
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{hash:016x}"))
}

pub(crate) fn remove_transaction(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!(
                "refusing to remove invalid transaction journal: {}",
                path.display()
            )
        }
        Ok(_) => std::fs::remove_file(path)
            .with_context(|| format!("removing transaction journal {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn bounded_environment_error(error: anyhow::Error) -> anyhow::Error {
    let detail = format!("{error:#}");
    let class = if detail.contains("exceeded") {
        "environment-timeout"
    } else {
        "environment-process"
    };
    anyhow::anyhow!("[{class}] {detail}")
}

fn child_failure(action: &str, stdout: &[u8], stderr: &[u8]) -> anyhow::Error {
    let detail = format!(
        "{}{}",
        sanitize_child_output(stdout),
        sanitize_child_output(stderr)
    );
    let lower = detail.to_ascii_lowercase();
    let class = if ["eacces", "eperm", "permission denied", "sandbox"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        "environment-permission"
    } else if ["enotfound", "econn", "network", "fetch failed", "timed out"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        "environment-network"
    } else {
        "placement"
    };
    anyhow::anyhow!("[{class}] Vercel Skills failed while {action}: {detail}")
}

fn run_command(mut command: Command, action: &str) -> Result<()> {
    let (status, stdout, stderr) = output_bounded(&mut command, action, Duration::from_secs(60))
        .map_err(bounded_environment_error)?;
    if !status.success() {
        return Err(child_failure(action, &stdout, &stderr));
    }
    Ok(())
}

pub(crate) fn projection_roots(command_root: &Path, global_scope: bool) -> Vec<PathBuf> {
    let relative_roots: &[&str] = if global_scope {
        &[
            ".agents/skills",
            ".claude/skills",
            ".codex/skills",
            ".config/opencode/skills",
            ".cursor/skills",
            ".gemini/skills",
            ".hermes/skills",
            ".pi/agent/skills",
        ]
    } else {
        &[
            ".agents/skills",
            ".claude/skills",
            ".codex/skills",
            ".config/opencode/skills",
            ".cursor/skills",
            ".gemini/skills",
            ".hermes/skills",
            ".opencode/skills",
            ".pi/skills",
        ]
    };
    relative_roots
        .iter()
        .map(|path| command_root.join(path))
        .collect()
}

fn checkpoint_state(
    previous: &InstalledState,
    resolved: &[ResolvedSkill<'_>],
    complete_names: &BTreeSet<String>,
    removed_names: &BTreeSet<String>,
    prepared_root: &Path,
) -> Result<InstalledState> {
    let mut skills = previous.skills.clone();
    skills.retain(|_, skill| !removed_names.contains(&skill.installed_name));
    for skill in resolved {
        if !complete_names.contains(&skill.installed_name) {
            continue;
        }
        skills.retain(|key, installed| {
            key == &skill.key || installed.installed_name != skill.installed_name
        });
        let intended_digest =
            directory_digest(&prepared_root.join("skills").join(&skill.installed_name))?;
        let content_digest = intended_digest;
        skills.insert(
            skill.key.clone(),
            InstalledSkill {
                installed_name: skill.installed_name.clone(),
                mode: skill.mode,
                gitignore: skill.gitignore,
                digest: Some(skill.digest.clone()),
                content_digest: Some(content_digest),
                legacy_path: None,
            },
        );
    }
    Ok(InstalledState {
        version: INSTALLED_STATE_VERSION,
        skills,
    })
}

pub(crate) fn projection_status(
    _global_scope: bool,
    installed: &InstalledSkill,
    desired: Option<&ResolvedSkill<'_>>,
    actual: &Path,
) -> Result<ProjectionStatus> {
    if !actual.is_dir() {
        return Ok(ProjectionStatus::Missing);
    }
    let current = directory_digest(actual)?;
    if installed
        .content_digest
        .as_ref()
        .is_none_or(|baseline| baseline != &current)
    {
        return Ok(ProjectionStatus::Drift);
    }
    let incoming = desired.is_some_and(|desired| {
        installed.installed_name != desired.installed_name
            || installed.mode != desired.mode
            || installed.gitignore != desired.gitignore
            || installed.digest.as_deref() != Some(&desired.digest)
    });
    Ok(if incoming {
        ProjectionStatus::Incoming
    } else {
        ProjectionStatus::Synced
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplacementChoice {
    This,
    All,
    Keep,
}

/// Unowned divergent names stay blocked unless an interactive operator approves replacement.
/// Approved names reuse the same reinstall path as owned drift, so no extra deletion logic exists.
fn resolve_unowned_overrides(selection: &mut ReconcileSelection) -> Result<()> {
    if selection.conflicts.is_empty() || !install_is_interactive() {
        return Ok(());
    }
    let output = crate::output::HumanOutput::stdout();
    println!(
        "{}",
        output.heading(&format!(
            "Conflicts: {} selected skill{} already exist and are not managed by Skiller",
            selection.conflicts.len(),
            if selection.conflicts.len() == 1 {
                ""
            } else {
                "s"
            }
        ))
    );
    for conflict in &selection.conflicts {
        println!(
            "  {}",
            output.tone(&conflict.installed_name, crate::output::Tone::Muted)
        );
    }
    println!(
        "{}",
        output
            .warning("Replacing one deletes its current content; keeping one leaves it unchanged")
    );
    let mut replace_all = false;
    let mut approved = BTreeSet::new();
    for conflict in &selection.conflicts {
        let replace = if replace_all {
            true
        } else {
            match prompt_unowned_replacement(&conflict.installed_name)? {
                ReplacementChoice::All => {
                    replace_all = true;
                    true
                }
                ReplacementChoice::This => true,
                ReplacementChoice::Keep => false,
            }
        };
        if replace {
            approved.insert(conflict.message.clone());
        }
    }
    if approved.is_empty() {
        return Ok(());
    }
    for conflict in &selection.conflicts {
        if approved.contains(&conflict.message) {
            selection
                .install_names
                .insert(conflict.installed_name.clone());
            selection
                .blocked
                .retain(|message| message != &conflict.message);
        }
    }
    Ok(())
}

fn install_is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// EOF and any unrecognized answer keep the existing content, so the safe choice is the default.
fn prompt_unowned_replacement(installed_name: &str) -> Result<ReplacementChoice> {
    print!(
        "  Replace {installed_name} with the catalog version? [y] replace  [Y] replace all  [n] keep: "
    );
    io::stdout()
        .flush()
        .context("flushing replacement prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("reading replacement choice")?;
    Ok(match input.trim() {
        "y" | "yes" => ReplacementChoice::This,
        "Y" | "YES" | "a" | "all" => ReplacementChoice::All,
        _ => ReplacementChoice::Keep,
    })
}

fn unowned_conflict_message(key: &str, installed_name: &str, global_scope: bool) -> String {
    format!(
        "[unowned-conflict] {key} was not installed: a skill named {installed_name} already exists and is not managed by Skiller. The existing copy was left unchanged. Remove or rename it and rerun to let Skiller manage this name, or stop trying with `skiller config{} --set {key}=off`",
        if global_scope { " -g" } else { "" }
    )
}

fn exact_projection_matches(actual: &Path, intended: &Path) -> Result<bool> {
    let actual = match std::fs::canonicalize(actual) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("resolving {}", actual.display())),
    };
    if !actual.is_dir() || !intended.is_dir() {
        return Ok(false);
    }
    Ok(directory_digest(&actual)? == directory_digest(intended)?)
}

fn classify_reconciliation(
    command_root: &Path,
    global_scope: bool,
    previous: &InstalledState,
    resolved: &[ResolvedSkill<'_>],
    recovery_owned: &BTreeSet<String>,
    agents: &[String],
    prepared_root: &Path,
) -> Result<ReconcileSelection> {
    let mut owned: BTreeSet<_> = previous
        .skills
        .values()
        .map(|skill| skill.installed_name.clone())
        .collect();
    owned.extend(recovery_owned.iter().cloned());
    let roots = projection_roots(command_root, global_scope);
    let agent_names: Vec<_> = agents
        .iter()
        .map(|agent| list_agent_skill_names(command_root, global_scope, agent))
        .collect::<Result<_>>()?;
    let mut selection = ReconcileSelection {
        install_names: BTreeSet::new(),
        complete_names: BTreeSet::new(),
        blocked: Vec::new(),
        conflicts: Vec::new(),
    };
    for skill in resolved {
        let intended = prepared_root.join("skills").join(&skill.installed_name);
        let existing: Vec<_> = roots
            .iter()
            .map(|root| root.join(&skill.installed_name))
            .filter(|path| path.exists() || path.is_symlink())
            .collect();
        let all_agents_have = agent_names
            .iter()
            .all(|names| names.contains(&skill.installed_name));
        let all_existing_match = existing
            .iter()
            .map(|path| exact_projection_matches(path, &intended))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .all(|matches| matches);
        let installed = previous.skills.get(&skill.key);
        let state_current = installed.is_some_and(|installed| {
            installed.installed_name == skill.installed_name
                && installed.mode == skill.mode
                && installed.gitignore == skill.gitignore
                && installed.digest.as_deref() == Some(&skill.digest)
        });
        if owned.contains(&skill.installed_name) {
            if state_current && all_agents_have && !existing.is_empty() && all_existing_match {
                selection
                    .complete_names
                    .insert(skill.installed_name.clone());
            } else {
                selection.install_names.insert(skill.installed_name.clone());
            }
        } else if existing.is_empty()
            && !agent_names
                .iter()
                .any(|names| names.contains(&skill.installed_name))
        {
            selection.install_names.insert(skill.installed_name.clone());
        } else if all_existing_match && !existing.is_empty() {
            if all_agents_have {
                selection
                    .complete_names
                    .insert(skill.installed_name.clone());
            } else {
                selection.install_names.insert(skill.installed_name.clone());
            }
        } else {
            let message = unowned_conflict_message(&skill.key, &skill.installed_name, global_scope);
            selection.blocked.push(message.clone());
            selection.conflicts.push(UnownedConflict {
                key: skill.key.clone(),
                installed_name: skill.installed_name.clone(),
                message,
            });
        }
    }

    let mut blocked_names: BTreeSet<_> = selection
        .conflicts
        .iter()
        .filter_map(|conflict| {
            resolved
                .iter()
                .find(|skill| skill.key == conflict.key)
                .map(|skill| skill.source_name.clone())
        })
        .collect();
    loop {
        let mut changed = false;
        for skill in resolved {
            if blocked_names.contains(&skill.source_name) {
                continue;
            }
            if skill.catalog.skills[&skill.source_name]
                .requires
                .iter()
                .any(|dependency| blocked_names.contains(dependency))
            {
                blocked_names.insert(skill.source_name.clone());
                selection.install_names.remove(&skill.installed_name);
                selection.complete_names.remove(&skill.installed_name);
                selection.blocked.push(format!(
                    "[dependency-blocked] {} depends on a blocked skill",
                    skill.key
                ));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(selection)
}

fn verified_skill_names(
    command_root: &Path,
    global_scope: bool,
    resolved: &[ResolvedSkill<'_>],
    agents: &[String],
    prepared_root: &Path,
) -> (BTreeSet<String>, Vec<String>) {
    let snapshots: Result<Vec<_>> = agents
        .iter()
        .map(|agent| list_agent_skill_names(command_root, global_scope, agent))
        .collect();
    let snapshots = match snapshots {
        Ok(snapshots) => snapshots,
        Err(error) => return (BTreeSet::new(), vec![format!("[environment] {error:#}")]),
    };
    let canonical_root = projection_roots(command_root, global_scope)
        .into_iter()
        .next()
        .unwrap_or_else(|| command_root.join(".agents/skills"));
    let mut verified = BTreeSet::new();
    let mut issues = Vec::new();
    for skill in resolved {
        let listed = snapshots
            .iter()
            .all(|names| names.contains(&skill.installed_name));
        let actual = canonical_root.join(&skill.installed_name);
        let intended = prepared_root.join("skills").join(&skill.installed_name);
        let content_matches = exact_projection_matches(&actual, &intended).unwrap_or(false);
        if listed && content_matches {
            verified.insert(skill.installed_name.clone());
        } else {
            issues.push(format!(
                "[projection-drift] {} was not completely placed and verified",
                skill.key
            ));
        }
    }
    (verified, issues)
}

fn known_agent_projection_root(
    command_root: &Path,
    _global_scope: bool,
    agent: &str,
) -> Option<PathBuf> {
    let relative = match agent {
        "universal" | "claude-code" | "pi" => ".agents/skills",
        _ => return None,
    };
    Some(command_root.join(relative))
}

fn directory_skill_names(root: &Path) -> Result<BTreeSet<String>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", root.display()))?;
        if entry.path().is_dir() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

pub(crate) fn list_agent_skill_names(
    command_root: &Path,
    global_scope: bool,
    agent: &str,
) -> Result<BTreeSet<String>> {
    if let Some(root) = known_agent_projection_root(command_root, global_scope, agent) {
        return directory_skill_names(&root);
    }
    let mut command = vercel_command();
    command.args(["list", "--json", "--agent", agent]);
    if global_scope {
        command.arg("--global");
    }
    command.current_dir(command_root);
    let (status, stdout, stderr) = output_bounded(
        &mut command,
        &format!("listing skills for agent {agent}"),
        Duration::from_secs(15),
    )
    .map_err(bounded_environment_error)?;
    if !status.success() {
        return Err(child_failure(
            &format!("listing skills for agent {agent}"),
            &stdout,
            &stderr,
        ));
    }
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&stdout)
        .with_context(|| format!("parsing Vercel Skills list for agent {agent}"))?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.get("name")?.as_str().map(str::to_owned))
        .collect())
}

fn write_work_marker(root: &Path) -> Result<()> {
    std::fs::write(root.join(".skiller-owned"), b"1\n")
        .with_context(|| format!("marking owned work directory {}", root.display()))
}

pub(crate) fn validate_owned_state(state: &InstalledState, prefix: &str) -> Result<()> {
    for (key, skill) in &state.skills {
        split_key(key)?;
        if !crate::model::valid_name(&skill.installed_name) {
            bail!(
                "installed state contains an unsafe owned name: {}",
                skill.installed_name
            );
        }
        if let Some(path) = &skill.legacy_path {
            let expected = format!("{prefix}/{}", skill.installed_name);
            if path != &expected {
                bail!("installed state contains an unsafe owned path: {path}");
            }
        }
    }
    Ok(())
}

fn update_gitignore(project_root: &Path, state: &InstalledState) -> Result<()> {
    let path = project_root.join(".gitignore");
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing to edit symlinked .gitignore");
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("reading .gitignore"),
    };
    let mut kept = Vec::new();
    let mut inside = false;
    for line in raw.lines() {
        if line == IGNORE_START {
            if inside {
                bail!(".gitignore contains nested Skiller marker blocks");
            }
            inside = true;
            continue;
        }
        if line == IGNORE_END {
            if !inside {
                bail!(".gitignore contains an unmatched Skiller end marker");
            }
            inside = false;
            continue;
        }
        if !inside {
            kept.push(line.to_owned());
        }
    }
    if inside {
        bail!(".gitignore contains an unterminated Skiller marker block");
    }
    while kept.last().is_some_and(String::is_empty) {
        kept.pop();
    }
    if !kept.is_empty() {
        kept.push(String::new());
    }
    kept.push(IGNORE_START.to_owned());
    for skill in state.skills.values().filter(|skill| skill.gitignore) {
        kept.push(format!("/**/skills/{}", skill.installed_name));
    }
    kept.push(IGNORE_END.to_owned());
    std::fs::write(&path, format!("{}\n", kept.join("\n"))).context("writing .gitignore")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogSkill;
    use crate::model::{
        CatalogMetadata, EffectiveMode, GlobalConfig, SCHEMA_VERSION, SkillSelection,
    };

    fn init_git(root: &Path) {
        assert!(
            Command::new("git")
                .arg("init")
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
    }

    fn catalog(global: bool) -> CatalogIndex {
        CatalogIndex {
            alias: "pyg".to_owned(),
            source: "test".to_owned(),
            root: PathBuf::from("."),
            metadata: CatalogMetadata::default(),
            skills: BTreeMap::from([
                (
                    "root".to_owned(),
                    CatalogSkill {
                        name: "root".to_owned(),
                        description: "Root".to_owned(),
                        digest: "root".to_owned(),
                        scope: Some("engineering".to_owned()),
                        installed_name: "root".to_owned(),
                        global,
                        recommend: None,
                        requires: vec!["dependency".to_owned()],
                    },
                ),
                (
                    "dependency".to_owned(),
                    CatalogSkill {
                        name: "dependency".to_owned(),
                        description: "Dependency".to_owned(),
                        digest: "dependency".to_owned(),
                        scope: Some("engineering".to_owned()),
                        installed_name: "dependency".to_owned(),
                        global,
                        recommend: None,
                        requires: Vec::new(),
                    },
                ),
            ]),
        }
    }

    #[test]
    fn scope_filters_roots_and_adds_dependency_closure() {
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "pyg/root".to_owned(),
                SkillSelection::Mode(SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(true))]);
        let resolved = resolve_manifest(&manifest, &catalogs, true).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].installed_name, "dependency");
        assert_eq!(resolved[0].mode, EffectiveMode::Dependency);
        assert_eq!(resolved[1].installed_name, "root");
        assert_eq!(resolved[1].mode, EffectiveMode::Enable);
        assert!(resolve_manifest(&manifest, &catalogs, false).is_err());
    }

    #[test]
    fn configured_mode_and_dependency_reachability_form_effective_capabilities() {
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(true))]);
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([
                (
                    "pyg/root".to_owned(),
                    SkillSelection::Mode(SelectionMode::Manual),
                ),
                (
                    "pyg/dependency".to_owned(),
                    SkillSelection::Mode(SelectionMode::Manual),
                ),
            ]),
            agents: crate::model::default_agents(),
        };
        let resolved = resolve_manifest(&manifest, &catalogs, true).unwrap();
        assert_eq!(resolved[0].mode, EffectiveMode::Enable);
        assert_eq!(resolved[1].mode, EffectiveMode::Manual);
    }

    #[test]
    fn install_forest_lists_every_root_and_dependency_owner() {
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(true))]);
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([
                (
                    "pyg/root".to_owned(),
                    SkillSelection::Mode(SelectionMode::Enable),
                ),
                (
                    "pyg/dependency".to_owned(),
                    SkillSelection::Mode(SelectionMode::Manual),
                ),
            ]),
            agents: crate::model::default_agents(),
        };
        let resolved = resolve_manifest(&manifest, &catalogs, true).unwrap();
        let plan = install_plan(
            &manifest,
            &catalogs,
            &resolved,
            &InstalledState::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            plan.rows,
            vec![
                InstallPlanRow {
                    branch: String::new(),
                    name: "dependency".to_owned(),
                    state: InstallPlanState::Configured(SelectionMode::Manual),
                    change: InstallPlanChange::New,
                },
                InstallPlanRow {
                    branch: String::new(),
                    name: "root".to_owned(),
                    state: InstallPlanState::Configured(SelectionMode::Enable),
                    change: InstallPlanChange::New,
                },
                InstallPlanRow {
                    branch: "└─ ".to_owned(),
                    name: "dependency".to_owned(),
                    state: InstallPlanState::Configured(SelectionMode::Manual),
                    change: InstallPlanChange::New,
                },
            ]
        );
        assert!(plan.removals.is_empty());
        assert_eq!(plan.heading(), "Install plan  3 skills  3 new");

        // Names drop the repeated catalog prefix, and states and changes align.
        let rendered = render_install_plan(&plan.rows, crate::output::HumanOutput::plain());
        assert_eq!(
            rendered,
            vec![
                "dependency     Human only     new",
                "root           Agent + Human  new",
                "└─ dependency  Human only     new",
            ]
        );
        assert!(rendered.iter().all(|line| !line.contains("pyg/")));
        assert!(
            render_install_plan(&plan.rows, crate::output::HumanOutput::styled())
                .iter()
                .all(|line| line.contains('\u{1b}'))
        );
    }

    #[test]
    fn install_plan_aligns_nested_dependency_branches() {
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(false))]);
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "pyg/root".to_owned(),
                SkillSelection::Mode(SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let resolved = resolve_manifest(&manifest, &catalogs, false).unwrap();
        let plan = install_plan(
            &manifest,
            &catalogs,
            &resolved,
            &InstalledState::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            render_install_plan(&plan.rows, crate::output::HumanOutput::plain()),
            vec![
                "root           Agent + Human  new",
                "└─ dependency  dependency     new",
            ]
        );
    }

    #[test]
    fn install_plan_reports_updates_and_removals_from_recorded_state() {
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(false))]);
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([(
                "pyg/root".to_owned(),
                SkillSelection::Mode(SelectionMode::Enable),
            )]),
            agents: crate::model::default_agents(),
        };
        let resolved = resolve_manifest(&manifest, &catalogs, false).unwrap();
        // The recorded baseline is the projected digest the resolver produces.
        let current_digest = resolved
            .iter()
            .find(|skill| skill.key == "pyg/root")
            .unwrap()
            .digest
            .clone();
        let installed = |name: &str, mode: EffectiveMode, digest: String| InstalledSkill {
            installed_name: name.to_owned(),
            mode,
            gitignore: false,
            digest: Some(digest),
            content_digest: None,
            legacy_path: None,
        };
        let previous = InstalledState {
            version: INSTALLED_STATE_VERSION,
            skills: BTreeMap::from([
                (
                    "pyg/root".to_owned(),
                    installed("root", EffectiveMode::Enable, current_digest),
                ),
                (
                    "pyg/dependency".to_owned(),
                    installed("dependency", EffectiveMode::Dependency, "stale".to_owned()),
                ),
                (
                    "pyg/memo".to_owned(),
                    installed("memo", EffectiveMode::Enable, "memo".to_owned()),
                ),
            ]),
        };
        let plan =
            install_plan(&manifest, &catalogs, &resolved, &previous, &BTreeSet::new()).unwrap();
        assert_eq!(plan.heading(), "Install plan  2 skills  1 update");
        assert_eq!(plan.removals, vec!["memo".to_owned()]);
        assert_eq!(
            plan.rows
                .iter()
                .map(|row| (row.name.as_str(), row.change))
                .collect::<Vec<_>>(),
            vec![
                ("root", InstallPlanChange::Current),
                ("dependency", InstallPlanChange::Update),
            ]
        );

        // An unavailable catalog keeps its recorded skills out of the removal list.
        let plan = install_plan(
            &manifest,
            &catalogs,
            &resolved,
            &previous,
            &BTreeSet::from(["pyg".to_owned()]),
        )
        .unwrap();
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn unavailable_catalog_filter_keeps_declarations_out_of_reconciliation() {
        let manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::from([
                (
                    "offline/root".to_owned(),
                    SkillSelection::Mode(SelectionMode::Enable),
                ),
                (
                    "pyg/root".to_owned(),
                    SkillSelection::Mode(SelectionMode::Enable),
                ),
            ]),
            agents: crate::model::default_agents(),
        };
        let unavailable = BTreeSet::from(["offline".to_owned()]);
        let active = manifest_without_unavailable(&manifest, &unavailable);
        assert!(manifest.skills.contains_key("offline/root"));
        assert!(!active.skills.contains_key("offline/root"));
        assert!(active.skills.contains_key("pyg/root"));
    }

    #[test]
    fn absent_global_selection_is_supported() {
        assert!(GlobalConfig::default().skills.is_empty());
    }

    #[test]
    fn agent_targets_are_configurable_but_cannot_be_empty_or_duplicate() {
        assert!(validate_agents(&["claude-code".to_owned()]).is_ok());
        assert!(validate_agents(&[]).is_err());
        assert!(validate_agents(&["pi".to_owned(), "pi".to_owned()]).is_err());
    }

    #[test]
    fn transaction_journal_is_compact_and_phase_ordered() {
        let journal = TransactionJournal {
            v: 1,
            scope: "g".to_owned(),
            phase: TransactionPhase::V,
            config: "abc".to_owned(),
            desired: vec!["learn-learning".to_owned()],
            remove: vec!["digest-knowledge".to_owned()],
        };
        assert_eq!(
            serde_json::to_string(&journal).unwrap(),
            r#"{"v":1,"scope":"g","phase":"v","config":"abc","desired":["learn-learning"],"remove":["digest-knowledge"]}"#
        );
        assert!(TransactionPhase::I < TransactionPhase::V);
    }

    #[test]
    fn exact_projection_comparison_adopts_only_identical_trees() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/exact-projection");
        let _ = std::fs::remove_dir_all(&base);
        let actual = base.join("actual");
        let intended = base.join("intended");
        std::fs::create_dir_all(actual.join("references")).unwrap();
        std::fs::create_dir_all(intended.join("references")).unwrap();
        std::fs::write(actual.join("SKILL.md"), "same").unwrap();
        std::fs::write(intended.join("SKILL.md"), "same").unwrap();
        std::fs::write(actual.join("references/data.json"), "{}").unwrap();
        std::fs::write(intended.join("references/data.json"), "{}").unwrap();
        assert!(exact_projection_matches(&actual, &intended).unwrap());
        std::fs::write(actual.join("references/data.json"), "{\"drift\":true}").unwrap();
        assert!(!exact_projection_matches(&actual, &intended).unwrap());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unowned_projection_is_adopted_only_when_identical() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/unowned-adoption");
        let _ = std::fs::remove_dir_all(&base);
        let actual = base.join(".agents/skills/root");
        let intended = base.join("prepared/skills/root");
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::create_dir_all(&intended).unwrap();
        std::fs::write(actual.join("SKILL.md"), "same").unwrap();
        std::fs::write(intended.join("SKILL.md"), "same").unwrap();

        let catalog = catalog(false);
        let resolved = vec![ResolvedSkill {
            key: "pyg/root".to_owned(),
            catalog: &catalog,
            source_name: "root".to_owned(),
            installed_name: "root".to_owned(),
            mode: EffectiveMode::Enable,
            gitignore: false,
            digest: "catalog-v1".to_owned(),
        }];
        let adopted = classify_reconciliation(
            &base,
            false,
            &InstalledState::default(),
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(adopted.complete_names.contains("root"));
        assert!(adopted.blocked.is_empty());

        std::fs::write(actual.join("SKILL.md"), "local work").unwrap();
        let blocked = classify_reconciliation(
            &base,
            false,
            &InstalledState::default(),
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(blocked.complete_names.is_empty());
        assert!(blocked.install_names.is_empty());
        assert_eq!(
            blocked.blocked,
            vec![unowned_conflict_message("pyg/root", "root", false)]
        );
        assert_eq!(blocked.conflicts.len(), 1);
        assert_eq!(blocked.conflicts[0].installed_name, "root");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unowned_conflict_message_states_the_consequence_and_both_options() {
        let project = unowned_conflict_message("pyg/note", "note", false);
        assert!(project.contains("pyg/note was not installed"));
        assert!(project.contains("The existing copy was left unchanged"));
        assert!(project.contains("skiller config --set pyg/note=off"));
        assert!(!project.contains(" -g --set"));

        let global = unowned_conflict_message("pyg/note", "note", true);
        assert!(global.contains("skiller config -g --set pyg/note=off"));
    }

    #[test]
    fn automated_installs_never_override_an_unowned_conflict() {
        let mut selection = ReconcileSelection {
            install_names: BTreeSet::new(),
            complete_names: BTreeSet::new(),
            blocked: Vec::new(),
            conflicts: vec![UnownedConflict {
                key: "pyg/note".to_owned(),
                installed_name: "note".to_owned(),
                message: unowned_conflict_message("pyg/note", "note", false),
            }],
        };
        selection
            .blocked
            .push(selection.conflicts[0].message.clone());

        assert!(!install_is_interactive());
        resolve_unowned_overrides(&mut selection).unwrap();
        assert!(selection.install_names.is_empty());
        assert_eq!(selection.blocked.len(), 1);
        assert_eq!(selection.conflicts.len(), 1);
    }

    #[test]
    fn owned_project_drift_is_always_scheduled_for_authoritative_overwrite() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-overrides");
        let _ = std::fs::remove_dir_all(&base);
        let actual = base.join(".agents/skills/root");
        let intended = base.join("prepared/skills/root");
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::create_dir_all(&intended).unwrap();
        std::fs::write(actual.join("SKILL.md"), "canonical").unwrap();
        std::fs::write(intended.join("SKILL.md"), "canonical").unwrap();
        let baseline = directory_digest(&actual).unwrap();
        std::fs::write(actual.join("SKILL.md"), "project override").unwrap();

        let catalog = catalog(false);
        let mut resolved = vec![ResolvedSkill {
            key: "pyg/root".to_owned(),
            catalog: &catalog,
            source_name: "root".to_owned(),
            installed_name: "root".to_owned(),
            mode: EffectiveMode::Enable,
            gitignore: false,
            digest: "catalog-v1".to_owned(),
        }];
        let mut previous = InstalledState {
            version: INSTALLED_STATE_VERSION,
            skills: BTreeMap::from([(
                "pyg/root".to_owned(),
                InstalledSkill {
                    installed_name: "root".to_owned(),
                    mode: EffectiveMode::Enable,
                    gitignore: false,
                    digest: Some("catalog-v1".to_owned()),
                    content_digest: Some(baseline),
                    legacy_path: None,
                },
            )]),
        };
        let drifted = classify_reconciliation(
            &base,
            false,
            &previous,
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(drifted.install_names.contains("root"));
        assert!(drifted.complete_names.is_empty());
        assert!(drifted.blocked.is_empty());

        previous.skills.get_mut("pyg/root").unwrap().digest = Some("catalog-v0".to_owned());
        let catalog_and_project_drift = classify_reconciliation(
            &base,
            false,
            &previous,
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(catalog_and_project_drift.install_names.contains("root"));
        assert!(catalog_and_project_drift.blocked.is_empty());

        previous.skills.get_mut("pyg/root").unwrap().digest = Some("catalog-v1".to_owned());
        resolved[0].digest = "catalog-v2".to_owned();
        std::fs::write(actual.join("SKILL.md"), "canonical").unwrap();
        std::fs::write(intended.join("SKILL.md"), "catalog v2").unwrap();
        let incoming = classify_reconciliation(
            &base,
            false,
            &previous,
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(incoming.install_names.contains("root"));
        assert!(incoming.blocked.is_empty());

        std::fs::write(actual.join("SKILL.md"), "catalog v2").unwrap();
        let resolved_manually = classify_reconciliation(
            &base,
            false,
            &previous,
            &resolved,
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(resolved_manually.install_names.contains("root"));
        assert!(resolved_manually.complete_names.is_empty());
        assert!(resolved_manually.blocked.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn removed_owned_projection_is_not_protected() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/orphaned-project-override");
        let _ = std::fs::remove_dir_all(&base);
        let actual = base.join(".agents/skills/retired");
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::write(actual.join("SKILL.md"), "local").unwrap();
        let previous = InstalledState {
            version: INSTALLED_STATE_VERSION,
            skills: BTreeMap::from([(
                "pyg/retired".to_owned(),
                InstalledSkill {
                    installed_name: "retired".to_owned(),
                    mode: EffectiveMode::Enable,
                    gitignore: false,
                    digest: Some("old".to_owned()),
                    content_digest: Some("different".to_owned()),
                    legacy_path: None,
                },
            )]),
        };
        let selection = classify_reconciliation(
            &base,
            false,
            &previous,
            &[],
            &BTreeSet::new(),
            &["universal".to_owned()],
            &base.join("prepared"),
        )
        .unwrap();
        assert!(selection.complete_names.is_empty());
        assert!(selection.install_names.is_empty());
        assert!(selection.blocked.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_lock_keeps_native_entries_and_excludes_catalog_entries() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-lock-partition");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        init_git(&root);
        let current_source = project_state_root(&root)
            .unwrap()
            .join("prepared-current")
            .display()
            .to_string();
        let lock = serde_json::json!({
            "version": 1,
            "skills": {
                "catalog-skill": {
                    "source": "./.skiller/prepared-current",
                    "sourceType": "local",
                    "computedHash": "managed"
                },
                "catalog-current": {
                    "source": current_source,
                    "sourceType": "local",
                    "computedHash": "managed-current"
                },
                "native-skill": {
                    "source": "owner/native-skills",
                    "sourceType": "github",
                    "computedHash": "native"
                },
                "native-local": {
                    "source": "./project-skills",
                    "sourceType": "local",
                    "computedHash": "local"
                },
                "native-lookalike": {
                    "source": "/unrelated/skiller/prepared-current",
                    "sourceType": "local",
                    "computedHash": "lookalike"
                }
            }
        });
        write_json_atomic(&root.join("skills-lock.json"), &lock).unwrap();
        let owned = BTreeSet::from([
            "catalog-skill".to_owned(),
            "catalog-current".to_owned(),
            "native-local".to_owned(),
            "native-lookalike".to_owned(),
        ]);
        assert_eq!(
            managed_project_lock_names(&root, &owned).unwrap(),
            BTreeSet::from(["catalog-current".to_owned(), "catalog-skill".to_owned()])
        );
        assert!(partition_project_skills_lock(&root, &owned).unwrap());
        let cleaned: serde_json::Value = read_json(&root.join("skills-lock.json")).unwrap();
        assert!(cleaned["skills"].get("catalog-skill").is_none());
        assert!(cleaned["skills"].get("catalog-current").is_none());
        assert_eq!(cleaned["skills"]["native-skill"]["computedHash"], "native");
        assert_eq!(cleaned["skills"]["native-local"]["computedHash"], "local");
        assert_eq!(
            cleaned["skills"]["native-lookalike"]["computedHash"],
            "lookalike"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_vercel_action_partitions_project_lock() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-lock-failed-action");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        init_git(&root);
        let owned = BTreeSet::from(["catalog-skill".to_owned()]);
        let error = run_vercel_partitioning_project_lock(Some(&root), &owned, || {
            write_json_atomic(
                &root.join("skills-lock.json"),
                &serde_json::json!({
                    "version": 1,
                    "skills": {
                        "catalog-skill": {
                            "source": "./.skiller/prepared-current",
                            "sourceType": "local",
                            "computedHash": "managed"
                        },
                        "native-skill": {
                            "source": "owner/native-skills",
                            "sourceType": "github",
                            "computedHash": "native"
                        }
                    }
                }),
            )?;
            bail!("placement failed")
        })
        .unwrap_err();
        assert!(error.to_string().contains("placement failed"));
        let cleaned: serde_json::Value = read_json(&root.join("skills-lock.json")).unwrap();
        assert!(cleaned["skills"].get("catalog-skill").is_none());
        assert_eq!(cleaned["skills"]["native-skill"]["computedHash"], "native");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_lock_disappears_when_it_contains_only_catalog_entries() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-lock-catalog-only");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        init_git(&root);
        write_json_atomic(
            &root.join("skills-lock.json"),
            &serde_json::json!({
                "version": 1,
                "skills": {
                    "catalog-skill": {
                        "source": ".skiller/prepared-current",
                        "sourceType": "local",
                        "computedHash": "managed"
                    }
                }
            }),
        )
        .unwrap();
        assert!(
            partition_project_skills_lock(&root, &BTreeSet::from(["catalog-skill".to_owned()]))
                .unwrap()
        );
        assert!(!root.join("skills-lock.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_lock_rejects_malformed_and_oversized_input() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-lock-invalid");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("skills-lock.json"), b"not JSON").unwrap();
        assert!(managed_project_lock_names(&root, &BTreeSet::new()).is_err());
        std::fs::write(
            root.join("skills-lock.json"),
            vec![b' '; MAX_PROJECT_SKILLS_LOCK_BYTES as usize + 1],
        )
        .unwrap();
        assert!(managed_project_lock_names(&root, &BTreeSet::new()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_lock_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/project-lock-symlink");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("outside.json"), r#"{"version":1,"skills":{}}"#).unwrap();
        symlink(root.join("outside.json"), root.join("skills-lock.json")).unwrap();
        assert!(managed_project_lock_names(&root, &BTreeSet::new()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vercel_install_targets_universal_claude_and_pi_explicitly() {
        let mut command = Command::new("skills");
        append_vercel_install_targets(&mut command, &crate::model::default_agents());
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--agent", "universal", "claude-code", "pi", "--yes"]);
    }
}
