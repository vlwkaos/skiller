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
    pub revision: Option<String>,
    pub metadata: CatalogMetadata,
    pub skills: BTreeMap<String, CatalogSkill>,
}

#[derive(Debug, Clone)]
pub struct CatalogSkill {
    pub name: String,
    pub description: String,
    pub digest: String,
    pub scope: Option<String>,
    pub installed_name: String,
    pub global: bool,
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogAvailability {
    Available,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct CatalogStatus {
    pub alias: String,
    pub availability: CatalogAvailability,
    pub warning: Option<String>,
    /// A stale index is intentionally exposed only to read-only callers.
    pub catalog: Option<CatalogIndex>,
}

#[derive(Debug, Clone, Default)]
pub struct CatalogSync {
    /// Fresh, canonical catalogs which may participate in reconciliation.
    pub catalogs: BTreeMap<String, CatalogIndex>,
    pub statuses: BTreeMap<String, CatalogStatus>,
}

impl CatalogSync {
    pub fn unavailable_aliases(&self) -> std::collections::BTreeSet<String> {
        self.statuses
            .iter()
            .filter(|(_, status)| status.availability != CatalogAvailability::Available)
            .map(|(alias, _)| alias.clone())
            .collect()
    }
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
    let config = load_global_config()?;
    if config.catalogs.contains_key(alias) {
        bail!("catalog alias already exists: {alias}");
    }
    configure_catalog(alias, Some(source), None, false, None, false)
}

pub fn configure_catalog(
    alias: &str,
    source: Option<&str>,
    reference: Option<&str>,
    clear_ref: bool,
    authoring_root: Option<&Path>,
    clear_authoring_root: bool,
) -> Result<()> {
    validate_alias(alias)?;
    let mut config = load_global_config()?;
    let existing = config.catalogs.get(alias).cloned();
    let source = source
        .map(str::to_owned)
        .or_else(|| existing.as_ref().map(|value| value.source.clone()))
        .with_context(|| format!("new catalog {alias} requires --source"))?;
    if source.trim().is_empty() {
        bail!("catalog source cannot be empty");
    }
    let configured_ref = if clear_ref {
        None
    } else {
        reference
            .map(str::to_owned)
            .or_else(|| existing.as_ref().and_then(|value| value.r#ref.clone()))
    };
    let configured_authoring = if clear_authoring_root {
        None
    } else {
        authoring_root
            .map(|path| path.display().to_string())
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|value| value.authoring_root.clone())
            })
    };
    if let Some(root) = &configured_authoring {
        validate_authoring_root(alias, Path::new(root))?;
    }
    let registration = CatalogRegistration {
        source,
        r#ref: configured_ref,
        authoring_root: configured_authoring,
    };
    let index = sync_catalog(alias, &registration)?;
    config.catalogs.insert(alias.to_owned(), registration);
    write_global_config(&config)?;
    println!(
        "configured catalog {alias}: {} skill{} from {}",
        index.skills.len(),
        if index.skills.len() == 1 { "" } else { "s" },
        index.source
    );
    Ok(())
}

fn validate_authoring_root(alias: &str, root: &Path) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("inspecting authoring root for catalog {alias}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("catalog {alias} authoring root must be a real directory");
    }
    root.canonicalize()
        .with_context(|| format!("resolving authoring root for catalog {alias}"))
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
    if std::fs::symlink_metadata(&destination).is_ok() {
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
    if std::fs::symlink_metadata(&staging).is_ok() {
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
    sync_catalog_with_policy(alias, registration, false).map_err(|error| match error {
        SyncError::Unreachable(error) | SyncError::Invalid(error) => error,
    })
}

/// Refresh every registration without prompting. Only source acquisition errors are
/// downgraded: a reached catalog which fails validation remains an error.
pub fn sync_registered_catalogs_resilient(config: &GlobalConfig) -> Result<CatalogSync> {
    let mut result = CatalogSync::default();
    for (alias, registration) in &config.catalogs {
        match sync_catalog_with_policy(alias, registration, true) {
            Ok(catalog) => {
                result.catalogs.insert(alias.clone(), catalog.clone());
                result.statuses.insert(
                    alias.clone(),
                    CatalogStatus {
                        alias: alias.clone(),
                        availability: CatalogAvailability::Available,
                        warning: None,
                        catalog: Some(catalog),
                    },
                );
            }
            Err(SyncError::Invalid(error)) => return Err(error),
            Err(SyncError::Unreachable(error)) => {
                let stale = cached_catalog(alias, registration).ok();
                result.statuses.insert(
                    alias.clone(),
                    CatalogStatus {
                        alias: alias.clone(),
                        availability: if stale.is_some() {
                            CatalogAvailability::Stale
                        } else {
                            CatalogAvailability::Unavailable
                        },
                        warning: Some(sanitized_warning(&error)),
                        catalog: stale,
                    },
                );
            }
        }
    }
    Ok(result)
}

fn cached_catalog(alias: &str, registration: &CatalogRegistration) -> Result<CatalogIndex> {
    scan_catalog(
        alias,
        &registration.source,
        &cache_root()?.join("catalogs").join(alias),
    )
}

fn sanitized_warning(error: &anyhow::Error) -> String {
    // Do not echo a registered URL or local path (which can contain credentials).
    let error_message = error.to_string();
    let detail = error_message
        .rsplit_once(": ")
        .map(|(_, detail)| detail)
        .unwrap_or("source unavailable");
    let mut value: String = detail
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    value.truncate(240);
    if value.is_empty() {
        "source unavailable".to_owned()
    } else {
        value
    }
}

/// A writable draft is supplementary to its canonical catalog. Its failure must not
/// turn a successful published update check into a failure.
pub fn sync_registered_authoring_catalogs_resilient(
    config: &GlobalConfig,
    canonical: &BTreeMap<String, CatalogIndex>,
) -> (BTreeMap<String, CatalogIndex>, BTreeMap<String, String>) {
    let mut catalogs = canonical.clone();
    let mut warnings = BTreeMap::new();
    for (alias, registration) in &config.catalogs {
        let Some(root) = &registration.authoring_root else {
            continue;
        };
        if !canonical.contains_key(alias) {
            continue;
        }
        match validate_authoring_root(alias, Path::new(root))
            .and_then(|root| scan_catalog(alias, &registration.source, &root))
        {
            Ok(catalog) => {
                catalogs.insert(alias.clone(), catalog);
            }
            Err(error) => {
                warnings.insert(alias.clone(), sanitized_warning(&error));
            }
        }
    }
    (catalogs, warnings)
}

enum SyncError {
    Unreachable(anyhow::Error),
    Invalid(anyhow::Error),
}

fn sync_catalog_with_policy(
    alias: &str,
    registration: &CatalogRegistration,
    noninteractive: bool,
) -> std::result::Result<CatalogIndex, SyncError> {
    validate_alias(alias).map_err(SyncError::Invalid)?;
    if registration.r#ref.as_deref().is_some_and(str::is_empty) {
        return Err(SyncError::Invalid(anyhow::anyhow!(
            "catalog {alias} ref cannot be empty"
        )));
    }
    let source_path = PathBuf::from(&registration.source);
    let (root, refreshed) = if source_path.exists() && registration.r#ref.is_none() {
        (
            source_path.canonicalize().map_err(|error| {
                SyncError::Unreachable(
                    anyhow::Error::new(error)
                        .context(format!("resolving catalog source {}", registration.source)),
                )
            })?,
            false,
        )
    } else {
        (
            clone_catalog(alias, registration, noninteractive).map_err(SyncError::Unreachable)?,
            true,
        )
    };
    // Validate the new clone before it replaces the last known-good cache.
    if let Err(error) = scan_catalog(alias, &registration.source, &root) {
        if refreshed && let Ok(catalogs_root) = cache_root().map(|root| root.join("catalogs")) {
            let _ = safe_remove_owned_dir(&root, &catalogs_root);
        }
        return Err(SyncError::Invalid(error));
    }
    if refreshed {
        let catalogs_root = cache_root()
            .map_err(SyncError::Unreachable)?
            .join("catalogs");
        let destination = catalogs_root.join(alias);
        safe_remove_owned_dir(&destination, &catalogs_root).map_err(SyncError::Unreachable)?;
        std::fs::rename(&root, &destination).map_err(|error| {
            SyncError::Unreachable(
                anyhow::Error::new(error).context(format!("committing refreshed catalog {alias}")),
            )
        })?;
        return scan_catalog(alias, &registration.source, &destination).map_err(SyncError::Invalid);
    }
    scan_catalog(alias, &registration.source, &root).map_err(SyncError::Invalid)
}

