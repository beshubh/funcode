use crate::{
    app::App,
    composer::DisplayRunKind,
    theme::{Theme, ThemeId, ThemeRole},
    transcript::{ActivityStatus, AssistantStatus, Entry, EntryId, EntryKind, ToolArtifact},
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryRegion {
    pub id: EntryId,
    pub area: Rect,
}

const RENDERED_ENTRY_CACHE_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightKey {
    revision: u64,
    width: usize,
    expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinesKey {
    revision: u64,
    expanded: bool,
    theme: ThemeId,
}

#[derive(Debug, Clone, Copy)]
struct CachedHeight {
    key: HeightKey,
    height: usize,
}

#[derive(Debug)]
struct CachedLines {
    entry_id: EntryId,
    key: LinesKey,
    lines: Arc<Vec<Line<'static>>>,
}

#[derive(Debug, Default)]
struct RenderCacheInner {
    heights: HashMap<EntryId, CachedHeight>,
    lines: VecDeque<CachedLines>,
    height_builds: usize,
    line_builds: usize,
}

/// Retains immutable transcript measurements between terminal frames.
///
/// Historical entries dominate long conversations, so their wrapped heights
/// are cached without a fixed limit. Fully materialized lines are much larger;
/// only a small viewport-oriented LRU is retained for those.
#[derive(Debug, Default)]
pub(crate) struct TranscriptRenderCache {
    inner: RefCell<RenderCacheInner>,
}

impl TranscriptRenderCache {
    fn height(&self, entry_id: EntryId, key: HeightKey) -> Option<usize> {
        self.inner
            .borrow()
            .heights
            .get(&entry_id)
            .filter(|cached| cached.key == key)
            .map(|cached| cached.height)
    }

    fn store_height(&self, entry_id: EntryId, key: HeightKey, height: usize) {
        let mut inner = self.inner.borrow_mut();
        inner.height_builds = inner.height_builds.saturating_add(1);
        inner.heights.insert(entry_id, CachedHeight { key, height });
    }

    fn lines(&self, entry_id: EntryId, key: LinesKey) -> Option<Arc<Vec<Line<'static>>>> {
        let mut inner = self.inner.borrow_mut();
        let index = inner
            .lines
            .iter()
            .position(|cached| cached.entry_id == entry_id && cached.key == key)?;
        let cached = inner.lines.remove(index)?;
        let lines = Arc::clone(&cached.lines);
        inner.lines.push_back(cached);
        Some(lines)
    }

    fn store_lines(
        &self,
        entry_id: EntryId,
        key: LinesKey,
        lines: Vec<Line<'static>>,
    ) -> Arc<Vec<Line<'static>>> {
        let mut inner = self.inner.borrow_mut();
        inner.line_builds = inner.line_builds.saturating_add(1);
        inner.lines.retain(|cached| cached.entry_id != entry_id);
        let lines = Arc::new(lines);
        inner.lines.push_back(CachedLines {
            entry_id,
            key,
            lines: Arc::clone(&lines),
        });
        while inner.lines.len() > RENDERED_ENTRY_CACHE_CAPACITY {
            inner.lines.pop_front();
        }
        lines
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (inner.height_builds, inner.line_builds)
    }
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
    cache: &TranscriptRenderCache,
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

    let width = content_area.width.max(1) as usize;
    let mut measured = Vec::with_capacity(app.transcript.entries().len());
    let mut next_line = 0usize;

    for entry in app.transcript.entries() {
        let user_layout = match &entry.kind {
            EntryKind::User(message) => {
                Some(message.content.layout(width.saturating_sub(4).max(1)))
            }
            _ => None,
        };
        let height = user_layout.as_ref().map_or_else(
            || measured_entry_height(entry, app, theme, width, cache),
            |layout| layout.total_rows().saturating_add(2),
        );
        let start = next_line;
        next_line += height;
        measured.push(MeasuredEntry {
            entry,
            start,
            end: next_line,
            user_layout,
        });
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
    let line_count = next_line;
    let viewport_height = viewport_area.height as usize;
    let maximum_top = line_count.saturating_sub(viewport_height);
    let from_bottom = app.scroll_from_bottom.min(maximum_top);
    let top = maximum_top.saturating_sub(from_bottom);
    let mut regions = Vec::new();
    for measured in measured {
        let visible_start = measured.start.max(top);
        let visible_end = measured
            .end
            .min(top.saturating_add(viewport_area.height as usize));
        if visible_start >= visible_end {
            continue;
        }
        let local = visible_start.saturating_sub(measured.start)
            ..visible_end.saturating_sub(measured.start);
        let area = Rect::new(
            viewport_area.x,
            viewport_area
                .y
                .saturating_add((visible_start - top).min(u16::MAX as usize) as u16),
            viewport_area.width,
            (visible_end - visible_start).min(u16::MAX as usize) as u16,
        );
        let lines = if let (EntryKind::User(message), Some(layout)) =
            (&measured.entry.kind, measured.user_layout.as_ref())
        {
            visible_user_lines(message, layout, local, theme)
        } else {
            let lines = cached_entry_lines(measured.entry, app, theme, cache);
            visible_wrapped_lines(lines.as_slice(), width, local)
        };
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        if matches!(
            measured.entry.kind,
            EntryKind::User(_) | EntryKind::Reasoning(_) | EntryKind::Tool(_)
        ) {
            regions.push(EntryRegion {
                id: measured.entry.id,
                area,
            });
        }
    }
    regions
}

fn measured_entry_height(
    entry: &Entry,
    app: &App,
    theme: &Theme,
    width: usize,
    cache: &TranscriptRenderCache,
) -> usize {
    let key = HeightKey {
        revision: entry.revision(),
        width,
        expanded: app.transcript_entry_is_expanded(entry),
    };
    if let Some(height) = cache.height(entry.id, key) {
        return height;
    }
    let lines = cached_entry_lines(entry, app, theme, cache);
    let height = wrapped_height(lines.as_slice(), width);
    cache.store_height(entry.id, key, height);
    height
}

fn cached_entry_lines(
    entry: &Entry,
    app: &App,
    theme: &Theme,
    cache: &TranscriptRenderCache,
) -> Arc<Vec<Line<'static>>> {
    let key = LinesKey {
        revision: entry.revision(),
        expanded: app.transcript_entry_is_expanded(entry),
        theme: theme.id(),
    };
    let cacheable = !matches!(
        &entry.kind,
        EntryKind::Reasoning(reasoning) if reasoning.status == ActivityStatus::Running
    );
    if cacheable && let Some(lines) = cache.lines(entry.id, key) {
        return lines;
    }
    let lines = entry_lines(entry, app, theme);
    if cacheable {
        cache.store_lines(entry.id, key, lines)
    } else {
        Arc::new(lines)
    }
}

