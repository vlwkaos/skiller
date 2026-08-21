use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::catalog::{
    CatalogEligibility, add_skill, load_global_config, scan_catalog, source_skill_name,
    sync_registered_catalogs,
};
use crate::installer::{InstallScope, cleanup_legacy_names, install_migration, validate_agents};
use crate::manual::set_skill_name;
use crate::model::{
    CatalogMetadata, CatalogRegistration, ProjectConfig, SelectionMode, SkillSelection,
    validate_alias, validate_schema,
};
use crate::paths::{
    copy_tree, ensure_real_dir, global_config_path, read_json, read_json_or_default,
    safe_remove_owned_dir, write_global_config, write_json_atomic,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MigrationPlan {
    pub version: u32,
    pub catalog: MigrationCatalog,
    pub target: MigrationTarget,
    pub agents: Vec<String>,
    pub skills: Vec<MigrationSkill>,
    #[serde(default)]
    pub install: bool,
    #[serde(default)]
    pub cleanup_legacy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MigrationCatalog {
    pub alias: String,
    pub root: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MigrationTarget {
    Global,
    Project { root: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MigrationSkill {
    pub source: PathBuf,
    pub source_name: String,
    pub scope: String,
    pub mode: SelectionMode,
    #[serde(default)]
    pub gitignore: bool,
    pub legacy_installed_name: String,
}

pub fn initialize(path: &Path) -> Result<()> {
    if path.exists() || path.is_symlink() {
        bail!(
            "refusing to replace existing migration plan: {}",
            path.display()
        );
    }
    let plan = MigrationPlan {
        version: 1,
        catalog: MigrationCatalog {
            alias: "pyg".to_owned(),
            root: PathBuf::from("/path/to/writable/catalog"),
            source: "owner/repository".to_owned(),
        },
        target: MigrationTarget::Global,
        agents: crate::model::default_agents(),
        skills: vec![MigrationSkill {
            source: PathBuf::from("/path/to/legacy/skill"),
            source_name: "skill".to_owned(),
            scope: "engineering".to_owned(),
            mode: SelectionMode::Enable,
            gitignore: false,
            legacy_installed_name: "legacy-skill".to_owned(),
        }],
        install: true,
        cleanup_legacy: false,
    };
    write_json_atomic(path, &plan)?;
    println!("created {}", path.display());
    println!("next: skiller migrate --plan {} --check", path.display());
    Ok(())
}

pub fn run_plan(path: &Path, apply: bool, yes: bool) -> Result<()> {
    let plan: MigrationPlan = read_json(path)?;
    validate_plan(&plan)?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    if !apply {
        println!(
            "migration plan is valid; apply with `skiller migrate --plan {} --apply`",
            path.display()
        );
        return Ok(());
    }
    if !yes && !confirm("Apply this migration plan? [y/N]: ")? {
        println!("migration cancelled");
        return Ok(());
    }
    apply_plan(&plan)
}

pub fn interactive() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive migration requires a terminal; use --init or --plan for automation");
    }
    println!("Skiller migration · catalog → configuration → installation");
    println!("Nothing is committed or pushed. Legacy cleanup is separately approved.");
    let alias = prompt_default("Catalog alias", "pyg")?;
    let root = PathBuf::from(prompt_required("Writable catalog checkout")?);
    let source = prompt_required("Portable catalog source (owner/repository or Git URL)")?;
    let global = prompt_default("Target global or project", "global")? == "global";
    let target = if global {
        MigrationTarget::Global
    } else {
        MigrationTarget::Project {
            root: PathBuf::from(prompt_default(
                "Project root",
                &std::env::current_dir()?.display().to_string(),
            )?),
        }
    };
    let candidates = discover_legacy_candidates(&target)?;
    if !candidates.is_empty() {
        println!("Discovered legacy skills:");
        for (index, candidate) in candidates.iter().enumerate() {
            println!("  {}. {}", index + 1, candidate.display());
        }
    }
    let source_values =
        prompt_required("Select numbers or enter skill directories (comma-separated)")?;
    let selected_sources = parse_source_selection(&source_values, &candidates)?;
    let mut skills = Vec::new();
    for skill_source in selected_sources {
        let legacy_name = source_skill_name(&skill_source)?;
        let source_name = prompt_default("Clean catalog source name", &legacy_name)?;
        let scope = prompt_required(&format!("Scope for {source_name}"))?;
        let mode =
            match prompt_default(&format!("Mode for {source_name} (enable/manual)"), "enable")?
                .as_str()
            {
                "enable" => SelectionMode::Enable,
                "manual" => SelectionMode::Manual,
                other => bail!("invalid mode: {other}"),
            };
        let gitignore = !global
            && prompt_default(&format!("Git-ignore {source_name}? (yes/no)"), "no")? == "yes";
        let legacy_installed_name = prompt_default("Legacy installed name", &legacy_name)?;
        skills.push(MigrationSkill {
            source: skill_source,
            source_name,
            scope,
            mode,
            gitignore,
            legacy_installed_name,
        });
    }
    let agents = prompt_default(
        "Vercel agents (comma-separated)",
        "universal,claude-code,pi",
    )?
    .split(',')
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_owned)
    .collect();
    let install = prompt_default("Install after catalog/config update? (yes/no)", "yes")? == "yes";
    let cleanup_legacy = prompt_default(
        "Remove selected legacy names after verified install? (yes/no)",
        "no",
    )? == "yes";
    let plan = MigrationPlan {
        version: 1,
        catalog: MigrationCatalog {
            alias,
            root,
            source,
        },
        target,
        agents,
        skills,
        install,
        cleanup_legacy,
    };
    validate_plan(&plan)?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    if confirm("Apply this migration? [y/N]: ")? {
        apply_plan(&plan)
    } else {
        println!("migration cancelled");
        Ok(())
    }
}

fn discover_legacy_candidates(target: &MigrationTarget) -> Result<Vec<PathBuf>> {
    let root = match target {
        MigrationTarget::Global => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not configured")?)
                .join(".agents/skills")
        }
        MigrationTarget::Project { root } => root.join(".agents/skills"),
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_dir() && !metadata.is_symlink() && entry.path().join("SKILL.md").is_file() {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn parse_source_selection(value: &str, candidates: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut selected = Vec::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let path = if let Ok(index) = item.parse::<usize>() {
            candidates
                .get(
                    index
                        .checked_sub(1)
                        .context("selection numbers start at 1")?,
                )
                .with_context(|| format!("legacy selection is out of range: {index}"))?
                .clone()
        } else {
            PathBuf::from(item)
        };
        if !selected.contains(&path) {
            selected.push(path);
        }
    }
    if selected.is_empty() {
        bail!("select at least one legacy skill");
    }
    Ok(selected)
}

fn validate_plan(plan: &MigrationPlan) -> Result<()> {
    if plan.version != 1 {
        bail!(
            "unsupported migration plan version {}; expected 1",
            plan.version
        );
    }
    validate_alias(&plan.catalog.alias)?;
    validate_agents(&plan.agents)?;
    if plan.catalog.source.trim().is_empty() {
        bail!("catalog source cannot be empty");
    }
    let metadata = std::fs::symlink_metadata(&plan.catalog.root)
        .with_context(|| format!("inspecting catalog root {}", plan.catalog.root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "catalog root must be a real directory: {}",
            plan.catalog.root.display()
        );
    }
    let catalog_metadata: CatalogMetadata = read_json(&plan.catalog.root.join("skiller.json"))?;
    validate_schema(catalog_metadata.version, "catalog metadata")?;
    if plan.skills.is_empty() {
        bail!("migration plan must include at least one skill");
    }
    let global = matches!(plan.target, MigrationTarget::Global);
    let mut names = BTreeSet::new();
    let mut legacy = BTreeSet::new();
    for skill in &plan.skills {
        let legacy_source_name = source_skill_name(&skill.source)?;
        if !crate::model::valid_name(&skill.source_name) || !names.insert(skill.source_name.clone())
        {
            bail!(
                "invalid or duplicate migration source name: {}",
                skill.source_name
            );
        }
        if !catalog_metadata.scopes.contains_key(&skill.scope) {
            bail!("catalog has no scope named {}", skill.scope);
        }
        if global && skill.gitignore {
            bail!(
                "global migration skill {} cannot use Git-ignore state",
                skill.source_name
            );
        }
        let source_metadata = std::fs::symlink_metadata(&skill.source)?;
        let source_leaf = skill
            .source
            .file_name()
            .and_then(|value| value.to_str())
            .context("legacy source has no UTF-8 directory name")?;
        if source_metadata.file_type().is_symlink()
            || !source_metadata.is_dir()
            || source_leaf != skill.legacy_installed_name
            || legacy_source_name != skill.legacy_installed_name
            || !legacy.insert(skill.legacy_installed_name.clone())
        {
            bail!(
                "legacy name {} is not proven by selected source {}",
                skill.legacy_installed_name,
                skill.source.display()
            );
        }
    }
    if plan.cleanup_legacy && !plan.install {
        bail!("legacy cleanup requires verified installation");
    }
    Ok(())
}

fn apply_plan(plan: &MigrationPlan) -> Result<()> {
    let global = matches!(plan.target, MigrationTarget::Global);
    let mut global_config = load_global_config()?;
    let previous_global = global_config.clone();
    match global_config.catalogs.get(&plan.catalog.alias) {
        Some(registration) if registration.source != plan.catalog.source => bail!(
            "catalog alias {} is already registered to {}",
            plan.catalog.alias,
            registration.source
        ),
        Some(_) => {}
        None => {
            global_config.catalogs.insert(
                plan.catalog.alias.clone(),
                CatalogRegistration {
                    source: plan.catalog.source.clone(),
                    r#ref: None,
                    authoring_root: Some(plan.catalog.root.display().to_string()),
                },
            );
        }
    }
    let mut install_catalogs = if plan.install {
        let mut sync_config = previous_global.clone();
        sync_config.catalogs.remove(&plan.catalog.alias);
        Some(sync_registered_catalogs(&sync_config)?)
    } else {
        None
    };
    let (scope, config_path, mut manifest) = match &plan.target {
        MigrationTarget::Global => (
            InstallScope::Global,
            global_config_path()?,
            ProjectConfig {
                version: global_config.version,
                skills: global_config.skills.clone(),
                agents: plan.agents.clone(),
            },
        ),
        MigrationTarget::Project { root } => {
            let root = root
                .canonicalize()
                .with_context(|| format!("resolving {}", root.display()))?;
            let path = root.join("skiller.config.json");
            let manifest = read_json_or_default(&path)?;
            (InstallScope::Project(root), path, manifest)
        }
    };

    let metadata_path = plan.catalog.root.join("skiller.json");
    let original_metadata: CatalogMetadata = read_json(&metadata_path)?;
    let (prepared_root, prepared_sources) = prepare_sources(plan)?;
    let mut added_names = Vec::new();
    let catalog_result = (|| -> Result<()> {
        for (skill, prepared_source) in plan.skills.iter().zip(&prepared_sources) {
            let before: CatalogMetadata = read_json(&metadata_path)?;
            add_skill(
                &plan.catalog.root,
                prepared_source,
                &skill.scope,
                if global {
                    CatalogEligibility::Global
                } else {
                    CatalogEligibility::Project
                },
            )?;
            let after: CatalogMetadata = read_json(&metadata_path)?;
            let added = after
                .skills
                .keys()
                .find(|name| !before.skills.contains_key(*name))
                .context("catalog migration did not add one skill")?
                .clone();
            added_names.push(added);
        }
        Ok(())
    })();
    let prepared_cleanup = safe_remove_owned_dir(
        &prepared_root,
        prepared_root
            .parent()
            .context("prepared migration root has no parent")?,
    );
    if let Err(error) = catalog_result {
        rollback_catalog(
            &plan.catalog.root,
            &metadata_path,
            &original_metadata,
            &added_names,
        )?;
        prepared_cleanup?;
        return Err(error);
    }
    prepared_cleanup?;
    if let Some(catalogs) = &mut install_catalogs {
        match scan_catalog(
            &plan.catalog.alias,
            &plan.catalog.source,
            &plan.catalog.root,
        ) {
            Ok(catalog) => {
                catalogs.insert(plan.catalog.alias.clone(), catalog);
            }
            Err(error) => {
                rollback_catalog(
                    &plan.catalog.root,
                    &metadata_path,
                    &original_metadata,
                    &added_names,
                )?;
                return Err(error);
            }
        }
    }

    manifest.agents = plan.agents.clone();
    for (skill, name) in plan.skills.iter().zip(&added_names) {
        manifest.skills.insert(
            format!("{}/{}", plan.catalog.alias, name),
            SkillSelection::from_parts(skill.mode, skill.gitignore),
        );
    }
    let config_result = match &scope {
        InstallScope::Global => {
            global_config.skills = manifest.skills.clone();
            global_config.agents = manifest.agents.clone();
            write_global_config(&global_config)
        }
        InstallScope::Project(_) => write_global_config(&global_config)
            .and_then(|()| write_json_atomic(&config_path, &manifest)),
    };
    if let Err(error) = config_result {
        let _ = write_global_config(&previous_global);
        rollback_catalog(
            &plan.catalog.root,
            &metadata_path,
            &original_metadata,
            &added_names,
        )?;
        return Err(error);
    }

    println!("catalog and configuration migration committed locally");
    if let Some(catalogs) = install_catalogs {
        let legacy: BTreeSet<_> = plan
            .skills
            .iter()
            .map(|skill| skill.legacy_installed_name.clone())
            .collect();
        install_migration(scope.clone(), &manifest, &catalogs, &legacy)?;
        if plan.cleanup_legacy {
            cleanup_legacy_names(&scope, &legacy)?;
        }
    }
    println!("next: inspect and publish {}", plan.catalog.root.display());
    println!(
        "then run `skiller doctor{}`",
        if global { " -g" } else { "" }
    );
    Ok(())
}

fn prepare_sources(plan: &MigrationPlan) -> Result<(PathBuf, Vec<PathBuf>)> {
    let parent = plan
        .catalog
        .root
        .parent()
        .context("catalog root has no parent")?;
    let prepared_root = parent.join(format!(".skiller-migrate-{}", std::process::id()));
    if std::fs::symlink_metadata(&prepared_root).is_ok() {
        bail!(
            "migration preparation path already exists: {}",
            prepared_root.display()
        );
    }
    ensure_real_dir(&prepared_root)?;
    let result = (|| -> Result<Vec<PathBuf>> {
        let mut sources = Vec::new();
        for skill in &plan.skills {
            let destination = prepared_root.join(&skill.source_name);
            copy_tree(&skill.source, &destination)?;
            set_skill_name(&destination, &skill.source_name)?;
            sources.push(destination);
        }
        Ok(sources)
    })();
    match result {
        Ok(sources) => Ok((prepared_root, sources)),
        Err(error) => {
            let cleanup = safe_remove_owned_dir(&prepared_root, parent);
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("migration preparation cleanup failed: {cleanup:#}")))
                }
            }
        }
    }
}

