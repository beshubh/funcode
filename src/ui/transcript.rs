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

pub(super) struct RenderResult {
    pub entries: Vec<EntryRegion>,
    pub scroll_maximum: usize,
}

const RENDERED_ENTRY_CACHE_CAPACITY: usize = 32;
const RENDERED_ROW_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightKey {
    revision: u64,
    width: usize,
    expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowsKey {
    revision: u64,
    width: usize,
    expanded: bool,
    theme: ThemeId,
}

#[derive(Debug, Clone, Copy)]
struct CachedHeight {
    key: HeightKey,
    height: usize,
}

#[derive(Debug)]
struct CachedRows {
    entry_id: EntryId,
    key: RowsKey,
    rows: Arc<Vec<Line<'static>>>,
    bytes: usize,
}

#[derive(Debug, Default)]
struct RenderCacheInner {
    heights: HashMap<EntryId, CachedHeight>,
    rows: VecDeque<CachedRows>,
    row_bytes: usize,
    height_builds: usize,
    line_builds: usize,
    visible_rows_copied: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexKey {
    width: usize,
    theme: ThemeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexedEntry {
    id: EntryId,
    revision: u64,
    expanded: bool,
    start: usize,
    end: usize,
}

#[derive(Debug, Default)]
struct LayoutIndex {
    key: Option<IndexKey>,
    entries: Vec<IndexedEntry>,
    entries_measured: usize,
}

/// Retains immutable transcript measurements between terminal frames.
///
/// Historical entries dominate long conversations, so their wrapped heights
/// are cached without a fixed limit. Fully materialized lines are much larger;
/// only a small viewport-oriented LRU is retained for those.
#[derive(Debug, Default)]
pub(crate) struct TranscriptRenderCache {
    inner: RefCell<RenderCacheInner>,
    index: RefCell<LayoutIndex>,
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

    fn rows(&self, entry_id: EntryId, key: RowsKey) -> Option<Arc<Vec<Line<'static>>>> {
        let mut inner = self.inner.borrow_mut();
        let index = inner
            .rows
            .iter()
            .position(|cached| cached.entry_id == entry_id && cached.key == key)?;
        let cached = inner.rows.remove(index)?;
        let rows = Arc::clone(&cached.rows);
        inner.rows.push_back(cached);
        Some(rows)
    }

    fn store_rows(
        &self,
        entry_id: EntryId,
        key: RowsKey,
        rows: Vec<Line<'static>>,
    ) -> Arc<Vec<Line<'static>>> {
        let mut inner = self.inner.borrow_mut();
        inner.line_builds = inner.line_builds.saturating_add(1);
        if let Some(index) = inner
            .rows
            .iter()
            .position(|cached| cached.entry_id == entry_id)
            && let Some(previous) = inner.rows.remove(index)
        {
            inner.row_bytes = inner.row_bytes.saturating_sub(previous.bytes);
        }
        let bytes = rows
            .iter()
            .map(|line| {
                std::mem::size_of::<Line<'static>>()
                    + line
                        .spans
                        .iter()
                        .map(|span| std::mem::size_of::<Span<'static>>() + span.content.len())
                        .sum::<usize>()
            })
            .sum();
        let rows = Arc::new(rows);
        inner.row_bytes = inner.row_bytes.saturating_add(bytes);
        inner.rows.push_back(CachedRows {
            entry_id,
            key,
            rows: Arc::clone(&rows),
            bytes,
        });
        while inner.rows.len() > RENDERED_ENTRY_CACHE_CAPACITY
            || inner.row_bytes > RENDERED_ROW_CACHE_BYTES
        {
            let Some(evicted) = inner.rows.pop_front() else {
                break;
            };
            inner.row_bytes = inner.row_bytes.saturating_sub(evicted.bytes);
        }
        rows
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (inner.height_builds, inner.line_builds)
    }

    #[cfg(test)]
    pub(crate) fn viewport_stats(&self) -> (usize, usize) {
        (
            self.index.borrow().entries_measured,
            self.inner.borrow().visible_rows_copied,
        )
    }

    fn record_visible_rows(&self, count: usize) {
        let mut inner = self.inner.borrow_mut();
        inner.visible_rows_copied = inner.visible_rows_copied.saturating_add(count);
    }
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
    cache: &TranscriptRenderCache,
) -> RenderResult {
    let content_area = area.inner(Margin::new(2, 0));
    if app.transcript.entries().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No messages yet. Type something below to begin.",
                theme.style(ThemeRole::MutedText),
            )),
            content_area,
        );
        return RenderResult {
            entries: Vec::new(),
            scroll_maximum: 0,
        };
    }

    let width = content_area.width.max(1) as usize;
    ensure_layout_index(app, theme, width, cache);
    let index = cache.index.borrow();
    let next_line = index.entries.last().map_or(0, |entry| entry.end);

    let full_viewport_maximum = next_line.saturating_sub(content_area.height as usize);
    let visibly_manual =
        !app.transcript_is_following() && app.transcript_scroll_offset(full_viewport_maximum) > 0;
    let viewport_area = if !visibly_manual {
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
    let from_bottom = app.transcript_scroll_offset(maximum_top);
    let top = maximum_top.saturating_sub(from_bottom);
    let mut regions = Vec::new();
    let viewport_end = top.saturating_add(viewport_area.height as usize);
    let first_visible = index.entries.partition_point(|entry| entry.end <= top);
    for (entry_index, measured) in index.entries.iter().enumerate().skip(first_visible) {
        if measured.start >= viewport_end {
            break;
        }
        let entry = &app.transcript.entries()[entry_index];
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
        let lines = if let EntryKind::User(message) = &entry.kind {
            let layout = message.content.layout(width.saturating_sub(4).max(1));
            visible_user_lines(message, &layout, local, theme)
        } else {
            let rows = cached_entry_rows(entry, app, theme, width, cache);
            let visible = rows[local].to_vec();
            cache.record_visible_rows(visible.len());
            visible
        };
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        if matches!(
            entry.kind,
            EntryKind::User(_) | EntryKind::Reasoning(_) | EntryKind::Tool(_)
        ) {
            regions.push(EntryRegion { id: entry.id, area });
        }
    }
    RenderResult {
        entries: regions,
        scroll_maximum: maximum_top,
    }
}