struct MeasuredEntry<'a> {
    entry: &'a Entry,
    start: usize,
    end: usize,
    user_layout: Option<Arc<crate::composer::ComposerLayout>>,
}

fn visible_user_lines(
    _message: &crate::transcript::UserMessage,
    layout: &crate::composer::ComposerLayout,
    range: std::ops::Range<usize>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_rows = layout.total_rows();
    let visible_start = range.start.max(1).saturating_sub(1).min(content_rows);
    let visible_end = range
        .end
        .min(content_rows.saturating_add(1))
        .saturating_sub(1)
        .max(visible_start);
    let mut visible = layout
        .visible_rows(visible_start, visible_end.saturating_sub(visible_start))
        .into_iter();
    range
        .filter_map(|row| {
            if row == 0 {
                Some(Line::from(vec![
                    Span::styled(
                        "┌─ you",
                        theme
                            .style(ThemeRole::User)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    Span::styled(" · click to open", theme.style(ThemeRole::MutedText)),
                ]))
            } else if row <= content_rows {
                let line = visible.next()?;
                let mut spans = vec![Span::styled("│ ", theme.style(ThemeRole::MutedText))];
                spans.extend(line.runs.into_iter().map(|run| {
                    let style = match run.kind {
                        DisplayRunKind::Text => theme.style(ThemeRole::Text),
                        DisplayRunKind::FileReference | DisplayRunKind::PastedBlock => {
                            theme.accent_badge()
                        }
                    };
                    Span::styled(run.text, style)
                }));
                Some(Line::from(spans))
            } else if row == content_rows.saturating_add(1) {
                Some(Line::styled(
                    "└",
                    theme
                        .style(ThemeRole::User)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
}

fn visible_wrapped_lines(
    lines: &[Line<'static>],
    width: usize,
    range: std::ops::Range<usize>,
) -> Vec<Line<'static>> {
    let mut output = Vec::with_capacity(range.len());
    let mut wrapped_row = 0usize;
    'lines: for line in lines {
        let mut spans = Vec::new();
        let mut column = 0usize;
        for span in &line.spans {
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                if column > 0 && column.saturating_add(grapheme_width) > width {
                    if range.contains(&wrapped_row) {
                        output.push(Line::from(std::mem::take(&mut spans)));
                    } else {
                        spans.clear();
                    }
                    wrapped_row = wrapped_row.saturating_add(1);
                    column = 0;
                    if wrapped_row >= range.end {
                        break 'lines;
                    }
                }
                spans.push(Span::styled(
                    grapheme.to_owned(),
                    line.style.patch(span.style),
                ));
                column = column.saturating_add(grapheme_width);
            }
        }
        if range.contains(&wrapped_row) {
            output.push(Line::from(spans));
        }
        wrapped_row = wrapped_row.saturating_add(1);
        if wrapped_row >= range.end {
            break;
        }
    }
    output
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
            lines.extend(message.content.display_lines(2).into_iter().map(|line| {
                let mut spans = vec![Span::styled("│ ", theme.style(ThemeRole::MutedText))];
                spans.extend(line.runs.into_iter().map(|run| {
                    let style = match run.kind {
                        DisplayRunKind::Text => theme.style(ThemeRole::Text),
                        DisplayRunKind::FileReference | DisplayRunKind::PastedBlock => {
                            theme.accent_badge()
                        }
                    };
                    Span::styled(run.text, style)
                }));
                Line::from(spans)
            }));
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
            let expanded = app.transcript_entry_is_expanded(entry);
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
            let expanded = app.transcript_entry_is_expanded(entry);
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
                    format!("│ {}", crate::composer::safe_single_line(&tool.summary, 2)),
                    theme.style(ThemeRole::MutedText),
                ));
            }
            lines.push(Line::styled("└", theme.style(ThemeRole::Accent)));
            lines
        }
        EntryKind::Retry(retry) => vec![Line::from(vec![
            Span::styled("↻ ", theme.style(ThemeRole::Accent)),
            Span::styled(
                format!("Attempt {}/{} failed: ", retry.attempt, retry.max_retries),
                theme.style(ThemeRole::Warning),
            ),
            Span::styled(retry.message.clone(), theme.style(ThemeRole::Warning)),
            Span::styled(" · Retrying…", theme.style(ThemeRole::Accent)),
        ])],
    }
}

