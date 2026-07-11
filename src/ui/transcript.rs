use crate::{
    app::App,
    theme::{Theme, ThemeRole},
    transcript::{ActivityStatus, AssistantStatus, Entry, EntryId, EntryKind, ToolArtifact},
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryRegion {
    pub id: EntryId,
    pub area: Rect,
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
) -> Vec<EntryRegion> {
    let content_area = area.inner(Margin::new(2, 0));
    if app.transcript.entries().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No messages yet. Type something below to begin.",
                theme.style(ThemeRole::MutedText),
            )),
            content_area,
        );
        return Vec::new();
    }

    let width = content_area.width.max(1);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut next_line = 0usize;

    for entry in app.transcript.entries() {
        let entry_lines = entry_lines(entry, app, theme);
        let height = wrapped_height(&entry_lines, width);
        let start = next_line;
        next_line += height;
        let interactive = matches!(
            entry.kind,
            EntryKind::User(_) | EntryKind::Reasoning(_) | EntryKind::Tool(_)
        );
        if interactive {
            spans.push((entry.id, start, next_line));
        }
        lines.extend(entry_lines);
    }

    let viewport_area = if app.follow_output {
        content_area
    } else {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "↑ End to follow",
                theme.style(ThemeRole::MutedText),
            )),
            Rect::new(content_area.x, content_area.y, content_area.width, 1),
        );
        Rect::new(
            content_area.x,
            content_area.y.saturating_add(1),
            content_area.width,
            content_area.height.saturating_sub(1),
        )
    };
    let text = Text::from(lines);
    let line_count = wrapped_height(&text.lines, width);
    let viewport_height = viewport_area.height as usize;
    let maximum_top = line_count.saturating_sub(viewport_height);
    let from_bottom = app.scroll_from_bottom.min(maximum_top);
    let top = maximum_top.saturating_sub(from_bottom);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((top.min(u16::MAX as usize) as u16, 0)),
        viewport_area,
    );

    spans
        .into_iter()
        .filter_map(|(id, start, end)| visible_region(id, start, end, top, viewport_area))
        .collect()
}

fn visible_region(
    id: EntryId,
    start: usize,
    end: usize,
    top: usize,
    area: Rect,
) -> Option<EntryRegion> {
    let visible_start = start.max(top);
    let visible_end = end.min(top + area.height as usize);
    (visible_start < visible_end).then(|| EntryRegion {
        id,
        area: Rect::new(
            area.x,
            area.y + (visible_start - top).min(u16::MAX as usize) as u16,
            area.width,
            (visible_end - visible_start).min(u16::MAX as usize) as u16,
        ),
    })
}

fn entry_lines(entry: &Entry, app: &App, theme: &Theme) -> Vec<Line<'static>> {
    match &entry.kind {
        EntryKind::User(message) => {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    "┌─ you",
                    theme
                        .style(ThemeRole::User)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::styled(" · click to open", theme.style(ThemeRole::MutedText)),
            ])];
            lines.extend(
                message
                    .content
                    .lines(
                        theme.style(ThemeRole::Text),
                        theme.accent_badge(),
                        theme.style(ThemeRole::Accent),
                    )
                    .into_iter()
                    .map(|line| {
                        let mut spans = vec![Span::styled("│ ", theme.style(ThemeRole::MutedText))];
                        spans.extend(line.spans);
                        Line::from(spans)
                    }),
            );
            lines.push(Line::styled(
                "└",
                theme
                    .style(ThemeRole::User)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ));
            lines
        }
        EntryKind::Assistant(message) => {
            let mut lines = vec![Line::styled(
                "┌─ funcode",
                theme
                    .style(ThemeRole::Agent)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )];
            match &message.status {
                AssistantStatus::Queued => {
                    lines.push(Line::styled("│ queued…", theme.style(ThemeRole::Accent)))
                }
                AssistantStatus::Thinking => {
                    lines.push(Line::styled("│ thinking…", theme.style(ThemeRole::Accent)))
                }
                AssistantStatus::Streaming | AssistantStatus::Completed => {
                    lines.extend(message_lines(&message.text, theme.style(ThemeRole::Text)));
                }
                AssistantStatus::Interrupted => {
                    lines.extend(message_lines(&message.text, theme.style(ThemeRole::Text)));
                    lines.push(Line::styled(
                        "│ [interrupted]",
                        theme.style(ThemeRole::Warning),
                    ));
                }
                AssistantStatus::Failed(message) => {
                    lines.push(Line::styled(
                        format!("│ [failed: {message}]"),
                        theme.style(ThemeRole::Warning),
                    ));
                }
            }
            lines.push(Line::styled(
                "└",
                theme
                    .style(ThemeRole::Agent)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ));
            lines
        }
        EntryKind::Reasoning(reasoning) => {
            let expanded = app.entry_is_expanded(entry.id);
            let status = status_label(&reasoning.status);
            let mut lines = vec![Line::from(vec![
                Span::styled("┌─ thinking", theme.style(ThemeRole::Accent)),
                Span::styled(
                    format!(
                        " · {status} · click to {}",
                        if expanded { "collapse" } else { "expand" }
                    ),
                    theme.style(ThemeRole::MutedText),
                ),
            ])];
            if expanded {
                if reasoning.summary.is_empty() {
                    let content = if reasoning.status == ActivityStatus::Running {
                        Line::styled(
                            format!("│ Working{}", spinner(app.animation_frame)),
                            theme.style(ThemeRole::Accent),
                        )
                    } else {
                        Line::styled(
                            "│ No reasoning summary was provided",
                            theme.style(ThemeRole::MutedText),
                        )
                    };
                    lines.push(content);
                } else {
                    lines.extend(message_lines(
                        &reasoning.summary,
                        theme.style(ThemeRole::MutedText),
                    ));
                }
            } else if reasoning.status == ActivityStatus::Running {
                lines.push(Line::styled(
                    format!("│ Thinking{}", spinner(app.animation_frame)),
                    theme.style(ThemeRole::Accent),
                ));
            } else {
                lines.push(Line::styled(
                    "│ Summary hidden",
                    theme.style(ThemeRole::MutedText),
                ));
            }
            lines.push(Line::styled("└", theme.style(ThemeRole::Accent)));
            lines
        }
        EntryKind::Tool(tool) => {
            let expanded = app.entry_is_expanded(entry.id);
            let title = if tool.name == "terminal" {
                "┌─ terminal".to_owned()
            } else {
                format!("┌─ tool · {}", tool.name)
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(title, theme.style(ThemeRole::Accent)),
                Span::styled(
                    format!(
                        " · {} · click to {}",
                        status_label(&tool.status),
                        if expanded { "collapse" } else { "expand" }
                    ),
                    theme.style(ThemeRole::MutedText),
                ),
            ])];
            if expanded {
                lines.extend(message_lines(
                    &tool.summary,
                    theme.style(ThemeRole::MutedText),
                ));
                for artifact in &tool.artifacts {
                    lines.extend(artifact_lines(artifact, theme));
                }
            } else {
                lines.push(Line::styled(
                    format!("│ {}", tool.summary),
                    theme.style(ThemeRole::MutedText),
                ));
            }
            lines.push(Line::styled("└", theme.style(ThemeRole::Accent)));
            lines
        }
    }
}