fn ensure_layout_index(app: &App, theme: &Theme, width: usize, cache: &TranscriptRenderCache) {
    let key = IndexKey {
        width,
        theme: theme.id(),
    };
    let entries = app.transcript.entries();
    let valid_prefix = {
        let index = cache.index.borrow();
        if index.key != Some(key) {
            0
        } else {
            index
                .entries
                .iter()
                .zip(entries)
                .take_while(|(cached, entry)| {
                    cached.id == entry.id
                        && cached.revision == entry.revision()
                        && cached.expanded == app.transcript_entry_is_expanded(entry)
                })
                .count()
        }
    };

    let already_complete = {
        let index = cache.index.borrow();
        index.key == Some(key)
            && valid_prefix == entries.len()
            && index.entries.len() == entries.len()
    };
    if already_complete {
        return;
    }

    let mut updated = {
        let index = cache.index.borrow();
        index.entries[..valid_prefix.min(index.entries.len())].to_vec()
    };
    let mut next_line = updated.last().map_or(0, |entry| entry.end);
    for entry in &entries[valid_prefix..] {
        let height = match &entry.kind {
            EntryKind::User(message) => message
                .content
                .layout(width.saturating_sub(4).max(1))
                .total_rows()
                .saturating_add(2),
            _ => measured_entry_height(entry, app, theme, width, cache),
        };
        let start = next_line;
        next_line = next_line.saturating_add(height);
        updated.push(IndexedEntry {
            id: entry.id,
            revision: entry.revision(),
            expanded: app.transcript_entry_is_expanded(entry),
            start,
            end: next_line,
        });
    }

    let mut index = cache.index.borrow_mut();
    index.key = Some(key);
    index.entries_measured = index
        .entries_measured
        .saturating_add(entries.len().saturating_sub(valid_prefix));
    index.entries = updated;
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
    let rows = cached_entry_rows(entry, app, theme, width, cache);
    let height = rows.len();
    cache.store_height(entry.id, key, height);
    height
}

fn cached_entry_rows(
    entry: &Entry,
    app: &App,
    theme: &Theme,
    width: usize,
    cache: &TranscriptRenderCache,
) -> Arc<Vec<Line<'static>>> {
    let key = RowsKey {
        revision: entry.revision(),
        width,
        expanded: app.transcript_entry_is_expanded(entry),
        theme: theme.id(),
    };
    let cacheable = !matches!(
        &entry.kind,
        EntryKind::Reasoning(reasoning) if reasoning.status == ActivityStatus::Running
    );
    if cacheable && let Some(rows) = cache.rows(entry.id, key) {
        return rows;
    }
    let lines = entry_lines(entry, app, theme);
    let rows = wrap_lines(&lines, width);
    if cacheable {
        cache.store_rows(entry.id, key, rows)
    } else {
        Arc::new(rows)
    }
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

fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        let mut column = 0usize;
        for span in &line.spans {
            let style = line.style.patch(span.style);
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                if column > 0 && column.saturating_add(grapheme_width) > width {
                    output.push(Line::from(std::mem::take(&mut spans)));
                    column = 0;
                }
                if let Some(previous) = spans.last_mut()
                    && previous.style == style
                {
                    previous.content.to_mut().push_str(grapheme);
                } else {
                    spans.push(Span::styled(grapheme.to_owned(), style));
                }
                column = column.saturating_add(grapheme_width);
            }
        }
        output.push(Line::from(spans));
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
