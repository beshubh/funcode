use crossbeam::channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, unbounded};
use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};
use std::{
    fs,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INDEX_READY_TIMEOUT: Duration = Duration::from_secs(10);
const INDEX_READY_POLL: Duration = Duration::from_millis(25);
const INDEX_RESCAN_INTERVAL: Duration = Duration::from_secs(2);

// TODO: this probably needs a modification we should rather ignore everything that's there in the
// .gitignore file
const IGNORED_DIRECTORIES: &[&str] = &[".git", "node_modules", "target"];

#[derive(Debug)]
pub(crate) enum WorkspaceSearch {
    Fixed(Vec<String>),
    Fff {
        picker: SharedFilePicker,
        frecency: SharedFrecency,
    },
}

impl WorkspaceSearch {
    pub(crate) fn from_files<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut files = files.into_iter().map(Into::into).collect::<Vec<_>>();
        files.sort();
        Self::Fixed(files)
    }

    pub(crate) fn start(root: &Path) -> Result<Self, fff_search::Error> {
        let picker = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                mode: FFFMode::Ai,
                enable_mmap_cache: false,
                enable_content_indexing: false,
                watch: true,
                ..Default::default()
            },
        )?;
        Ok(Self::Fff { picker, frecency })
    }

    pub(crate) fn suggestions(&self, query: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        match self {
            Self::Fixed(files) => {
                let query = query.to_lowercase();
                files
                    .iter()
                    .filter(|path| path.to_lowercase().contains(&query))
                    .take(limit)
                    .cloned()
                    .collect()
            }
            Self::Fff { picker, .. } => {
                let Ok(guard) = picker.read() else {
                    return Vec::new();
                };
                let Some(picker) = guard.as_ref() else {
                    return Vec::new();
                };
                let query = QueryParser::default().parse(query);
                picker
                    .fuzzy_search(
                        &query,
                        None,
                        FuzzySearchOptions {
                            pagination: PaginationArgs { offset: 0, limit },
                            ..Default::default()
                        },
                    )
                    .items
                    .iter()
                    .map(|item| normalize_relative_path(&item.relative_path(picker)))
                    .collect()
            }
        }
    }

    fn wait_until_ready(&self, timeout: Duration) -> bool {
        match self {
            Self::Fixed(_) => true,
            Self::Fff { picker, .. } => {
                picker.wait_for_scan(timeout) && picker.wait_for_watcher(timeout)
            }
        }
    }

    fn refresh(&self) {
        if let Self::Fff { picker, frecency } = self {
            let _ = picker.trigger_full_rescan_async(frecency);
        }
    }
}

#[derive(Debug)]
enum WorkspaceCommand {
    Search { query: String, limit: usize },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceEvent {
    Ready { warning: Option<String> },
    Suggestions { query: String, paths: Vec<String> },
}

pub struct WorkspaceTaskRunner {
    commands: Sender<WorkspaceCommand>,
    events: Receiver<WorkspaceEvent>,
    _worker: JoinHandle<()>,
}

impl WorkspaceTaskRunner {
    pub fn spawn(root: PathBuf) -> Self {
        let fallback_root = root.clone();
        Self::spawn_with(
            move || WorkspaceSearch::start(&root).map_err(|error| error.to_string()),
            move || discover_files(&fallback_root),
        )
    }

