use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::model::{
    CatalogMetadata, CatalogRegistration, GlobalConfig, valid_name, validate_alias, validate_schema,
};
use crate::paths::{
    cache_root, global_config_path, read_json, read_json_or_default, safe_remove_owned_dir,
    sanitize_child_output, write_global_config,
};

#[derive(Debug, Clone)]
pub struct CatalogIndex {
    pub alias: String,
    pub source: String,
    pub root: PathBuf,
    pub metadata: CatalogMetadata,
    pub skills: BTreeMap<String, CatalogSkill>,
}

#[derive(Debug, Clone)]
pub struct CatalogSkill {
    pub name: String,
    pub description: String,
    pub scope: Option<String>,
    pub installed_name: String,
    pub global: bool,
    pub requires: Vec<String>,
}

pub fn load_global_config() -> Result<GlobalConfig> {
    let config: GlobalConfig = read_json_or_default(&global_config_path()?)?;
    validate_schema(config.version, "global config")?;
    for alias in config.catalogs.keys() {
        validate_alias(alias)?;
    }
    Ok(config)
}

pub fn add_catalog(alias: &str, source: &str) -> Result<()> {
    validate_alias(alias)?;
    if source.trim().is_empty() {
        bail!("catalog source cannot be empty");
    }
    let mut config = load_global_config()?;
    if config.catalogs.contains_key(alias) {
        bail!("catalog alias already exists: {alias}");
    }
    let registration = CatalogRegistration {
        source: source.to_owned(),
    };
    let index = sync_catalog(alias, &registration)?;
    config.catalogs.insert(alias.to_owned(), registration);
    write_global_config(&config)?;
    println!(
        "added catalog {alias}: {} skill{} from {source}",
        index.skills.len(),
        if index.skills.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

pub fn sync_registered_catalogs(config: &GlobalConfig) -> Result<BTreeMap<String, CatalogIndex>> {
    config
        .catalogs
        .iter()
        .map(|(alias, registration)| {
            sync_catalog(alias, registration).map(|catalog| (alias.clone(), catalog))
        })
        .collect()
}

pub fn sync_catalog(alias: &str, registration: &CatalogRegistration) -> Result<CatalogIndex> {
    validate_alias(alias)?;
    let source_path = PathBuf::from(&registration.source);
    let root = if source_path.exists() {
        source_path
            .canonicalize()
            .with_context(|| format!("resolving catalog source {}", registration.source))?
    } else {
        clone_catalog(alias, &registration.source)?
    };
    scan_catalog(alias, &registration.source, &root)
}

fn clone_catalog(alias: &str, source: &str) -> Result<PathBuf> {
    let catalogs_root = cache_root()?.join("catalogs");
    crate::paths::ensure_real_dir(&catalogs_root)?;
    let destination = catalogs_root.join(alias);
    safe_remove_owned_dir(&destination, &catalogs_root)?;

    let candidates = clone_candidates(source);
    let mut failures = Vec::new();
    for candidate in candidates {
        let output = Command::new("git")
            .args(["clone", "--depth", "1", &candidate])
            .arg(&destination)
            .output()
            .with_context(|| format!("starting git clone for {source}"))?;
        if output.status.success() {
            return destination
                .canonicalize()
                .with_context(|| format!("resolving cloned catalog {alias}"));
        }
        failures.push(sanitize_child_output(&output.stderr));
        safe_remove_owned_dir(&destination, &catalogs_root)?;
    }
    bail!(
        "failed to clone catalog {source}: {}",
        failures.join(" | ").trim()
    )
}

fn clone_candidates(source: &str) -> Vec<String> {
    let slash_count = source.bytes().filter(|byte| *byte == b'/').count();
    if slash_count == 1 && !source.contains(':') && !source.starts_with('.') {
        vec![
            format!("https://github.com/{source}.git"),
            format!("git@github.com:{source}.git"),
        ]
    } else {
        vec![source.to_owned()]
    }
}

fn scan_catalog(alias: &str, source: &str, root: &Path) -> Result<CatalogIndex> {
    let metadata_path = root.join("skiller.json");
    let metadata: CatalogMetadata = if metadata_path.exists() {
        let value: CatalogMetadata = read_json(&metadata_path)?;
        validate_schema(value.version, "catalog metadata")?;
        value
    } else {
        CatalogMetadata::default()
    };
    for scope in metadata.scopes.keys() {
        if !valid_name(scope) {
            bail!("invalid scope name in {}: {scope}", metadata_path.display());
        }
    }

    let skills_root = root.join("skills");
    let entries = std::fs::read_dir(&skills_root).with_context(|| {
        format!(
            "catalog has no readable skills directory: {}",
            skills_root.display()
        )
    })?;
    let mut skills = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let directory_name = entry.file_name().to_string_lossy().into_owned();
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&skill_path)
            .with_context(|| format!("reading {}", skill_path.display()))?;
        let frontmatter = frontmatter(&raw, &skill_path)?;
        let name = scalar(frontmatter, "name").unwrap_or_else(|| directory_name.clone());
        if name != directory_name || !valid_name(&name) {
            bail!(
                "Agent Skills requires a valid name matching its folder: {}",
                skill_path.display()
            );
        }
        let description =
            scalar(frontmatter, "description").unwrap_or_else(|| "No description".to_owned());
        let skill_metadata = metadata.skills.get(&name).cloned().unwrap_or_default();
        let scope = skill_metadata.scope;
        if let Some(scope) = &scope
            && !metadata.scopes.contains_key(scope)
        {
            bail!("skill {name} references unknown scope {scope}");
        }
        let requires = nested_scalar(frontmatter, "skiller.requires")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|dependency| !dependency.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let installed_name = installed_name(&name, scope.as_deref())?;
        skills.insert(
            name.clone(),
            CatalogSkill {
                name,
                description,
                scope,
                installed_name,
                global: skill_metadata.global,
                requires,
            },
        );
    }
    if skills.is_empty() {
        bail!(
            "catalog contains no flat skills under {}",
            skills_root.display()
        );
    }
    for name in metadata.skills.keys() {
        if !skills.contains_key(name) {
            bail!("catalog metadata references missing skill: {name}");
        }
    }
    validate_dependencies(&skills)?;
    Ok(CatalogIndex {
        alias: alias.to_owned(),
        source: source.to_owned(),
        root: root.to_owned(),
        metadata,
        skills,
    })
}

