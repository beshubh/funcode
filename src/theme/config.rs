use super::ThemeId;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ThemeConfig {
    #[serde(default)]
    pub theme: ThemeId,
}

#[derive(Debug, Clone)]
pub struct ThemeConfigStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeConfigLoad {
    pub config: ThemeConfig,
    pub warning: Option<String>,
}

impl ThemeConfigStore {
    pub fn standard() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
        Ok(Self::at(home.join(".funcode/config.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<ThemeConfig, String> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ThemeConfig::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn load_or_default(&self) -> ThemeConfigLoad {
        match self.load() {
            Ok(config) => ThemeConfigLoad {
                config,
                warning: None,
            },
            Err(error) => ThemeConfigLoad {
                config: ThemeConfig::default(),
                warning: Some(format!(
                    "Could not load theme configuration ({error}); using terminal"
                )),
            },
        }
    }

    pub fn save(&self, config: ThemeConfig) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "theme config path has no parent".to_owned())?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{ThemeConfig, ThemeConfigStore};
    use crate::theme::ThemeId;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn store() -> (std::path::PathBuf, ThemeConfigStore) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("funcode-theme-{unique}"));
        let path = directory.join("config.json");
        (directory, ThemeConfigStore::at(path))
    }

    #[test]
    fn configuration_falls_back_safely_and_round_trips_atomically() {
        let (directory, store) = store();

        let missing = store.load_or_default();
        assert_eq!(missing.config.theme, ThemeId::Terminal);
        assert!(missing.warning.is_none());

        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("config.json"), br#"{"theme":"unknown"}"#).unwrap();
        let malformed = store.load_or_default();
        assert_eq!(malformed.config.theme, ThemeId::Terminal);
        assert!(malformed.warning.as_deref().unwrap().contains("terminal"));

        store
            .save(ThemeConfig {
                theme: ThemeId::Paper,
            })
            .unwrap();
        let loaded = store.load_or_default();
        assert_eq!(loaded.config.theme, ThemeId::Paper);
        assert!(loaded.warning.is_none());
        assert!(!directory.join("config.json.tmp").exists());

        fs::remove_dir_all(directory).unwrap();
    }
}
