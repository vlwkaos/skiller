use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::catalog::{CatalogIndex, load_global_config, sync_registered_catalogs};
use crate::manual::{apply_manual_mode, rename_skill};
use crate::model::{InstalledSkill, InstalledState, ProjectConfig, SelectionMode, validate_schema};
use crate::paths::{
    cache_root, copy_tree, ensure_real_dir, global_skills_root, global_state_path, read_json,
    read_json_or_default, safe_remove_owned_dir, sanitize_child_output, write_json_atomic,
};

const VERCEL_SKILLS_PACKAGE: &str = "skills@1.5.23";
const VERCEL_INSTALL_AGENTS: &[&str] = &["universal", "claude-code", "pi"];
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
struct ResolvedSkill<'a> {
    key: String,
    catalog: &'a CatalogIndex,
    source_name: String,
    installed_name: String,
    mode: SelectionMode,
    gitignore: bool,
}

struct InstallPaths {
    state_path: PathBuf,
    work_root: PathBuf,
    target_root: PathBuf,
    command_root: PathBuf,
    state_prefix: &'static str,
}

pub fn install(scope: InstallScope, migrate: bool) -> Result<()> {
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
        },
    };
    install_with_catalogs(scope, &manifest, &catalogs, migrate)
}

pub fn install_with_catalogs(
    scope: InstallScope,
    manifest: &ProjectConfig,
    catalogs: &BTreeMap<String, CatalogIndex>,
    migrate: bool,
) -> Result<()> {
    validate_schema(manifest.version, "skill config")?;
    let paths = install_paths(&scope)?;
    let previous: InstalledState = read_json_or_default(&paths.state_path)?;
    validate_schema(previous.version, "installed state")?;
    validate_owned_state(&previous, paths.state_prefix)?;
    let resolved = resolve_manifest(manifest, catalogs, scope.is_global())?;

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
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = safe_remove_owned_dir(&staging_root, &paths.work_root);
        let _ = safe_remove_owned_dir(&prepared_root, &paths.work_root);
        return Err(error);
    }

    let result = (|| -> Result<()> {
        prepare_skills(&resolved, &staging_root, &prepared_root)?;
        if migrate {
            unlink_legacy_skill_roots(&paths.command_root, scope.is_global())?;
        } else {
            refuse_unowned_conflicts(&paths.command_root, scope.is_global(), &previous, &resolved)?;
        }
        run_vercel_install(
            &paths.command_root,
            &prepared_root,
            &resolved,
            scope.is_global(),
        )?;
        verify_installation(&paths.target_root, &resolved)?;

        let desired_names: BTreeSet<_> = resolved
            .iter()
            .map(|skill| skill.installed_name.as_str())
            .collect();
        let removed: Vec<_> = previous
            .skills
            .values()
            .filter(|skill| !desired_names.contains(skill.installed_name.as_str()))
            .map(|skill| skill.installed_name.clone())
            .collect();
        run_vercel_remove(&paths.command_root, &removed, scope.is_global())?;

        let next = InstalledState {
            version: crate::model::SCHEMA_VERSION,
            skills: resolved
                .iter()
                .map(|skill| {
                    (
                        skill.key.clone(),
                        InstalledSkill {
                            catalog: skill.catalog.alias.clone(),
                            source_skill: skill.source_name.clone(),
                            installed_name: skill.installed_name.clone(),
                            path: format!("{}/{}", paths.state_prefix, skill.installed_name),
                            mode: skill.mode,
                            gitignore: skill.gitignore,
                        },
                    )
                })
                .collect(),
        };
        write_json_atomic(&paths.state_path, &next)?;
        if let InstallScope::Project(project_root) = &scope {
            update_gitignore(project_root, &next)?;
        }
        Ok(())
    })();

    let staging_cleanup = safe_remove_owned_dir(&staging_root, &paths.work_root);
    let prepared_cleanup = safe_remove_owned_dir(&prepared_root, &paths.work_root);
    result?;
    staging_cleanup?;
    prepared_cleanup?;

    let manual_count = resolved
        .iter()
        .filter(|skill| skill.mode == SelectionMode::Manual)
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
    Ok(())
}

fn install_paths(scope: &InstallScope) -> Result<InstallPaths> {
    match scope {
        InstallScope::Project(project_root) => Ok(InstallPaths {
            state_path: project_root.join(".skiller/installed.json"),
            work_root: project_root.join(".skiller"),
            target_root: project_root.join(".agents/skills"),
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
                work_root: cache_root()?.join("install"),
                target_root: global_skills_root()?,
                command_root: home,
                state_prefix: ".agents/skills",
            })
        }
    }
}