fn clone_catalog(
    alias: &str,
    registration: &CatalogRegistration,
    noninteractive: bool,
) -> Result<PathBuf> {
    let catalogs_root = cache_root()?.join("catalogs");
    crate::paths::ensure_real_dir(&catalogs_root)?;
    // Preserve the last good clone until this refresh has succeeded.
    let staging = catalogs_root.join(format!(".{alias}-refresh-{}", std::process::id()));
    safe_remove_owned_dir(&staging, &catalogs_root)?;

    let candidates = clone_candidates(&registration.source);
    let mut failures = Vec::new();
    for candidate in candidates {
        let mut command = Command::new("git");
        command.args(["clone", "--depth", "1"]);
        let commit_ref = registration.r#ref.as_deref().is_some_and(|reference| {
            reference.len() == 40 && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if let Some(reference) = &registration.r#ref
            && !commit_ref
        {
            command.args(["--branch", reference, "--single-branch"]);
        }
        if noninteractive {
            command.env("GIT_TERMINAL_PROMPT", "0").env(
                "GIT_SSH_COMMAND",
                "ssh -o BatchMode=yes -o ConnectTimeout=5",
            );
        }
        let output = command
            .arg(&candidate)
            .arg(&staging)
            .output()
            .with_context(|| format!("starting git clone for {}", registration.source))?;
        if output.status.success() {
            if commit_ref {
                let reference = registration.r#ref.as_deref().expect("commit ref exists");
                let mut fetch = Command::new("git");
                fetch
                    .args(["fetch", "--depth", "1", "origin", reference])
                    .current_dir(&staging);
                if noninteractive {
                    fetch.env("GIT_TERMINAL_PROMPT", "0").env(
                        "GIT_SSH_COMMAND",
                        "ssh -o BatchMode=yes -o ConnectTimeout=5",
                    );
                }
                let fetched = fetch.output()?;
                let checked_out = fetched.status.success()
                    && Command::new("git")
                        .args(["checkout", "--detach", "FETCH_HEAD"])
                        .current_dir(&staging)
                        .output()?
                        .status
                        .success();
                if !checked_out {
                    failures.push(sanitize_child_output(&fetched.stderr));
                    safe_remove_owned_dir(&staging, &catalogs_root)?;
                    continue;
                }
            }
            return staging
                .canonicalize()
                .with_context(|| format!("resolving cloned catalog {alias}"));
        }
        failures.push(sanitize_child_output(&output.stderr));
        safe_remove_owned_dir(&staging, &catalogs_root)?;
    }
    bail!(
        "failed to clone catalog {}: {}",
        registration.source,
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

pub(crate) fn scan_catalog(alias: &str, source: &str, root: &Path) -> Result<CatalogIndex> {
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
    let revision = git_revision(root)?;
    Ok(CatalogIndex {
        alias: alias.to_owned(),
        source: source.to_owned(),
        root: root.to_owned(),
        revision,
        metadata,
        skills,
    })
}

pub(crate) fn source_skill_name(directory: &Path) -> Result<String> {
    Ok(read_skill_directory(directory, CatalogSkillMetadata::default())?.name)
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
        digest: directory_digest(directory)?,
        name,
        description,
        scope: metadata.scope,
        global: metadata.global,
        requires,
    })
}

