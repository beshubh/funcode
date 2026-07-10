use std::{fs, path::Path};

const IGNORED_DIRECTORIES: &[&str] = &[".git", "node_modules", "target"];

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
    use super::discover_files;
    use std::{fs, time::SystemTime};

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
