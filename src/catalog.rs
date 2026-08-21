use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::model::{
    CatalogMetadata, CatalogRegistration, CatalogSkillMetadata, GlobalConfig, valid_name,
    validate_alias, validate_schema,
};
use crate::paths::{
    cache_root, copy_tree, ensure_real_dir, global_config_path, read_json, read_json_or_default,
    safe_remove_owned_dir, sanitize_child_output, write_global_config, write_json_atomic,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogEligibility {
    Global,
    Project,
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

// ^ README.md#catalog-authoring owns the explicit checkout and eligibility boundary.
pub fn add_skill(
    catalog_root: &Path,
    source: &Path,
    scope: &str,
    eligibility: CatalogEligibility,
) -> Result<()> {
    for (kind, path) in [("catalog root", catalog_root), ("skill source", source)] {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspecting {kind} {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("{kind} must be a real directory: {}", path.display());
        }
    }
    let root = catalog_root
        .canonicalize()
        .with_context(|| format!("resolving catalog root {}", catalog_root.display()))?;
    let source = source
        .canonicalize()
        .with_context(|| format!("resolving skill source {}", source.display()))?;
    if source.starts_with(&root) {
        bail!(
            "skill source must be outside the target catalog: {}",
            source.display()
        );
    }
    if !valid_name(scope) {
        bail!("invalid catalog scope: {scope}");
    }

    let metadata_path = root.join("skiller.json");
    let metadata_file = std::fs::symlink_metadata(&metadata_path)
        .with_context(|| format!("inspecting {}", metadata_path.display()))?;
    if metadata_file.file_type().is_symlink() || !metadata_file.is_file() {
        bail!(
            "catalog metadata must be a real file: {}",
            metadata_path.display()
        );
    }
    let mut metadata: CatalogMetadata = read_json(&metadata_path)?;
    validate_schema(metadata.version, "catalog metadata")?;
    if !metadata.scopes.contains_key(scope) {
        bail!("catalog has no scope named {scope}");
    }
    let current = scan_catalog("authoring", &root.display().to_string(), &root)?;
    let skill = read_skill_directory(
        &source,
        CatalogSkillMetadata {
            scope: Some(scope.to_owned()),
            global: eligibility == CatalogEligibility::Global,
        },
    )?;
    if current.skills.contains_key(&skill.name) || metadata.skills.contains_key(&skill.name) {
        bail!("catalog already contains skill: {}", skill.name);
    }
    for dependency in &skill.requires {
        let required = current
            .skills
            .get(dependency)
            .with_context(|| format!("skill {} has invalid dependency {dependency}", skill.name))?;
        if eligibility == CatalogEligibility::Global && !required.global {
            bail!(
                "global skill {} requires project-only skill {dependency}; mark its dependency closure global",
                skill.name
            );
        }
    }

    let skills_root = root.join("skills");
    ensure_real_dir(&skills_root)?;
    let destination = skills_root.join(&skill.name);
    if destination.exists() {
        bail!(
            "catalog skill destination already exists: {}",
            destination.display()
        );
    }
    let staging = root.join(format!(
        ".skiller-add-{}-{}",
        std::process::id(),
        skill.name
    ));
    if staging.exists() {
        bail!(
            "catalog add staging path already exists: {}",
            staging.display()
        );
    }

    if let Err(error) = copy_tree(&source, &staging) {
        let _ = safe_remove_owned_dir(&staging, &root);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, &destination) {
        let _ = safe_remove_owned_dir(&staging, &root);
        return Err(error).with_context(|| format!("committing {}", destination.display()));
    }

    metadata.skills.insert(
        skill.name.clone(),
        CatalogSkillMetadata {
            scope: Some(scope.to_owned()),
            global: eligibility == CatalogEligibility::Global,
        },
    );
    if let Err(error) = write_json_atomic(&metadata_path, &metadata) {
        let cleanup = safe_remove_owned_dir(&destination, &skills_root);
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "catalog metadata write failed and cleanup also failed: {cleanup:#}"
            ))),
        };
    }

    println!(
        "added {} as {} skill in scope {scope}",
        skill.name,
        if eligibility == CatalogEligibility::Global {
            "global"
        } else {
            "project"
        }
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
        let path = entry.path();
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        let directory_name = entry.file_name().to_string_lossy().into_owned();
        let skill = read_skill_directory(
            &path,
            metadata
                .skills
                .get(&directory_name)
                .cloned()
                .unwrap_or_default(),
        )?;
        if let Some(scope) = &skill.scope
            && !metadata.scopes.contains_key(scope)
        {
            bail!("skill {} references unknown scope {scope}", skill.name);
        }
        skills.insert(skill.name.clone(), skill);
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
    validate_renames(&metadata, &skills)?;
    Ok(CatalogIndex {
        alias: alias.to_owned(),
        source: source.to_owned(),
        root: root.to_owned(),
        metadata,
        skills,
    })
}