fn git_revision(root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| format!("reading catalog revision from {}", root.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!revision.is_empty()).then_some(revision))
}

fn directory_digest(root: &Path) -> Result<String> {
    fn update(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    fn walk(root: &Path, current: &Path, hash: &mut u64) -> Result<()> {
        let mut entries = std::fs::read_dir(current)
            .with_context(|| format!("reading skill directory {}", current.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("skill source contains a symlink: {}", path.display());
            }
            if metadata.is_dir() {
                walk(root, &path, hash)?;
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).expect("walked path is below root");
                update(hash, relative.to_string_lossy().as_bytes());
                update(hash, &[0]);
                update(hash, &std::fs::read(&path)?);
                update(hash, &[0xff]);
            }
        }
        Ok(())
    }
    let mut hash = 0xcbf29ce484222325;
    walk(root, root, &mut hash)?;
    Ok(format!("{hash:016x}"))
}

pub fn installed_name(name: &str, _scope: Option<&str>) -> Result<String> {
    // ^ Agent Skills names and matching folder names: https://agentskills.io/specification
    if !valid_name(name) {
        bail!("skill name is not Agent Skills compatible: {name}");
    }
    Ok(name.to_owned())
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
    fn reachable_malformed_catalog_remains_a_hard_error() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/malformed-catalog");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("skills/broken")).unwrap();
        std::fs::write(root.join("skills/broken/SKILL.md"), "not frontmatter").unwrap();
        let config = GlobalConfig {
            version: crate::model::SCHEMA_VERSION,
            catalogs: BTreeMap::from([(
                "bad".to_owned(),
                CatalogRegistration {
                    source: root.display().to_string(),
                    r#ref: None,
                    authoring_root: None,
                },
            )]),
            skills: BTreeMap::new(),
            agents: crate::model::default_agents(),
        };
        assert!(sync_registered_catalogs_resilient(&config).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scope_does_not_change_the_installed_name() {
        assert_eq!(
            installed_name("develop", Some("engineering")).unwrap(),
            "develop"
        );
        assert!(installed_name(&"a".repeat(65), Some("scope")).is_err());
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
    fn skill_digest_ignores_empty_runtime_directories() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/test-work/catalog-empty-directory-digest");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: test\ndescription: Test\n---\n",
        )
        .unwrap();
        let before = directory_digest(&root).unwrap();
        std::fs::create_dir_all(root.join(".claude/.cc-writes")).unwrap();
        assert_eq!(directory_digest(&root).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
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
            digest: "test".to_owned(),
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
        assert!(error.contains("symlink"));
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