    fn spawn_with(
        load: impl FnOnce() -> Result<WorkspaceSearch, String> + Send + 'static,
        fallback: impl FnOnce() -> Vec<String> + Send + 'static,
    ) -> Self {
        let (commands, command_rx) = bounded(1);
        let (event_tx, events) = unbounded();
        let worker = thread::spawn(move || {
            let (search, mut warning) = match load() {
                Ok(search) => (search, None),
                Err(error) => (
                    WorkspaceSearch::from_files(fallback()),
                    Some(format!(
                        "FFF file index could not start ({error}); using basic file matching"
                    )),
                ),
            };
            let ready_deadline = Instant::now() + INDEX_READY_TIMEOUT;
            let mut pending_search = None;
            let mut last_rescan = Instant::now()
                .checked_sub(INDEX_RESCAN_INTERVAL)
                .unwrap_or_else(Instant::now);
            while !search.wait_until_ready(INDEX_READY_POLL) {
                match command_rx.try_recv() {
                    Ok(WorkspaceCommand::Search { query, limit }) => {
                        pending_search = Some((query, limit));
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return,
                }
                if Instant::now() >= ready_deadline {
                    warning.get_or_insert_with(|| {
                        "FFF file index is still warming up; suggestions may be incomplete".into()
                    });
                    break;
                }
            }
            if event_tx.send(WorkspaceEvent::Ready { warning }).is_err() {
                return;
            }
            if let Some((query, limit)) = pending_search
                && !send_suggestions(&search, &event_tx, query, limit, &mut last_rescan)
            {
                return;
            }
            while let Ok(command) = command_rx.recv() {
                match command {
                    WorkspaceCommand::Search { query, limit } => {
                        if !send_suggestions(&search, &event_tx, query, limit, &mut last_rescan) {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            commands,
            events,
            _worker: worker,
        }
    }

    pub(crate) fn request_suggestions(&self, query: String, limit: usize) -> bool {
        match self
            .commands
            .try_send(WorkspaceCommand::Search { query, limit })
        {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub(crate) fn try_event(&self) -> Option<WorkspaceEvent> {
        self.events.try_recv().ok()
    }
}

fn normalize_relative_path(path: &str) -> String {
    Path::new(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn send_suggestions(
    search: &WorkspaceSearch,
    events: &Sender<WorkspaceEvent>,
    query: String,
    limit: usize,
    last_rescan: &mut Instant,
) -> bool {
    if last_rescan.elapsed() >= INDEX_RESCAN_INTERVAL {
        search.refresh();
        *last_rescan = Instant::now();
    }
    let paths = search.suggestions(&query, limit);
    events
        .send(WorkspaceEvent::Suggestions { query, paths })
        .is_ok()
}

pub fn discover_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort();
    files
}

fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        // TODO:  we should give feedback to the user that we are not able to open a directory, with
        // the path and any other context that might help the user
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            // TODO: again this can error out here, we need to handle that instead of failing silently
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
        // TODO: this path.strip_prefix() will fail silently
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
    use super::{
        WorkspaceEvent, WorkspaceSearch, WorkspaceTaskRunner, discover_files,
        normalize_relative_path,
    };
    use crossbeam::channel::unbounded;
    use std::{
        fs, thread,
        time::{Duration, Instant, SystemTime},
    };

    fn receive_suggestions(
        runner: &WorkspaceTaskRunner,
        query: &str,
        timeout: Duration,
    ) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let _ = runner.request_suggestions(query.to_owned(), 8);
            if let Ok(WorkspaceEvent::Suggestions { paths, .. }) =
                runner.events.recv_timeout(Duration::from_millis(50))
                && !paths.is_empty()
            {
                return paths;
            }
            assert!(
                Instant::now() < deadline,
                "suggestions did not become ready"
            );
        }
    }

    #[test]
    fn workspace_discovery_runs_without_blocking_the_caller() {
        let (release_tx, release_rx) = unbounded();
        let runner = WorkspaceTaskRunner::spawn_with(
            move || {
                release_rx
                    .recv()
                    .expect("test should release workspace discovery");
                Ok(WorkspaceSearch::from_files(["src/main.rs"]))
            },
            Vec::new,
        );

        assert!(runner.try_event().is_none());
        release_tx
            .send(())
            .expect("workspace discovery worker should still be running");
        assert_eq!(
            runner
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("workspace discovery should report readiness"),
            WorkspaceEvent::Ready { warning: None }
        );
        assert_eq!(
            receive_suggestions(&runner, "main", Duration::from_secs(1)),
            ["src/main.rs"]
        );
    }

    #[test]
    fn workspace_task_runner_builds_a_typo_tolerant_fff_index() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::create_dir_all(root.path().join("src"))
            .expect("workspace source directory should be created");
        fs::write(root.path().join("src/main.rs"), "fn main() {}")
            .expect("main source fixture should be written");
        fs::write(root.path().join("src/runtime.rs"), "")
            .expect("runtime source fixture should be written");

        let runner = WorkspaceTaskRunner::spawn(root.path().to_path_buf());
        assert_eq!(
            runner
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("FFF index should report readiness"),
            WorkspaceEvent::Ready { warning: None }
        );
        assert_eq!(
            receive_suggestions(&runner, "src/maim", Duration::from_secs(2))[0],
            "src/main.rs"
        );
    }

    #[test]
    fn workspace_task_runner_falls_back_and_reports_fff_startup_failure() {
        let runner = WorkspaceTaskRunner::spawn_with(
            || Err("index unavailable".into()),
            || vec!["src/main.rs".into()],
        );

        let WorkspaceEvent::Ready { warning } = runner
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("fallback index should report readiness")
        else {
            panic!("runner should report readiness before suggestions");
        };
        assert!(
            warning
                .expect("FFF startup failure should produce a warning")
                .contains("index unavailable")
        );
        assert_eq!(
            receive_suggestions(&runner, "main", Duration::from_secs(1)),
            ["src/main.rs"]
        );
    }

    #[test]
    fn long_lived_fff_index_makes_new_files_searchable_without_restarting() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::create_dir_all(root.path().join("src"))
            .expect("workspace source directory should be created");
        fs::write(root.path().join("src/main.rs"), "")
            .expect("initial source fixture should be written");
        let runner = WorkspaceTaskRunner::spawn(root.path().to_path_buf());
        assert!(matches!(
            runner
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("FFF index should report readiness"),
            WorkspaceEvent::Ready { warning: None }
        ));
        let _ = receive_suggestions(&runner, "main", Duration::from_secs(2));

        fs::write(root.path().join("src/new_file.rs"), "")
            .expect("new source fixture should be written");

        assert_eq!(
            receive_suggestions(&runner, "new_file", Duration::from_secs(3))[0],
            "src/new_file.rs"
        );
    }

    #[test]
    fn dropping_workspace_runner_never_waits_for_blocked_work() {
        let (release_tx, release_rx) = unbounded();
        let runner = WorkspaceTaskRunner::spawn_with(
            move || {
                let _ = release_rx.recv();
                Ok(WorkspaceSearch::from_files(["src/main.rs"]))
            },
            Vec::new,
        );
        let (dropped_tx, dropped_rx) = unbounded();
        let drop_thread = thread::spawn(move || {
            drop(runner);
            let _ = dropped_tx.send(());
        });

        let dropped_without_waiting = dropped_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        if !dropped_without_waiting {
            let _ = release_tx.send(());
        }
        let _ = drop_thread.join();

        assert!(
            dropped_without_waiting,
            "dropping the runner waited for blocked workspace work"
        );
    }

    #[test]
    fn fff_paths_are_normalized_for_inline_attachment_tokens() {
        assert_eq!(normalize_relative_path("src/main.rs"), "src/main.rs");
        #[cfg(windows)]
        assert_eq!(normalize_relative_path(r"src\main.rs"), "src/main.rs");
        #[cfg(not(windows))]
        assert_eq!(normalize_relative_path(r"src\main.rs"), r"src\main.rs");
    }

    #[test]
    fn discovery_returns_relative_files_and_skips_build_directories() {
        let root = std::env::temp_dir().join(format!(
            "funcode-files-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("source fixture should be created");
        fs::create_dir_all(root.join("target/debug"))
            .expect("build output fixture should be created");
        fs::write(root.join("Cargo.toml"), "").expect("manifest fixture should be written");
        fs::write(root.join("src/main.rs"), "").expect("source fixture should be written");
        fs::write(root.join("target/debug/ignored"), "")
            .expect("ignored build fixture should be written");

        assert_eq!(discover_files(&root), ["Cargo.toml", "src/main.rs"]);

        fs::remove_dir_all(root).expect("discovery fixture should be removed");
    }
}