fn message_lines(text: &str, style: ratatui::style::Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        vec![Line::styled("│", style)]
    } else {
        text.split('\n')
            .map(|line| Line::styled(format!("│ {line}"), style))
            .collect()
    }
}

fn artifact_lines(artifact: &ToolArtifact, theme: &Theme) -> Vec<Line<'static>> {
    match artifact {
        ToolArtifact::CodeRange {
            path,
            start_line,
            end_line,
            preview,
        } => {
            let mut lines = vec![Line::styled(
                format!("│ Read {path}:{start_line}-{end_line}"),
                theme.style(ThemeRole::Accent),
            )];
            if let Some(preview) = preview {
                lines.extend(message_lines(preview, theme.style(ThemeRole::MutedText)));
            }
            lines
        }
        ToolArtifact::Patch { path, diff } => {
            let mut lines = vec![Line::styled(
                format!("│ Edited {path}"),
                theme.style(ThemeRole::Accent),
            )];
            lines.extend(diff.lines().map(|line| {
                let role = if line.starts_with('+') && !line.starts_with("+++") {
                    ThemeRole::DiffAdded
                } else if line.starts_with('-') && !line.starts_with("---") {
                    ThemeRole::DiffRemoved
                } else {
                    ThemeRole::MutedText
                };
                Line::styled(format!("│ {line}"), theme.style(role))
            }));
            lines
        }
        ToolArtifact::SearchResults { query, matches } => {
            let mut lines = vec![Line::styled(
                format!("│ Search /{query}/"),
                theme.style(ThemeRole::Accent),
            )];
            lines.extend(message_lines(matches, theme.style(ThemeRole::MutedText)));
            lines
        }
        ToolArtifact::Terminal {
            description,
            command,
            output,
            exit_code,
        } => {
            let mut lines = vec![
                Line::styled(
                    format!("│ {description}"),
                    theme.style(ThemeRole::MutedText),
                ),
                Line::styled(format!("│ $ {command}"), theme.style(ThemeRole::Text)),
            ];
            if !output.is_empty() {
                lines.extend(message_lines(output, theme.style(ThemeRole::Text)));
            }
            if let Some(exit_code) = exit_code {
                lines.push(Line::styled(
                    format!("│ exit {exit_code}"),
                    if *exit_code == 0 {
                        theme.style(ThemeRole::DiffAdded)
                    } else {
                        theme.style(ThemeRole::Warning)
                    },
                ));
            }
            lines
        }
        ToolArtifact::TextDetail(text) => message_lines(text, theme.style(ThemeRole::MutedText)),
        ToolArtifact::FileReference(path) => {
            vec![Line::styled(
                format!("│ File {path}"),
                theme.style(ThemeRole::Accent),
            )]
        }
    }
}

fn status_label(status: &ActivityStatus) -> String {
    status.to_string()
}

fn spinner(frame: usize) -> &'static str {
    ["|", "/", "-", "\\"][(frame / 2) % 4]
}

fn wrapped_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}