fn rollback_catalog(
    root: &Path,
    metadata_path: &Path,
    metadata: &CatalogMetadata,
    added_names: &[String],
) -> Result<()> {
    for name in added_names {
        safe_remove_owned_dir(&root.join("skills").join(name), &root.join("skills"))?;
    }
    write_json_atomic(metadata_path, metadata)
}

fn confirm(message: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("noninteractive migration apply requires --yes");
    }
    Ok(matches!(
        prompt_required(message)?.to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn prompt_required(message: &str) -> Result<String> {
    print!("{message}: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("a value is required for {message}");
    }
    Ok(value)
}

fn prompt_default(message: &str, default: &str) -> Result<String> {
    print!("{message} [{default}]: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_requires_nonempty_agents_and_cleanup_requires_install() {
        let mut plan = MigrationPlan {
            version: 1,
            catalog: MigrationCatalog {
                alias: "pyg".to_owned(),
                root: PathBuf::from("missing"),
                source: "owner/repo".to_owned(),
            },
            target: MigrationTarget::Global,
            agents: Vec::new(),
            skills: Vec::new(),
            install: false,
            cleanup_legacy: true,
        };
        assert!(validate_plan(&plan).is_err());
        plan.agents = vec!["pi".to_owned()];
        assert!(validate_plan(&plan).is_err());
    }
}
