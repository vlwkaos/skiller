use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const SCHEMA_VERSION: u32 = 1;
pub const INSTALLED_STATE_VERSION: u32 = 3;
pub const PREVIOUS_INSTALLED_STATE_VERSION: u32 = 2;

pub fn default_agents() -> Vec<String> {
    ["universal", "claude-code", "pi"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub catalogs: BTreeMap<String, CatalogRegistration>,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillSelection>,
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogRegistration {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillSelection>,
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
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
    #[serde(default)]
    pub renames: BTreeMap<String, String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledState {
    pub version: u32,
    pub skills: BTreeMap<String, InstalledSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub installed_name: String,
    pub mode: EffectiveMode,
    pub gitignore: bool,
    pub digest: Option<String>,
    pub legacy_path: Option<String>,
}

#[derive(Serialize)]
struct CompactInstalledState<'a> {
    v: u32,
    skills: BTreeMap<&'a str, (&'a str, CompactMode, bool, &'a str)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CompactMode {
    E,
    M,
    D,
}

impl From<EffectiveMode> for CompactMode {
    fn from(value: EffectiveMode) -> Self {
        match value {
            EffectiveMode::Enable => Self::E,
            EffectiveMode::Manual => Self::M,
            EffectiveMode::Dependency => Self::D,
        }
    }
}

impl From<CompactMode> for EffectiveMode {
    fn from(value: CompactMode) -> Self {
        match value {
            CompactMode::E => Self::Enable,
            CompactMode::M => Self::Manual,
            CompactMode::D => Self::Dependency,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InstalledStateWire {
    CompactCurrent {
        v: u32,
        #[serde(default)]
        skills: BTreeMap<String, (String, CompactMode, bool, String)>,
    },
    CompactPrevious {
        v: u32,
        #[serde(default)]
        skills: BTreeMap<String, (String, CompactMode, bool)>,
    },
    Legacy {
        #[serde(default = "schema_version")]
        version: u32,
        #[serde(default)]
        skills: BTreeMap<String, LegacyInstalledSkill>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyInstalledSkill {
    catalog: String,
    source_skill: String,
    installed_name: String,
    path: String,
    mode: EffectiveMode,
    #[serde(default)]
    gitignore: bool,
}

impl Serialize for InstalledState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let skills = self
            .skills
            .iter()
            .map(|(key, skill)| {
                (
                    key.as_str(),
                    (
                        skill.installed_name.as_str(),
                        CompactMode::from(skill.mode),
                        skill.gitignore,
                        skill.digest.as_deref().unwrap_or(""),
                    ),
                )
            })
            .collect();
        CompactInstalledState {
            v: INSTALLED_STATE_VERSION,
            skills,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InstalledState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstalledStateWire::deserialize(deserializer)?;
        match wire {
            InstalledStateWire::CompactCurrent { v, skills } => {
                if v != INSTALLED_STATE_VERSION {
                    return Err(serde::de::Error::custom(format!(
                        "unsupported installed state schema version {v}; expected {INSTALLED_STATE_VERSION}"
                    )));
                }
                Ok(Self {
                    version: v,
                    skills: skills
                        .into_iter()
                        .map(|(key, (installed_name, mode, gitignore, digest))| {
                            (
                                key,
                                InstalledSkill {
                                    installed_name,
                                    mode: mode.into(),
                                    gitignore,
                                    digest: (!digest.is_empty()).then_some(digest),
                                    legacy_path: None,
                                },
                            )
                        })
                        .collect(),
                })
            }
            InstalledStateWire::CompactPrevious { v, skills } => {
                if v != PREVIOUS_INSTALLED_STATE_VERSION {
                    return Err(serde::de::Error::custom(format!(
                        "unsupported installed state schema version {v}; expected {PREVIOUS_INSTALLED_STATE_VERSION}"
                    )));
                }
                Ok(Self {
                    version: v,
                    skills: skills
                        .into_iter()
                        .map(|(key, (installed_name, mode, gitignore))| {
                            (
                                key,
                                InstalledSkill {
                                    installed_name,
                                    mode: mode.into(),
                                    gitignore,
                                    digest: None,
                                    legacy_path: None,
                                },
                            )
                        })
                        .collect(),
                })
            }
            InstalledStateWire::Legacy { version, skills } => {
                if version != SCHEMA_VERSION {
                    return Err(serde::de::Error::custom(format!(
                        "unsupported installed state schema version {version}; expected {SCHEMA_VERSION} or {INSTALLED_STATE_VERSION}"
                    )));
                }
                let mut converted = BTreeMap::new();
                for (key, skill) in skills {
                    let expected_key = format!("{}/{}", skill.catalog, skill.source_skill);
                    if key != expected_key {
                        return Err(serde::de::Error::custom(format!(
                            "installed state key {key} does not match {expected_key}"
                        )));
                    }
                    converted.insert(
                        key,
                        InstalledSkill {
                            installed_name: skill.installed_name,
                            mode: skill.mode,
                            gitignore: skill.gitignore,
                            digest: None,
                            legacy_path: Some(skill.path),
                        },
                    );
                }
                Ok(Self {
                    version,
                    skills: converted,
                })
            }
        }
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            catalogs: BTreeMap::new(),
            skills: BTreeMap::new(),
            agents: default_agents(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
            agents: default_agents(),
        }
    }
}

impl Default for CatalogMetadata {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            scopes: BTreeMap::new(),
            skills: BTreeMap::new(),
            renames: BTreeMap::new(),
        }
    }
}

impl Default for InstalledState {
    fn default() -> Self {
        Self {
            version: INSTALLED_STATE_VERSION,
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

pub fn validate_installed_state(version: u32) -> Result<()> {
    if version != SCHEMA_VERSION
        && version != PREVIOUS_INSTALLED_STATE_VERSION
        && version != INSTALLED_STATE_VERSION
    {
        bail!(
            "unsupported installed state schema version {version}; expected {SCHEMA_VERSION}, {PREVIOUS_INSTALLED_STATE_VERSION}, or {INSTALLED_STATE_VERSION}"
        );
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
        assert_eq!(InstalledState::default().version, INSTALLED_STATE_VERSION);
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
    fn installed_state_reads_legacy_and_writes_compact_schema() {
        let legacy = r#"{"version":1,"skills":{"pyg/learn":{"catalog":"pyg","source_skill":"learn","installed_name":"learn-learning","path":".agents/skills/learn-learning","mode":"enable","gitignore":false}}}"#;
        let state: InstalledState = serde_json::from_str(legacy).unwrap();
        assert_eq!(state.version, SCHEMA_VERSION);
        assert_eq!(
            state.skills["pyg/learn"].legacy_path.as_deref(),
            Some(".agents/skills/learn-learning")
        );

        let compact = serde_json::to_string(&state).unwrap();
        assert_eq!(
            compact,
            r#"{"v":3,"skills":{"pyg/learn":["learn-learning","e",false,""]}}"#
        );
        let round_trip: InstalledState = serde_json::from_str(&compact).unwrap();
        assert_eq!(round_trip.version, INSTALLED_STATE_VERSION);
        assert_eq!(round_trip.skills["pyg/learn"].mode, EffectiveMode::Enable);
        let previous: InstalledState =
            serde_json::from_str(r#"{"v":2,"skills":{"pyg/learn":["learn-learning","e",false]}}"#)
                .unwrap();
        assert_eq!(previous.version, PREVIOUS_INSTALLED_STATE_VERSION);
        assert!(previous.skills["pyg/learn"].digest.is_none());
    }

    #[test]
    fn agent_skill_names_reject_colons_and_double_hyphens() {
        assert!(valid_name("develop-engineering"));
        assert!(!valid_name("develop:engineering"));
        assert!(!valid_name("develop--engineering"));
    }
}
