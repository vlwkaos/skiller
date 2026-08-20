use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub catalogs: BTreeMap<String, CatalogRegistration>,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogRegistration {
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SkillSelection {
    Mode(SelectionMode),
    Detailed {
        mode: SelectionMode,
        #[serde(default)]
        gitignore: bool,
    },
}

impl SkillSelection {
    pub fn mode(&self) -> SelectionMode {
        match self {
            Self::Mode(mode) | Self::Detailed { mode, .. } => *mode,
        }
    }

    pub fn gitignore(&self) -> bool {
        matches!(
            self,
            Self::Detailed {
                gitignore: true,
                ..
            }
        )
    }

    pub fn from_parts(mode: SelectionMode, gitignore: bool) -> Self {
        if gitignore {
            Self::Detailed { mode, gitignore }
        } else {
            Self::Mode(mode)
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SelectionMode {
    Enable,
    Manual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EffectiveMode {
    Enable,
    Manual,
    Dependency,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogMetadata {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub scopes: BTreeMap<String, ScopeMetadata>,
    #[serde(default)]
    pub skills: BTreeMap<String, CatalogSkillMetadata>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeMetadata {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub order: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogSkillMetadata {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub global: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledState {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub skills: BTreeMap<String, InstalledSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledSkill {
    pub catalog: String,
    pub source_skill: String,
    pub installed_name: String,
    pub path: String,
    pub mode: EffectiveMode,
    #[serde(default)]
    pub gitignore: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            catalogs: BTreeMap::new(),
            skills: BTreeMap::new(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
        }
    }
}

impl Default for CatalogMetadata {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            scopes: BTreeMap::new(),
            skills: BTreeMap::new(),
        }
    }
}

impl Default for InstalledState {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
        }
    }
}

pub fn validate_schema(version: u32, kind: &str) -> Result<()> {
    if version != SCHEMA_VERSION {
        bail!("unsupported {kind} schema version {version}; expected {SCHEMA_VERSION}");
    }
    Ok(())
}

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn validate_alias(value: &str) -> Result<()> {
    if !valid_name(value) {
        bail!("catalog alias must use lowercase letters, numbers, and single hyphens: {value}");
    }
    Ok(())
}

const fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_defaults_use_current_schema() {
        assert_eq!(GlobalConfig::default().version, SCHEMA_VERSION);
        assert_eq!(ProjectConfig::default().version, SCHEMA_VERSION);
        assert_eq!(CatalogMetadata::default().version, SCHEMA_VERSION);
        assert_eq!(InstalledState::default().version, SCHEMA_VERSION);
    }

    #[test]
    fn selection_shorthand_and_detail_round_trip() {
        let config: ProjectConfig = serde_json::from_str(
            r#"{"version":1,"skills":{"pyg/a":"enable","pyg/b":{"mode":"manual","gitignore":true}}}"#,
        )
        .unwrap();
        assert_eq!(config.skills["pyg/a"].mode(), SelectionMode::Enable);
        assert!(!config.skills["pyg/a"].gitignore());
        assert_eq!(config.skills["pyg/b"].mode(), SelectionMode::Manual);
        assert!(config.skills["pyg/b"].gitignore());
    }

    #[test]
    fn agent_skill_names_reject_colons_and_double_hyphens() {
        assert!(valid_name("develop-engineering"));
        assert!(!valid_name("develop:engineering"));
        assert!(!valid_name("develop--engineering"));
    }
}