pub fn installed_name(name: &str, scope: Option<&str>) -> Result<String> {
    // ^ Agent Skills names and matching folder names: https://agentskills.io/specification
    let value = match scope {
        Some(scope) => format!("{name}-{scope}"),
        None => name.to_owned(),
    };
    if !valid_name(&value) {
        bail!("postfixed skill name is not Agent Skills compatible: {value}");
    }
    Ok(value)
}

fn frontmatter<'a>(raw: &'a str, path: &Path) -> Result<&'a str> {
    let mut lines = raw.split_inclusive('\n');
    let first = lines
        .next()
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n']);
    if first != "---" {
        bail!("SKILL.md is missing YAML frontmatter: {}", path.display());
    }
    let start = first.len() + raw[first.len()..].find('\n').map_or(0, |_| 1);
    let rest = &raw[start..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok(&rest[..offset]);
        }
        offset += line.len();
    }
    bail!("SKILL.md frontmatter is not closed: {}", path.display())
}

fn scalar(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<_> = frontmatter.lines().collect();
    lines.iter().enumerate().find_map(|(index, line)| {
        let (candidate, value) = line.split_once(':')?;
        if candidate.trim() != key || candidate.starts_with(char::is_whitespace) {
            return None;
        }
        scalar_value(&lines, index, value)
    })
}

fn nested_scalar(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<_> = frontmatter.lines().collect();
    lines.iter().enumerate().find_map(|(index, line)| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key)
            .then(|| scalar_value(&lines, index, value))
            .flatten()
    })
}

fn scalar_value(lines: &[&str], index: usize, value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('>') || value.starts_with('|') {
        let folded = lines[index + 1..]
            .iter()
            .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace))
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return (!folded.is_empty()).then_some(folded);
    }
    if value.is_empty() {
        return None;
    }
    Some(value.trim_matches(['\'', '"']).to_owned())
}

fn validate_dependencies(skills: &BTreeMap<String, CatalogSkill>) -> Result<()> {
    for skill in skills.values() {
        for dependency in &skill.requires {
            if dependency == &skill.name || !skills.contains_key(dependency) {
                bail!("skill {} has invalid dependency {dependency}", skill.name);
            }
        }
    }
    fn visit(
        name: &str,
        skills: &BTreeMap<String, CatalogSkill>,
        visiting: &mut Vec<String>,
        visited: &mut std::collections::BTreeSet<String>,
    ) -> Result<()> {
        if let Some(index) = visiting.iter().position(|candidate| candidate == name) {
            let mut cycle = visiting[index..].to_vec();
            cycle.push(name.to_owned());
            bail!("catalog dependency cycle: {}", cycle.join(" -> "));
        }
        if visited.contains(name) {
            return Ok(());
        }
        visiting.push(name.to_owned());
        for dependency in &skills[name].requires {
            visit(dependency, skills, visiting, visited)?;
        }
        visiting.pop();
        visited.insert(name.to_owned());
        Ok(())
    }
    let mut visited = std::collections::BTreeSet::new();
    for name in skills.keys() {
        visit(name, skills, &mut Vec::new(), &mut visited)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_postfixed_with_portable_separator() {
        assert_eq!(
            installed_name("develop", Some("engineering")).unwrap(),
            "develop-engineering"
        );
        assert!(installed_name(&"a".repeat(60), Some("scope")).is_err());
    }

    #[test]
    fn github_shorthand_has_authenticated_fallback() {
        assert_eq!(
            clone_candidates("vlwkaos/skills"),
            vec![
                "https://github.com/vlwkaos/skills.git",
                "git@github.com:vlwkaos/skills.git"
            ]
        );
    }

    #[test]
    fn nested_skiller_dependency_string_is_parsed() {
        let frontmatter = "name: develop\nmetadata:\n  skiller.requires: \"recall,simplify\"\n";
        assert_eq!(
            nested_scalar(frontmatter, "skiller.requires").as_deref(),
            Some("recall,simplify")
        );
    }

    #[test]
    fn folded_description_is_joined_for_noninteractive_output() {
        let frontmatter = "name: recall\ndescription: >-\n  Load project context before planning.\n  Skip literal lookups.\nmetadata:\n  skiller.requires: dream\n";
        assert_eq!(
            scalar(frontmatter, "description").as_deref(),
            Some("Load project context before planning. Skip literal lookups.")
        );
    }
}
