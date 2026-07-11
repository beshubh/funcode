use std::{
    fmt, fs,
    path::{Component, Path, PathBuf},
};

const MAX_ATTACHMENT_BYTES: u64 = 256 * 1024;
const PREVIEW_CHAR_LIMIT: usize = 2_000;

#[derive(Debug, Clone)]
pub struct WorkspaceFileReader {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    pub path: String,
    pub content: String,
    pub line_count: u32,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadError(String);

impl fmt::Display for FileReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FileReadError {}

impl WorkspaceFileReader {
    pub fn from_current_dir() -> Result<Self, FileReadError> {
        let root = std::env::current_dir()
            .map_err(|error| FileReadError(format!("could not locate the workspace: {error}")))?;
        Self::new(root)
    }

    pub fn new(root: PathBuf) -> Result<Self, FileReadError> {
        let root = fs::canonicalize(root)
            .map_err(|error| FileReadError(format!("could not access the workspace: {error}")))?;
        Ok(Self { root })
    }

    pub fn read(&self, path: &str) -> Result<ReadFile, FileReadError> {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(FileReadError(
                "the attached path is outside the workspace".into(),
            ));
        }

        let resolved = fs::canonicalize(self.root.join(relative)).map_err(|error| {
            FileReadError(format!("could not read attached file '{path}': {error}"))
        })?;
        if !resolved.starts_with(&self.root) {
            return Err(FileReadError(
                "the attached path resolves outside the workspace".into(),
            ));
        }

        let metadata = fs::metadata(&resolved).map_err(|error| {
            FileReadError(format!("could not inspect attached file '{path}': {error}"))
        })?;
        if !metadata.is_file() {
            return Err(FileReadError(format!(
                "attached path '{path}' is not a file"
            )));
        }
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            return Err(FileReadError(format!(
                "attached file '{path}' exceeds the {} KiB limit",
                MAX_ATTACHMENT_BYTES / 1024
            )));
        }

        let content = fs::read_to_string(&resolved).map_err(|error| {
            FileReadError(format!(
                "could not read attached file '{path}' as UTF-8 text: {error}"
            ))
        })?;
        let line_count = content.lines().count().max(1) as u32;
        let mut preview = content.chars().take(PREVIEW_CHAR_LIMIT).collect::<String>();
        if content.chars().count() > PREVIEW_CHAR_LIMIT {
            preview.push_str("\n…");
        }

        Ok(ReadFile {
            path: path.to_owned(),
            content,
            line_count,
            preview,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceFileReader;
    use std::{fs, time::SystemTime};

    fn workspace() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "funcode-tools-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        root
    }

    #[test]
    fn reads_a_text_file_inside_the_workspace_with_a_bounded_preview() {
        let root = workspace();
        let reader = WorkspaceFileReader::new(root.clone()).unwrap();

        let file = reader.read("src/main.rs").unwrap();

        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.content, "fn main() {}\n");
        assert_eq!(file.line_count, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_paths_that_escape_the_workspace() {
        let root = workspace();
        let reader = WorkspaceFileReader::new(root.clone()).unwrap();

        let error = reader.read("../outside.rs").unwrap_err();

        assert!(error.to_string().contains("outside the workspace"));
        let _ = fs::remove_dir_all(root);
    }
}
