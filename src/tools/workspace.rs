use super::ToolFailure;
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const MAX_ATTACHMENT_BYTES: u64 = 256 * 1024;
const PREVIEW_CHAR_LIMIT: usize = 2_000;

#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub(crate) fn new(root: PathBuf) -> Result<Self, ToolFailure> {
        let root = fs::canonicalize(root).map_err(|error| {
            ToolFailure::new(format!("could not access the workspace: {error}"))
        })?;
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn existing_file(&self, path: &str) -> Result<PathBuf, ToolFailure> {
        let relative = safe_relative(path)?;
        let resolved = fs::canonicalize(self.root.join(relative)).map_err(|error| {
            ToolFailure::new(format!("could not access workspace file '{path}': {error}"))
        })?;
        if !resolved.starts_with(&self.root) {
            return Err(ToolFailure::new("the path resolves outside the workspace"));
        }
        if !resolved.is_file() {
            return Err(ToolFailure::new(format!("'{path}' is not a file")));
        }
        Ok(resolved)
    }

    pub(crate) fn existing_scope(&self, path: Option<&str>) -> Result<PathBuf, ToolFailure> {
        let Some(path) = path.filter(|path| !path.trim().is_empty()) else {
            return Ok(self.root.clone());
        };
        let relative = safe_relative(path)?;
        let resolved = fs::canonicalize(self.root.join(relative)).map_err(|error| {
            ToolFailure::new(format!("could not access search scope '{path}': {error}"))
        })?;
        if !resolved.starts_with(&self.root) {
            return Err(ToolFailure::new(
                "the search scope resolves outside the workspace",
            ));
        }
        Ok(resolved)
    }

    pub(crate) fn new_file(&self, path: &str) -> Result<PathBuf, ToolFailure> {
        let relative = safe_relative(path)?;
        let target = self.root.join(relative);
        if target.exists() {
            return Err(ToolFailure::new(format!("'{path}' already exists")));
        }
        let mut ancestor = target.parent().ok_or_else(|| {
            ToolFailure::new(format!("could not determine the parent of '{path}'"))
        })?;
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(|| {
                ToolFailure::new(format!("could not validate the parent of '{path}'"))
            })?;
        }
        let ancestor = fs::canonicalize(ancestor).map_err(|error| {
            ToolFailure::new(format!(
                "could not validate the parent of '{path}': {error}"
            ))
        })?;
        if !ancestor.starts_with(&self.root) {
            return Err(ToolFailure::new(
                "the new path resolves outside the workspace",
            ));
        }
        Ok(target)
    }

    pub(crate) fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub(crate) fn write_atomic(
        &self,
        path: &Path,
        content: &str,
        permissions: Option<fs::Permissions>,
    ) -> Result<(), ToolFailure> {
        let parent = path
            .parent()
            .ok_or_else(|| ToolFailure::new("the destination has no parent directory"))?;
        fs::create_dir_all(parent).map_err(|error| {
            ToolFailure::infrastructure(format!(
                "could not create the destination directory: {error}"
            ))
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            ToolFailure::infrastructure(format!("could not create a temporary edit file: {error}"))
        })?;
        temporary.write_all(content.as_bytes()).map_err(|error| {
            ToolFailure::infrastructure(format!("could not write the edited file: {error}"))
        })?;
        temporary.as_file().sync_all().map_err(|error| {
            ToolFailure::infrastructure(format!("could not flush the edited file: {error}"))
        })?;
        if let Some(permissions) = permissions {
            temporary
                .as_file()
                .set_permissions(permissions)
                .map_err(|error| {
                    ToolFailure::infrastructure(format!(
                        "could not preserve file permissions: {error}"
                    ))
                })?;
        }
        temporary.persist(path).map_err(|error| {
            ToolFailure::infrastructure(format!(
                "could not replace the edited file: {}",
                error.error
            ))
        })?;
        Ok(())
    }
}

fn safe_relative(path: &str) -> Result<&Path, ToolFailure> {
    let relative = Path::new(path);
    if path.trim().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolFailure::new("the path is outside the workspace"));
    }
    Ok(relative)
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

impl std::fmt::Display for FileReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FileReadError {}

#[derive(Debug, Clone)]
pub struct WorkspaceFileReader {
    workspace: Workspace,
}

impl WorkspaceFileReader {
    pub fn from_current_dir() -> Result<Self, FileReadError> {
        let root = std::env::current_dir()
            .map_err(|error| FileReadError(format!("could not locate the workspace: {error}")))?;
        Self::new(root)
    }

    pub fn new(root: PathBuf) -> Result<Self, FileReadError> {
        Workspace::new(root)
            .map(|workspace| Self { workspace })
            .map_err(|error| FileReadError(error.to_string()))
    }

    pub fn read(&self, path: &str) -> Result<ReadFile, FileReadError> {
        let resolved = self
            .workspace
            .existing_file(path)
            .map_err(|error| FileReadError(error.to_string()))?;
        let metadata = fs::metadata(&resolved).map_err(|error| {
            FileReadError(format!("could not inspect attached file '{path}': {error}"))
        })?;
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

    pub(crate) fn root(&self) -> PathBuf {
        self.workspace.root().to_owned()
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
                .expect("clock should be after the Unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("workspace should be created");
        fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("fixture should be written");
        root
    }

    #[test]
    fn reads_a_text_file_inside_the_workspace_with_a_bounded_preview() {
        let root = workspace();
        let reader = WorkspaceFileReader::new(root.clone()).expect("reader should be created");
        let file = reader.read("src/main.rs").expect("file should be read");
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.content, "fn main() {}\n");
        assert_eq!(file.line_count, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_paths_that_escape_the_workspace() {
        let root = workspace();
        let reader = WorkspaceFileReader::new(root.clone()).expect("reader should be created");
        let error = reader
            .read("../outside.rs")
            .expect_err("escaping path should fail");
        assert!(error.to_string().contains("outside the workspace"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let root = workspace();
        let outside = tempfile::NamedTempFile::new().expect("outside fixture should be created");
        symlink(outside.path(), root.join("escape.txt")).expect("symlink should be created");
        let reader = WorkspaceFileReader::new(root.clone()).expect("reader should be created");

        let error = reader
            .read("escape.txt")
            .expect_err("escaping symlink should fail");

        assert!(error.to_string().contains("outside the workspace"));
        let _ = fs::remove_dir_all(root);
    }
}
