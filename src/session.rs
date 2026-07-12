#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    Plan,
    #[default]
    Build,
}

impl SessionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Build => "Build",
        }
    }

    pub(crate) fn apply_to_prompt(self, prompt: String) -> String {
        match self {
            Self::Build => prompt,
            Self::Plan => format!(
                "Plan mode is active. Produce a decision-complete implementation plan and do not modify files. You may use read_file and search_files, plus terminal only for non-mutating inspection commands.\n\n{prompt}"
            ),
        }
    }
}
