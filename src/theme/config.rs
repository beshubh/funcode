use super::ThemeId;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct ThemeConfig {
    #[serde(default)]
    pub theme: ThemeId,
}

#[derive(Debug, Clone)]
pub(crate) struct ThemeConfigStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemeConfigLoad {
    pub config: ThemeConfig,
    pub warning: Option<String>,
}

impl ThemeConfigStore {
    pub(crate) fn standard() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
        Ok(Self::at(home.join(".funcode/config.json")))
    }

    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> Result<ThemeConfig, String> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ThemeConfig::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub(crate) fn load_or_default(&self) -> ThemeConfigLoad {
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

    fn save(&self, config: ThemeConfig) -> Result<(), String> {
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

enum ThemeConfigCommand {
    Save(ThemeId),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeConfigEvent {
    Saved(ThemeId),
    Failed(String),
}

pub(crate) struct ThemeConfigTaskRunner {
    commands: Sender<ThemeConfigCommand>,
    events: Receiver<ThemeConfigEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ThemeConfigTaskRunner {
    pub(crate) fn spawn(store: ThemeConfigStore) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            while let Ok(command) = command_rx.recv() {
                match command {
                    ThemeConfigCommand::Save(theme) => {
                        let event = match store.save(ThemeConfig { theme }) {
                            Ok(()) => ThemeConfigEvent::Saved(theme),
                            Err(error) => ThemeConfigEvent::Failed(error),
                        };
                        let _ = event_tx.send(event);
                    }
                    ThemeConfigCommand::Shutdown => break,
                }
            }
        });
        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub(crate) fn save(&self, theme: ThemeId) -> Result<(), String> {
        self.commands
            .send(ThemeConfigCommand::Save(theme))
            .map_err(|_| "theme configuration worker stopped".to_owned())
    }

    pub(crate) fn try_event(&self) -> Option<ThemeConfigEvent> {
        self.events.try_recv().ok()
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.commands.send(ThemeConfigCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ThemeConfigTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
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

        fs::write(directory.join("config.json"), b"{not-json").unwrap();
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
