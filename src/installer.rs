use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogIndex, load_global_config, sync_registered_catalogs};
use crate::manual::{apply_invocation_mode, apply_projected_identity};
use crate::model::{
    EffectiveMode, INSTALLED_STATE_VERSION, InstalledSkill, InstalledState, ProjectConfig,
    SelectionMode, validate_installed_state, validate_schema,
};
use crate::paths::{
    cache_root, copy_tree, ensure_real_dir, global_skills_root, global_state_path, read_json,
    read_json_or_default, safe_remove_owned_dir, sanitize_child_output, validate_managed_json_path,
    write_json_atomic_compact,
};

const VERCEL_SKILLS_PACKAGE: &str = "skills@1.5.23";
const IGNORE_START: &str = "# skiller:start";
const IGNORE_END: &str = "# skiller:end";

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

#[derive(Debug)]
pub(crate) struct ResolvedSkill<'a> {
    pub(crate) key: String,
    catalog: &'a CatalogIndex,
    source_name: String,
    pub(crate) installed_name: String,
    pub(crate) mode: EffectiveMode,
    pub(crate) gitignore: bool,
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

pub fn install(scope: InstallScope) -> Result<()> {
    let global_config = load_global_config()?;
    let catalogs = sync_registered_catalogs(&global_config)?;
    let manifest = match &scope {
        InstallScope::Project(project_root) => {
            let path = project_root.join("skiller.config.json");
            read_json(&path).with_context(|| "run `skiller config` before installing")?
        }
        InstallScope::Global => ProjectConfig {
            version: global_config.version,
            skills: global_config.skills,
            agents: global_config.agents,
        },
    };
    install_with_catalogs(scope, &manifest, &catalogs)
}

pub fn install_with_catalogs(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
) -> Result<()> {
    install_with_catalogs_recovery(
        scope,
        manifest,
        catalogs,
        &BTreeSet::new(),
        &BTreeSet::new(),
        false,
    )
}

pub(crate) fn install_migration(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    legacy_names: &BTreeSet<String>,
) -> Result<()> {
    install_with_catalogs_recovery(
        scope,
        manifest,
        catalogs,
        legacy_names,
        &BTreeSet::new(),
        false,
    )
}