fn read_skill_directory(directory: &Path, metadata: CatalogSkillMetadata) -> Result<CatalogSkill> {
    let skill_path = directory.join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_path)
        .with_context(|| format!("reading {}", skill_path.display()))?;
    let frontmatter = frontmatter(&raw, &skill_path)?;
    let folder_name = directory
        .file_name()
        .context("skill directory has no name")?
        .to_string_lossy()
        .into_owned();
    let name = scalar(frontmatter, "name").unwrap_or_else(|| folder_name.clone());
    if name != folder_name || !valid_name(&name) {
        bail!(
            "Agent Skills requires a valid name matching its folder: {}",
            skill_path.display()
        );
    }
    let description =
        scalar(frontmatter, "description").unwrap_or_else(|| "No description".to_owned());
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
    Ok(CatalogSkill {
        installed_name: installed_name(&name, metadata.scope.as_deref())?,
        name,
        description,
        scope: metadata.scope,
        global: metadata.global,
        requires,
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

fn validate_renames(
    metadata: &CatalogMetadata,
    skills: &BTreeMap<String, CatalogSkill>,
) -> Result<()> {
    for (old, new) in &metadata.renames {
        if !valid_name(old) || !valid_name(new) {
            bail!("catalog rename must use valid skill names: {old} -> {new}");
        }
        if skills.contains_key(old) {
            bail!("catalog rename source still exists as a skill: {old}");
        }
    }
    for start in metadata.renames.keys() {
        let mut path = Vec::new();
        let mut current = start.as_str();
        while let Some(next) = metadata.renames.get(current) {
            if let Some(index) = path.iter().position(|name| *name == current) {
                let mut cycle = path[index..].to_vec();
                cycle.push(current);
                bail!("catalog rename cycle: {}", cycle.join(" -> "));
            }
            path.push(current);
            current = next;
        }
        if !skills.contains_key(current) {
            path.push(current);
            bail!(
                "catalog rename has no current target: {}",
                path.join(" -> ")
            );
        }
    }
    Ok(())
}

pub fn resolve_rename(catalog: &CatalogIndex, name: &str) -> Option<String> {
    let mut current = name;
    let mut changed = false;
    while let Some(next) = catalog.metadata.renames.get(current) {
        current = next;
        changed = true;
    }
    changed.then(|| current.to_owned())
}

fn validate_dependencies(skills: &BTreeMap<String, CatalogSkill>) -> Result<()> {
    for skill in skills.values() {
        for dependency in &skill.requires {
            if !skills.contains_key(dependency) {
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

    fn dependency_skill(name: &str, requires: &[&str]) -> CatalogSkill {
        CatalogSkill {
            name: name.to_owned(),
            description: name.to_owned(),
            scope: None,
            installed_name: name.to_owned(),
            global: true,
            requires: requires
                .iter()
                .map(|dependency| (*dependency).to_owned())
                .collect(),
        }
    }

    #[test]
    fn dependency_cycles_report_the_complete_path() {
        let direct = BTreeMap::from([("a".to_owned(), dependency_skill("a", &["a"]))]);
        assert_eq!(
            validate_dependencies(&direct).unwrap_err().to_string(),
            "catalog dependency cycle: a -> a"
        );

        let indirect = BTreeMap::from([
            ("a".to_owned(), dependency_skill("a", &["b"])),
            ("b".to_owned(), dependency_skill("b", &["c"])),
            ("c".to_owned(), dependency_skill("c", &["a"])),
        ]);
        assert_eq!(
            validate_dependencies(&indirect).unwrap_err().to_string(),
            "catalog dependency cycle: a -> b -> c -> a"
        );
    }

    #[test]
    fn catalog_renames_require_acyclic_paths_to_current_skills() {
        let skills = BTreeMap::from([("learn".to_owned(), dependency_skill("learn", &[]))]);
        let mut metadata = CatalogMetadata::default();
        metadata
            .renames
            .insert("digest".to_owned(), "teach".to_owned());
        metadata
            .renames
            .insert("teach".to_owned(), "learn".to_owned());
        validate_renames(&metadata, &skills).unwrap();

        metadata.renames = BTreeMap::from([
            ("digest".to_owned(), "teach".to_owned()),
            ("teach".to_owned(), "digest".to_owned()),
        ]);
        assert_eq!(
            validate_renames(&metadata, &skills)
                .unwrap_err()
                .to_string(),
            "catalog rename cycle: digest -> teach -> digest"
        );
    }

    #[test]
    fn add_skill_requires_explicit_scope_and_preserves_global_closure() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/catalog-add-skill");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("catalog");
        let source = base.join("candidate/learn");
        std::fs::create_dir_all(root.join("skills/note")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            root.join("skiller.json"),
            r#"{"version":1,"scopes":{"knowledge":{"order":10},"learning":{"order":20}},"skills":{"note":{"scope":"knowledge","global":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("skills/note/SKILL.md"),
            "---\nname: note\ndescription: Save notes\n---\n",
        )
        .unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: learn\ndescription: Teach deeply\nmetadata:\n  skiller.requires: note\n---\n",
        )
        .unwrap();

        add_skill(&root, &source, "learning", CatalogEligibility::Global).unwrap();
        assert!(root.join("skills/learn/SKILL.md").is_file());
        let metadata: CatalogMetadata = read_json(&root.join("skiller.json")).unwrap();
        assert_eq!(
            metadata.skills["learn"],
            CatalogSkillMetadata {
                scope: Some("learning".to_owned()),
                global: true,
            }
        );
        assert!(
            add_skill(&root, &source, "missing", CatalogEligibility::Project)
                .unwrap_err()
                .to_string()
                .contains("no scope")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn add_skill_rejects_project_only_global_dependencies_without_mutation() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/catalog-add-project-dependency");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("catalog");
        let source = base.join("candidate/root");
        std::fs::create_dir_all(root.join("skills/helper")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            root.join("skiller.json"),
            r#"{"version":1,"scopes":{"test":{"order":10}},"skills":{"helper":{"scope":"test","global":false}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("skills/helper/SKILL.md"),
            "---\nname: helper\ndescription: Project helper\n---\n",
        )
        .unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: root\ndescription: Global root\nmetadata:\n  skiller.requires: helper\n---\n",
        )
        .unwrap();

        let error = add_skill(&root, &source, "test", CatalogEligibility::Global)
            .unwrap_err()
            .to_string();
        assert!(error.contains("project-only skill helper"));
        assert!(!root.join("skills/root").exists());
        let metadata: CatalogMetadata = read_json(&root.join("skiller.json")).unwrap();
        assert!(!metadata.skills.contains_key("root"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn add_skill_rejects_nested_symlinks_and_cleans_staging() {
        use std::os::unix::fs::symlink;

        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/catalog-add-symlink");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("catalog");
        let source = base.join("candidate/root");
        std::fs::create_dir_all(root.join("skills/helper")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            root.join("skiller.json"),
            r#"{"version":1,"scopes":{"test":{"order":10}},"skills":{"helper":{"scope":"test","global":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("skills/helper/SKILL.md"),
            "---\nname: helper\ndescription: Helper\n---\n",
        )
        .unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: root\ndescription: Root\nmetadata:\n  skiller.requires: helper\n---\n",
        )
        .unwrap();
        std::fs::write(base.join("outside.txt"), "outside").unwrap();
        symlink(base.join("outside.txt"), source.join("escape")).unwrap();

        let error = add_skill(&root, &source, "test", CatalogEligibility::Global)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported symlink"));
        assert!(!root.join("skills/root").exists());
        assert!(
            !root
                .join(format!(".skiller-add-{}-root", std::process::id()))
                .exists()
        );
        let metadata: CatalogMetadata = read_json(&root.join("skiller.json")).unwrap();
        assert!(!metadata.skills.contains_key("root"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
