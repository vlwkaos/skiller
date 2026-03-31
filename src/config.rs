use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    #[default]
    Folder,
    Granular,
}

impl fmt::Display for LinkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkMode::Folder => write!(f, "folder"),
            LinkMode::Granular => write!(f, "granular"),
        }
    }
}

impl std::str::FromStr for LinkMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "folder" => Ok(LinkMode::Folder),
            "granular" => Ok(LinkMode::Granular),
            other => Err(format!("invalid link mode: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub r#type: String,
    pub path: PathBuf,
    #[serde(default)]
    pub mode: LinkMode,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub source: Option<PathBuf>,
    pub targets: Vec<Target>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let dir = dirs::home_dir()
            .context("cannot find home dir")?
            .join(".config/skiller");
        Ok(dir.join("config.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).context("parsing config")
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
    }

    pub fn source_dir(&self) -> Result<&Path> {
        self.source
            .as_deref()
            .context("source not configured — run: skiller source <path>")
    }
}
