use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Layout {
    #[serde(default = "default_session_name")]
    pub session_name: String,
    #[serde(default)]
    pub modules: Vec<ModuleDef>,
}

fn default_session_name() -> String {
    "los".into()
}

#[derive(Debug, Deserialize)]
pub struct ModuleDef {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "default_count")]
    pub count: usize,
    #[serde(default)]
    pub command: Option<String>,
}

fn default_count() -> usize {
    1
}

impl Layout {
    pub fn load() -> Result<Self> {
        let config_dir = config_dir();

        let paths: [PathBuf; 4] = [
            PathBuf::from("los.toml"),
            config_dir.join("layout.toml"),
            config_dir.join("los.toml"),
            dirs_home_dir().join(".los.toml"),
        ];

        for path in &paths {
            if path.exists() {
                let data = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config: {}", path.display()))?;
                return toml::from_str(&data)
                    .with_context(|| format!("parsing config: {}", path.display()));
            }
        }

        Ok(Self::default())
    }

    pub fn total_modules(&self) -> usize {
        self.modules.iter().map(|m| m.count).sum()
    }
}

fn config_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(val).join("los")
    } else {
        dirs_home_dir().join(".config").join("los")
    }
}

fn dirs_home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            session_name: "los".into(),
            modules: vec![
                ModuleDef {
                    kind: "conductor".into(),
                    count: 1,
                    command: None,
                },
                ModuleDef {
                    kind: "sequencer".into(),
                    count: 1,
                    command: None,
                },
                ModuleDef {
                    kind: "mixer".into(),
                    count: 1,
                    command: None,
                },
                ModuleDef {
                    kind: "voice".into(),
                    count: 4,
                    command: None,
                },
                ModuleDef {
                    kind: "scope".into(),
                    count: 1,
                    command: None,
                },
                ModuleDef {
                    kind: "envelope".into(),
                    count: 1,
                    command: None,
                },
            ],
        }
    }
}
