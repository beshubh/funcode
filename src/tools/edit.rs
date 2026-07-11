use super::{
    AgentTool, ToolAvailability, ToolDisplay, ToolExecutionContext, ToolFailure, ToolInvocation,
    ToolResult, ToolSpec,
};
use crate::composer::SessionMode;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use similar::TextDiff;
use std::fs;

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum EditFileArgs {
    Replace {
        path: String,
        replacements: Vec<Replacement>,
    },
    Create {
        path: String,
        content: String,
    },
}

#[derive(Debug, Deserialize)]
struct Replacement {
    old_text: String,
    new_text: String,
}

pub(super) struct EditFileTool;

impl AgentTool for EditFileTool {
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::BuildOnly
    }

    fn spec(&self, _mode: SessionMode) -> ToolSpec {
        ToolSpec {
            name: "edit_file",
            description: "Atomically replace exact text in an existing workspace file or create a new file. Every old_text must occur exactly once.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["replace", "create"] },
                    "path": { "type": "string" },
                    "replacements": {
                        "type": "array",
                        "description": "Required for replace; omit for create",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": { "type": "string", "minLength": 1 },
                                "new_text": { "type": "string" }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    },
                    "content": {
                        "type": "string",
                        "description": "Required for create; omit for replace"
                    }
                },
                "required": ["operation", "path"],
                "additionalProperties": false
            }),
        }
    }

    fn invocation(&self, arguments: &str) -> ToolInvocation {
        let path = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("path")?.as_str().map(str::to_owned));
        ToolInvocation {
            summary: path
                .map(|path| format!("Editing {path}"))
                .unwrap_or_else(|| "Editing a workspace file".into()),
            display: None,
        }
    }

    fn execute(
        &self,
        arguments: String,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let args: EditFileArgs = serde_json::from_str(&arguments).map_err(|error| {
                    ToolFailure::new(format!("invalid edit_file arguments: {error}"))
                })?;
                match args {
                    EditFileArgs::Replace { path, replacements } => {
                        replace_file(path, replacements, context)
                    }
                    EditFileArgs::Create { path, content } => create_file(path, content, context),
                }
            })
            .await
            .map_err(|error| ToolFailure::infrastructure(format!("edit worker failed: {error}")))?
        })
    }
}

fn replace_file(
    path: String,
    replacements: Vec<Replacement>,
    context: ToolExecutionContext,
) -> Result<ToolResult, ToolFailure> {
    if replacements.is_empty() {
        return Err(ToolFailure::new("at least one replacement is required"));
    }
    let resolved = context.workspace().existing_file(&path)?;
    let metadata = fs::metadata(&resolved)
        .map_err(|error| ToolFailure::new(format!("could not inspect '{path}': {error}")))?;
    let original = fs::read_to_string(&resolved)
        .map_err(|error| ToolFailure::new(format!("could not read '{path}' as UTF-8: {error}")))?;
    let mut ranges = Vec::with_capacity(replacements.len());
    for replacement in &replacements {
        if replacement.old_text.is_empty() {
            return Err(ToolFailure::new("old_text must not be empty"));
        }
        let occurrences = original
            .match_indices(&replacement.old_text)
            .collect::<Vec<_>>();
        if occurrences.len() != 1 {
            return Err(ToolFailure::new(format!(
                "old_text must occur exactly once in '{path}', but occurred {} time(s)",
                occurrences.len()
            )));
        }
        let start = occurrences[0].0;
        ranges.push((
            start,
            start + replacement.old_text.len(),
            &replacement.new_text,
        ));
    }
    ranges.sort_by_key(|range| range.0);
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ToolFailure::new("replacement ranges overlap"));
    }
    let mut edited = String::with_capacity(original.len());
    let mut cursor = 0;
    for (start, end, new_text) in ranges {
        edited.push_str(&original[cursor..start]);
        edited.push_str(new_text);
        cursor = end;
    }
    edited.push_str(&original[cursor..]);
    let diff = unified_diff(&path, &path, &original, &edited);
    context
        .workspace()
        .write_atomic(&resolved, &edited, Some(metadata.permissions()))?;
    Ok(edit_result(path, diff))
}

fn create_file(
    path: String,
    content: String,
    context: ToolExecutionContext,
) -> Result<ToolResult, ToolFailure> {
    let resolved = context.workspace().new_file(&path)?;
    let diff = unified_diff("/dev/null", &path, "", &content);
    context
        .workspace()
        .write_atomic(&resolved, &content, None)?;
    Ok(edit_result(path, diff))
}

fn edit_result(path: String, diff: String) -> ToolResult {
    ToolResult {
        output: diff.clone(),
        display: ToolDisplay::Patch {
            path: path.clone(),
            diff,
        },
        summary: Some(format!("Edited {path}")),
    }
}

fn unified_diff(old_path: &str, new_path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_path, new_path)
        .to_string()
}
