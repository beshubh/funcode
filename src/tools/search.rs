use super::{
    AgentTool, ToolDisplay, ToolExecutionContext, ToolFailure, ToolInvocation, ToolResult, ToolSpec,
};
use crate::{session::SessionMode, workspace::WorkspacePath};
use futures::future::BoxFuture;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::Path};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT: usize = 10;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    Path,
    Content,
}

#[derive(Debug, Deserialize)]
struct SearchFilesArgs {
    mode: SearchMode,
    query: String,
    scope: Option<String>,
    glob: Option<String>,
    context_lines: Option<usize>,
    limit: Option<usize>,
}

pub(super) struct SearchFilesTool;

impl AgentTool for SearchFilesTool {
    fn spec(&self, _mode: SessionMode) -> ToolSpec {
        ToolSpec {
            name: "search_files",
            description: "Search workspace-relative paths or UTF-8 file contents with a regular expression. Honors Git ignore files and skips generated directories.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["path", "content"] },
                    "query": { "type": "string", "description": "Rust regular expression" },
                    "scope": { "type": "string", "description": "Optional workspace-relative file or directory" },
                    "glob": { "type": "string", "description": "Optional glob such as **/*.rs" },
                    "context_lines": { "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT }
                },
                "required": ["mode", "query"],
                "additionalProperties": false
            }),
        }
    }

    fn invocation(&self, arguments: &str) -> ToolInvocation {
        serde_json::from_str::<SearchFilesArgs>(arguments)
            .map(|args| ToolInvocation {
                summary: format!("Searching for {}", args.query),
                display: None,
            })
            .unwrap_or_else(|_| ToolInvocation {
                summary: "Searching workspace files".into(),
                display: None,
            })
    }

    fn execute(
        &self,
        arguments: String,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || search_files(arguments, context))
                .await
                .map_err(|error| {
                    ToolFailure::infrastructure(format!("search worker failed: {error}"))
                })?
        })
    }
}

fn search_files(
    arguments: String,
    context: ToolExecutionContext,
) -> Result<ToolResult, ToolFailure> {
    let args: SearchFilesArgs = serde_json::from_str(&arguments)
        .map_err(|error| ToolFailure::new(format!("invalid search_files arguments: {error}")))?;
    let expression = Regex::new(&args.query)
        .map_err(|error| ToolFailure::new(format!("invalid search regex: {error}")))?;
    let scope = context.workspace().existing_scope(args.scope.as_deref())?;
    let glob = build_glob(args.glob.as_deref())?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let context_lines = args.context_lines.unwrap_or(0).min(MAX_CONTEXT);
    let mut matches = Vec::new();
    let mut walker = WalkBuilder::new(&scope);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_str()
                .is_some_and(|name| matches!(name, ".git" | "node_modules" | "target"))
        });
    for entry in walker.build() {
        let entry = entry.map_err(|error| {
            ToolFailure::infrastructure(format!("could not walk the search scope: {error}"))
        })?;
        if matches.len() >= limit || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = context.workspace().relative(entry.path());
        if glob.as_ref().is_some_and(|glob| !glob.is_match(&relative)) {
            continue;
        }
        match args.mode {
            SearchMode::Path => {
                if expression.is_match(&relative) {
                    matches.push(WorkspacePath::from_raw(relative).display());
                }
            }
            SearchMode::Content => search_content(
                entry.path(),
                &relative,
                &expression,
                context_lines,
                limit,
                &mut matches,
            )?,
        }
    }
    let match_count = matches.len();
    let output = if matches.is_empty() {
        "No matches found.".to_owned()
    } else {
        matches.join("\n--\n")
    };
    Ok(ToolResult {
        output: output.clone(),
        display: ToolDisplay::SearchResults {
            query: args.query,
            matches: output,
        },
        summary: Some(format!("Found {match_count} match(es)")),
    })
}

fn build_glob(pattern: Option<&str>) -> Result<Option<GlobSet>, ToolFailure> {
    let Some(pattern) = pattern.filter(|pattern| !pattern.is_empty()) else {
        return Ok(None);
    };
    let mut builder = GlobSetBuilder::new();
    builder.add(
        Glob::new(pattern)
            .map_err(|error| ToolFailure::new(format!("invalid search glob: {error}")))?,
    );
    builder
        .build()
        .map(Some)
        .map_err(|error| ToolFailure::new(format!("invalid search glob: {error}")))
}

fn search_content(
    path: &Path,
    relative: &str,
    expression: &Regex,
    context_lines: usize,
    limit: usize,
    matches: &mut Vec<String>,
) -> Result<(), ToolFailure> {
    let display_relative = WorkspacePath::from_raw(relative).display();
    let metadata = fs::metadata(path).map_err(|error| {
        ToolFailure::infrastructure(format!("could not inspect '{display_relative}': {error}"))
    })?;
    if metadata.len() > MAX_SEARCH_FILE_BYTES {
        return Ok(());
    }
    let bytes = fs::read(path).map_err(|error| {
        ToolFailure::infrastructure(format!("could not read '{display_relative}': {error}"))
    })?;
    if bytes.contains(&0) {
        return Ok(());
    }
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(());
    };
    let lines = content.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if !expression.is_match(line) {
            continue;
        }
        let from = index.saturating_sub(context_lines);
        let to = (index + context_lines + 1).min(lines.len());
        let snippet = lines[from..to]
            .iter()
            .enumerate()
            .map(|(context_index, context)| {
                let line_number = from + context_index + 1;
                let marker = if line_number == index + 1 { ':' } else { '-' };
                format!("{display_relative}{marker}{line_number}{marker}{context}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        matches.push(snippet);
        if matches.len() >= limit {
            return Ok(());
        }
    }
    Ok(())
}