fn resolve_manifest<'a>(
    manifest: &ProjectConfig,
    catalogs: &'a BTreeMap<String, CatalogIndex>,
    global_scope: bool,
) -> Result<Vec<ResolvedSkill<'a>>> {
    let mut selected = BTreeMap::<String, (SelectionMode, bool, bool)>::new();
    for (key, selection) in &manifest.skills {
        selected.insert(key.clone(), (selection.mode(), selection.gitignore(), true));
    }

    let roots: Vec<_> = selected.keys().cloned().collect();
    for key in roots {
        add_dependency_closure(&key, catalogs, global_scope, &mut selected)?;
    }

    let mut installed_names = BTreeMap::<String, String>::new();
    let mut resolved = Vec::new();
    for (key, (mode, gitignore, explicit)) in selected {
        let (alias, source_name) = split_key(&key)?;
        let source_name = source_name.to_owned();
        let catalog = catalogs
            .get(alias)
            .with_context(|| format!("configuration references unregistered catalog: {alias}"))?;
        let skill = catalog
            .skills
            .get(&source_name)
            .with_context(|| format!("catalog {alias} has no skill named {source_name}"))?;
        if explicit && skill.global != global_scope {
            bail!(
                "{} skill {key} cannot be selected in {} configuration",
                if skill.global { "global" } else { "project" },
                if global_scope { "global" } else { "project" }
            );
        }
        if global_scope && gitignore {
            bail!("global skill {key} cannot use project Git ignore state");
        }
        let installed_name = if global_scope {
            skill.name.clone()
        } else {
            skill.installed_name.clone()
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
    selected: &mut BTreeMap<String, (SelectionMode, bool, bool)>,
) -> Result<()> {
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
        if !selected.contains_key(&dependency_key) {
            selected.insert(
                dependency_key.clone(),
                (SelectionMode::Manual, false, false),
            );
            add_dependency_closure(&dependency_key, catalogs, global_scope, selected)?;
        }
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
            rename_skill(&destination, &skill.installed_name)?;
            if skill.mode == SelectionMode::Manual {
                apply_manual_mode(&destination)?;
            }
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
    // ^ skills@1.5.23 writes the universal canonical skill and explicit Claude Code/Pi projections.
    append_vercel_install_targets(&mut command);
    if global_scope {
        command.arg("--global");
    }
    command.current_dir(command_root);
    run_command(command, "installing prepared skills")
}

fn append_vercel_install_targets(command: &mut Command) {
    command
        .arg("--agent")
        .args(VERCEL_INSTALL_AGENTS)
        .arg("--yes");
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

fn projection_roots(command_root: &Path, global_scope: bool) -> Vec<PathBuf> {
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

fn unlink_legacy_skill_roots(command_root: &Path, global_scope: bool) -> Result<()> {
    for path in projection_roots(command_root, global_scope) {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                std::fs::remove_file(&path)
                    .with_context(|| format!("unlinking legacy skill root {}", path.display()))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()));
            }
        }
    }
    Ok(())
}

fn refuse_unowned_conflicts(
    command_root: &Path,
    global_scope: bool,
    previous: &InstalledState,
    resolved: &[ResolvedSkill<'_>],
) -> Result<()> {
    let owned: BTreeSet<_> = previous
        .skills
        .values()
        .map(|skill| skill.installed_name.as_str())
        .collect();
    let roots = projection_roots(command_root, global_scope);
    for skill in resolved {
        let conflict = roots
            .iter()
            .map(|root| root.join(&skill.installed_name))
            .any(|path| path.exists() || path.is_symlink());
        if conflict && !owned.contains(skill.installed_name.as_str()) {
            bail!(
                "refusing to replace a skill not owned by Skiller: {}",
                skill.installed_name
            );
        }
    }
    Ok(())
}

fn verify_installation(target_root: &Path, resolved: &[ResolvedSkill<'_>]) -> Result<()> {
    for skill in resolved {
        let path = target_root.join(&skill.installed_name).join("SKILL.md");
        if !path.is_file() {
            bail!("Vercel Skills did not install {}", path.display());
        }
    }
    Ok(())
}

fn validate_owned_state(state: &InstalledState, prefix: &str) -> Result<()> {
    for skill in state.skills.values() {
        let expected = format!("{prefix}/{}", skill.installed_name);
        if skill.path != expected || !crate::model::valid_name(&skill.installed_name) {
            bail!(
                "installed state contains an unsafe owned path: {}",
                skill.path
            );
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
    use crate::model::{CatalogMetadata, GlobalConfig, SCHEMA_VERSION, SkillSelection};

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
                        installed_name: "root-engineering".to_owned(),
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
                        installed_name: "dependency-engineering".to_owned(),
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
        };
        let catalogs = BTreeMap::from([("pyg".to_owned(), catalog(true))]);
        let resolved = resolve_manifest(&manifest, &catalogs, true).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].installed_name, "dependency");
        assert_eq!(resolved[0].mode, SelectionMode::Manual);
        assert!(resolve_manifest(&manifest, &catalogs, false).is_err());
    }

    #[test]
    fn absent_global_selection_is_supported() {
        assert!(GlobalConfig::default().skills.is_empty());
    }

    #[test]
    fn vercel_install_targets_universal_claude_and_pi_explicitly() {
        let mut command = Command::new("skills");
        append_vercel_install_targets(&mut command);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--agent", "universal", "claude-code", "pi", "--yes"]);
    }
}