fn message_lines(text: &str, style: ratatui::style::Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        vec![Line::styled("│", style)]
    } else {
        crate::composer::SubmittedContent::plain(text)
            .display_lines(2)
            .into_iter()
            .map(|line| {
                let mut spans = vec![Span::styled("│ ", style)];
                spans.extend(
                    line.runs
                        .into_iter()
                        .map(|run| Span::styled(run.text, style)),
                );
                Line::from(spans)
            })
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
                format!("│ Read {}:{start_line}-{end_line}", path.display()),
                theme.style(ThemeRole::Accent),
            )];
            if let Some(preview) = preview {
                lines.extend(message_lines(preview, theme.style(ThemeRole::MutedText)));
            }
            lines
        }
        ToolArtifact::Patch { path, diff } => {
            let mut lines = vec![Line::styled(
                format!("│ Edited {}", path.display()),
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
                Line::styled(
                    format!("│ {}", crate::composer::safe_single_line(line, 2)),
                    theme.style(role),
                )
            }));
            lines
        }
        ToolArtifact::SearchResults { query, matches } => {
            let mut lines = vec![Line::styled(
                format!(
                    "│ Search /{}/",
                    crate::composer::safe_single_line(query, 10)
                ),
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
                    format!("│ {}", crate::composer::safe_single_line(description, 2)),
                    theme.style(ThemeRole::MutedText),
                ),
                Line::styled(
                    format!("│ $ {}", crate::composer::safe_single_line(command, 4)),
                    theme.style(ThemeRole::Text),
                ),
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
                format!("│ File {}", path.display()),
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

fn wrapped_height(lines: &[Line<'_>], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}
