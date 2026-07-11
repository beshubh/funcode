use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

const IGNORED_DIRECTORIES: &[&str] = &[".git", "node_modules", "target"];

pub struct WorkspaceTaskRunner {
    files: Receiver<Vec<String>>,
}

impl WorkspaceTaskRunner {
    pub fn spawn(root: PathBuf) -> Self {
        Self::spawn_with(move || discover_files(&root))
    }

    fn spawn_with(discover: impl FnOnce() -> Vec<String> + Send + 'static) -> Self {
        let (files_tx, files) = mpsc::channel();
        thread::spawn(move || {
            let _ = files_tx.send(discover());
        });
        Self { files }
    }

    pub fn try_files(&self) -> Option<Vec<String>> {
        self.files.try_recv().ok()
    }
}

pub fn discover_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort();
    files
}

fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let ignored = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| IGNORED_DIRECTORIES.contains(&name));
            if !ignored {
                visit(root, &path, files);
            }
        } else if file_type.is_file()
            && let Ok(relative) = path.strip_prefix(root)
        {
            files.push(
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceTaskRunner, discover_files};
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, SystemTime},
    };

    #[test]
    fn workspace_discovery_runs_without_blocking_the_caller() {
        let (release_tx, release_rx) = mpsc::channel();
        let runner = WorkspaceTaskRunner::spawn_with(move || {
            release_rx.recv().unwrap();
            vec!["src/main.rs".to_owned()]
        });

        assert!(runner.try_files().is_none());
        release_tx.send(()).unwrap();
        assert_eq!(
            runner.files.recv_timeout(Duration::from_secs(1)).unwrap(),
            ["src/main.rs"]
        );
    }

    #[test]
    fn discovery_returns_relative_files_and_skips_build_directories() {
        let root = std::env::temp_dir().join(format!(
            "funcode-files-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("target/debug/ignored"), "").unwrap();

        assert_eq!(discover_files(&root), ["Cargo.toml", "src/main.rs"]);

        fs::remove_dir_all(root).unwrap();
    }
}