pub(crate) fn install_with_catalogs_recovery(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    replacement_owned: &BTreeSet<String>,
    cleanup_owned: &BTreeSet<String>,
    recovering: bool,
) -> Result<()> {
    validate_schema(manifest.version, "skill config")?;
    validate_agents(&manifest.agents)?;
    let paths = install_paths(&scope)?;
    validate_managed_json_path(&paths.state_path)?;
    validate_managed_json_path(&paths.transaction_path)?;
    if paths.transaction_path.exists() && !recovering {
        bail!(
            "an unfinished Skiller transaction exists at {}; run `skiller doctor{} --repair`",
            paths.transaction_path.display(),
            if scope.is_global() { " -g" } else { "" }
        );
    }
    let previous: InstalledState = read_json_or_default(&paths.state_path)?;
    validate_installed_state(previous.version)?;
    validate_owned_state(&previous, paths.state_prefix)?;
    let resolved = resolve_manifest(manifest, catalogs, scope.is_global())?;

    ensure_real_dir(
        paths
            .work_root
            .parent()
            .context("managed work root has no parent")?,
    )?;
    ensure_real_dir(&paths.work_root)?;
    let staging_root = paths
        .work_root
        .join(format!("staging-{}", std::process::id()));
    let prepared_root = paths
        .work_root
        .join(format!("prepared-{}", std::process::id()));
    let setup = (|| -> Result<()> {
        safe_remove_owned_dir(&staging_root, &paths.work_root)?;
        safe_remove_owned_dir(&prepared_root, &paths.work_root)?;
        ensure_real_dir(&staging_root)?;
        ensure_real_dir(&prepared_root.join("skills"))?;
        write_work_marker(&staging_root)?;
        write_work_marker(&prepared_root)?;
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = safe_remove_owned_dir(&staging_root, &paths.work_root);
        let _ = safe_remove_owned_dir(&prepared_root, &paths.work_root);
        return Err(error);
    }

    let result = (|| -> Result<()> {
        prepare_skills(&resolved, &staging_root, &prepared_root)?;
        refuse_unowned_conflicts(
            &paths.command_root,
            scope.is_global(),
            &previous,
            &resolved,
            replacement_owned,
            &manifest.agents,
        )?;
        let desired_names: BTreeSet<_> = resolved
            .iter()
            .map(|skill| skill.installed_name.clone())
            .collect();
        let mut removed: BTreeSet<_> = previous
            .skills
            .values()
            .filter(|skill| !desired_names.contains(&skill.installed_name))
            .map(|skill| skill.installed_name.clone())
            .collect();
        removed.extend(
            cleanup_owned
                .iter()
                .filter(|name| !desired_names.contains(*name))
                .cloned(),
        );
        let mut journal = TransactionJournal {
            v: 1,
            scope: if scope.is_global() { "g" } else { "p" }.to_owned(),
            phase: TransactionPhase::P,
            config: manifest_fingerprint(manifest)?,
            desired: desired_names.iter().cloned().collect(),
            remove: removed.iter().cloned().collect(),
        };
        write_json_atomic_compact(&paths.transaction_path, &journal)?;
        run_vercel_install(
            &paths.command_root,
            &prepared_root,
            &resolved,
            &manifest.agents,
            scope.is_global(),
        )?;
        journal.phase = TransactionPhase::I;
        write_json_atomic_compact(&paths.transaction_path, &journal)?;
        verify_installation(
            &paths.command_root,
            scope.is_global(),
            &resolved,
            &manifest.agents,
        )?;
        journal.phase = TransactionPhase::V;
        write_json_atomic_compact(&paths.transaction_path, &journal)?;
        run_vercel_remove(
            &paths.command_root,
            &removed.into_iter().collect::<Vec<_>>(),
            scope.is_global(),
        )?;
        journal.phase = TransactionPhase::C;
        write_json_atomic_compact(&paths.transaction_path, &journal)?;

        let next = InstalledState {
            version: INSTALLED_STATE_VERSION,
            skills: resolved
                .iter()
                .map(|skill| {
                    (
                        skill.key.clone(),
                        InstalledSkill {
                            installed_name: skill.installed_name.clone(),
                            mode: skill.mode,
                            gitignore: skill.gitignore,
                            legacy_path: None,
                        },
                    )
                })
                .collect(),
        };
        write_json_atomic_compact(&paths.state_path, &next)?;
        if let InstallScope::Project(project_root) = &scope {
            update_gitignore(project_root, &next)?;
        }
        remove_transaction(&paths.transaction_path)?;
        Ok(())
    })();

    let staging_cleanup = safe_remove_owned_dir(&staging_root, &paths.work_root);
    let prepared_cleanup = safe_remove_owned_dir(&prepared_root, &paths.work_root);
    result?;
    staging_cleanup?;
    prepared_cleanup?;

    let manual_count = resolved
        .iter()
        .filter(|skill| skill.mode == EffectiveMode::Manual)
        .count();
    let dependency_count = resolved
        .iter()
        .filter(|skill| skill.mode == EffectiveMode::Dependency)
        .count();
    println!(
        "installed {} managed {} skill{} through Vercel Skills",
        resolved.len(),
        if scope.is_global() {
            "global"
        } else {
            "project"
        },
        if resolved.len() == 1 { "" } else { "s" }
    );
    if manual_count > 0 {
        println!(
            "warning: manual mode is enforced by Pi, Claude Code, Cursor, and Codex; OpenCode and Gemini CLI may still expose these skills to the model"
        );
    }
    if dependency_count > 0 {
        println!(
            "warning: dependency-only user hiding is enforced by Claude Code and Pygmalion; other agents may expose exact invocation"
        );
    }
    Ok(())
}

pub(crate) fn install_paths(scope: &InstallScope) -> Result<InstallPaths> {
    match scope {
        InstallScope::Project(project_root) => Ok(InstallPaths {
            state_path: project_root.join(".skiller/installed.json"),
            transaction_path: project_root.join(".skiller/transaction.json"),
            work_root: project_root.join(".skiller"),
            command_root: project_root.clone(),
            state_prefix: ".agents/skills",
        }),
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
        resolved.push(ResolvedSkill {
            key,
            catalog,
            source_name,
            installed_name,
            mode,
            gitignore,
        });
    }
    Ok(resolved)
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

fn prepare_skills(
    resolved: &[ResolvedSkill<'_>],
    staging_root: &Path,
    prepared_root: &Path,
) -> Result<()> {
    let mut by_catalog = BTreeMap::<String, Vec<&ResolvedSkill<'_>>>::new();
    for skill in resolved {
        by_catalog
            .entry(skill.catalog.alias.clone())
            .or_default()
            .push(skill);
    }
    for (alias, skills) in by_catalog {
        let catalog = skills[0].catalog;
        let stage = staging_root.join(&alias);
        ensure_real_dir(&stage)?;
        run_vercel_stage(&stage, catalog, &skills)?;
        for skill in skills {
            let source = stage.join(".agents/skills").join(&skill.source_name);
            if !source.join("SKILL.md").is_file() {
                bail!(
                    "Vercel Skills did not stage expected skill {} from {}",
                    skill.source_name,
                    catalog.source
                );
            }
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
    }
    Ok(())
}

fn run_vercel_stage(
    stage: &Path,
    catalog: &CatalogIndex,
    skills: &[&ResolvedSkill<'_>],
) -> Result<()> {
    let mut command = vercel_command();
    command.arg("add").arg(&catalog.root);
    for skill in skills {
        command.args(["--skill", &skill.source_name]);
    }
    command
        .args(["--agent", "universal", "--copy", "--yes"])
        .current_dir(stage);
    run_command(command, &format!("staging catalog {}", catalog.alias))
}

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

pub(crate) fn cleanup_legacy_names(scope: &InstallScope, names: &BTreeSet<String>) -> Result<()> {
    let paths = install_paths(scope)?;
    run_vercel_remove(
        &paths.command_root,
        &names.iter().cloned().collect::<Vec<_>>(),
        scope.is_global(),
    )
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

fn run_command(mut command: Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("starting npx {VERCEL_SKILLS_PACKAGE} while {action}"))?;
    if !output.status.success() {
        bail!(
            "Vercel Skills failed while {action}: {}{}",
            sanitize_child_output(&output.stdout),
            sanitize_child_output(&output.stderr)
        );
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

fn refuse_unowned_conflicts(
    command_root: &Path,
    global_scope: bool,
    previous: &InstalledState,
    resolved: &[ResolvedSkill<'_>],
    recovery_owned: &BTreeSet<String>,
    agents: &[String],
) -> Result<()> {
    let mut owned: BTreeSet<_> = previous
        .skills
        .values()
        .map(|skill| skill.installed_name.as_str())
        .collect();
    owned.extend(recovery_owned.iter().map(String::as_str));
    let roots = projection_roots(command_root, global_scope);
    let agent_names: Vec<_> = agents
        .iter()
        .map(|agent| list_agent_skill_names(command_root, global_scope, agent))
        .collect::<Result<_>>()?;
    for skill in resolved {
        let conflict = roots
            .iter()
            .map(|root| root.join(&skill.installed_name))
            .any(|path| path.exists() || path.is_symlink())
            || agent_names
                .iter()
                .any(|names| names.contains(&skill.installed_name));
        if conflict && !owned.contains(skill.installed_name.as_str()) {
            bail!(
                "refusing to replace a skill not owned by Skiller: {}",
                skill.installed_name
            );
        }
    }
    Ok(())
}

fn verify_installation(
    command_root: &Path,
    global_scope: bool,
    resolved: &[ResolvedSkill<'_>],
    agents: &[String],
) -> Result<()> {
    for agent in agents {
        let installed = list_agent_skill_names(command_root, global_scope, agent)?;
        for skill in resolved {
            if !installed.contains(&skill.installed_name) {
                bail!(
                    "Vercel Skills did not install {} for agent {agent}",
                    skill.installed_name
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn list_agent_skill_names(
    command_root: &Path,
    global_scope: bool,
    agent: &str,
) -> Result<BTreeSet<String>> {
    let mut command = vercel_command();
    command.args(["list", "--json", "--agent", agent]);
    if global_scope {
        command.arg("--global");
    }
    command.current_dir(command_root);
    let output = command
        .output()
        .with_context(|| format!("listing Vercel Skills for agent {agent}"))?;
    if !output.status.success() {
        bail!(
            "Vercel Skills rejected agent {agent}: {}{}",
            sanitize_child_output(&output.stdout),
            sanitize_child_output(&output.stderr)
        );
    }
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
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
    kept.push("/.skiller/".to_owned());
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
                        scope: Some("engineering".to_owned()),
                        installed_name: "root".to_owned(),
                        global,
                        requires: vec!["dependency".to_owned()],
                    },
                ),
                (
                    "dependency".to_owned(),
                    CatalogSkill {
                        name: "dependency".to_owned(),
                        description: "Dependency".to_owned(),
                        scope: Some("engineering".to_owned()),
                        installed_name: "dependency".to_owned(),
                        global,
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
