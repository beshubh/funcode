use super::markdown::MarkdownLayout;
use crate::{
    app::{App, ToolOutputScrollMetrics},
    composer::DisplayRunKind,
    theme::{Theme, ThemeId, ThemeRole},
    transcript::{
        ActivityStatus, AssistantMessage, AssistantStatus, CodeRangeArtifact, Entry, EntryId,
        EntryKind, FileReferenceArtifact, PatchArtifact, Reasoning, RetryAttempt,
        SearchResultsArtifact, TerminalArtifact, TextDetailArtifact, ToolArtifact, ToolCall,
        UserMessage,
    },
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Margin, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryRegion {
    pub id: EntryId,
    pub area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRegion {
    pub id: EntryId,
    pub area: Rect,
}

pub(super) struct RenderResult {
    pub entries: Vec<EntryRegion>,
    pub outputs: Vec<OutputRegion>,
    pub output_scroll_metrics: Vec<ToolOutputScrollMetrics>,
    pub scroll_maximum: usize,
}

const RENDERED_SLICE_CACHE_CAPACITY: usize = 32;
const RENDERED_SLICE_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MARKDOWN_LAYOUT_CACHE_CAPACITY: usize = 32;
const MARKDOWN_LAYOUT_CACHE_BYTES: usize = 4 * 1024 * 1024;
const TOOL_OUTPUT_LAYOUT_CACHE_CAPACITY: usize = 32;
const TOOL_OUTPUT_LAYOUT_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightKey {
    revision: u64,
    width: usize,
    available_height: usize,
    expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceKey {
    revision: u64,
    width: usize,
    available_height: usize,
    expanded: bool,
    output_scroll_from_bottom: usize,
    theme: ThemeId,
    visible_start: usize,
    visible_height: usize,
}

#[derive(Debug, Clone, Copy)]
struct CachedHeight {
    key: HeightKey,
    height: usize,
}

#[derive(Debug)]
struct CachedSlice {
    entry_id: EntryId,
    key: SliceKey,
    buffer: Arc<Buffer>,
    bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolOutputLayoutKey {
    revision: u64,
    width: usize,
}

#[derive(Debug)]
struct CachedToolOutputLayout {
    entry_id: EntryId,
    key: ToolOutputLayoutKey,
    layout: Arc<ToolOutputBodyLayout>,
    bytes: usize,
}

#[derive(Debug)]
struct ToolOutputRow {
    line: Line<'static>,
    role: ThemeRole,
    logical_line: usize,
}

#[derive(Debug, Default)]
struct ToolOutputBodyLayout {
    rows: Vec<ToolOutputRow>,
    logical_lines: Arc<[usize]>,
}

impl ToolOutputBodyLayout {
    fn height(&self) -> usize {
        self.rows.len()
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.rows.iter().fold(0usize, |bytes, row| {
            bytes
                .saturating_add(std::mem::size_of::<ToolOutputRow>())
                .saturating_add(row.line.to_string().len())
        }))
    }

    fn logical_lines(&self) -> Arc<[usize]> {
        Arc::clone(&self.logical_lines)
    }

    fn render(&self, context: RenderContext<'_>) {
        let start = context.visible_rows.start.min(self.rows.len());
        let end = context.visible_rows.end.min(self.rows.len());
        for (destination_row, row) in self.rows[start..end].iter().enumerate() {
            context.buffer.set_line(
                context.area.x,
                context.area.y.saturating_add(destination_row as u16),
                &row.line.clone().style(context.theme.style(row.role)),
                context.area.width,
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MarkdownLayoutKey {
    revision: u64,
    width: usize,
}

#[derive(Debug)]
struct CachedMarkdownLayout {
    entry_id: EntryId,
    key: MarkdownLayoutKey,
    layout: Arc<MarkdownLayout>,
    bytes: usize,
}

#[derive(Debug, Default)]
struct RenderCacheInner {
    heights: HashMap<EntryId, CachedHeight>,
    slices: VecDeque<CachedSlice>,
    slice_bytes: usize,
    markdown_layouts: VecDeque<CachedMarkdownLayout>,
    markdown_layout_bytes: usize,
    tool_output_layouts: VecDeque<CachedToolOutputLayout>,
    tool_output_layout_bytes: usize,
    height_builds: usize,
    slice_builds: usize,
    markdown_layout_builds: usize,
    visible_rows_copied: usize,
    tool_output_layout_builds: usize,
    tool_output_rows_indexed: usize,
    tool_output_rows_rendered: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexKey {
    width: usize,
    available_height: usize,
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

/// Retains transcript measurements and rendered viewport slices between frames.
///
/// Historical entries dominate long conversations, so their wrapped heights
/// are cached without a fixed limit. Rendered buffers are larger and depend on
/// the visible row range, so only a small viewport-oriented LRU is retained.
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

    fn slice(&self, entry_id: EntryId, key: SliceKey) -> Option<Arc<Buffer>> {
        let mut inner = self.inner.borrow_mut();
        let index = inner
            .slices
            .iter()
            .position(|cached| cached.entry_id == entry_id && cached.key == key)?;
        let cached = inner.slices.remove(index)?;
        let buffer = Arc::clone(&cached.buffer);
        inner.slices.push_back(cached);
        Some(buffer)
    }

    fn store_slice(&self, entry_id: EntryId, key: SliceKey, buffer: Buffer) -> Arc<Buffer> {
        let mut inner = self.inner.borrow_mut();
        if let Some(index) = inner
            .slices
            .iter()
            .position(|cached| cached.entry_id == entry_id && cached.key == key)
            && let Some(previous) = inner.slices.remove(index)
        {
            inner.slice_bytes = inner.slice_bytes.saturating_sub(previous.bytes);
        }
        let bytes = std::mem::size_of::<Buffer>().saturating_add(
            buffer.content.len().saturating_mul(std::mem::size_of_val(
                buffer
                    .content
                    .first()
                    .unwrap_or(&ratatui::buffer::Cell::EMPTY),
            )),
        );
        let buffer = Arc::new(buffer);
        inner.slice_bytes = inner.slice_bytes.saturating_add(bytes);
        inner.slices.push_back(CachedSlice {
            entry_id,
            key,
            buffer: Arc::clone(&buffer),
            bytes,
        });
        while inner.slices.len() > RENDERED_SLICE_CACHE_CAPACITY
            || inner.slice_bytes > RENDERED_SLICE_CACHE_BYTES
        {
            let Some(evicted) = inner.slices.pop_front() else {
                break;
            };
            inner.slice_bytes = inner.slice_bytes.saturating_sub(evicted.bytes);
        }
        buffer
    }

    fn tool_output_layout(&self, entry: &Entry, width: usize) -> Option<Arc<ToolOutputBodyLayout>> {
        let EntryKind::Tool(tool) = &entry.kind else {
            return None;
        };
        if tool_is_summary_only(tool) || output_artifacts(tool).next().is_none() {
            return None;
        }
        let key = ToolOutputLayoutKey {
            revision: entry.revision(),
            width,
        };
        let mut inner = self.inner.borrow_mut();
        if let Some(index) = inner
            .tool_output_layouts
            .iter()
            .position(|cached| cached.entry_id == entry.id && cached.key == key)
            && let Some(cached) = inner.tool_output_layouts.remove(index)
        {
            let layout = Arc::clone(&cached.layout);
            inner.tool_output_layouts.push_back(cached);
            return Some(layout);
        }
        drop(inner);

        let layout = Arc::new(build_tool_output_layout(tool, width));
        let bytes = layout.bytes();
        let mut inner = self.inner.borrow_mut();
        inner.tool_output_layout_builds = inner.tool_output_layout_builds.saturating_add(1);
        inner.tool_output_rows_indexed = inner
            .tool_output_rows_indexed
            .saturating_add(layout.height());
        inner.tool_output_layout_bytes = inner.tool_output_layout_bytes.saturating_add(bytes);
        inner.tool_output_layouts.push_back(CachedToolOutputLayout {
            entry_id: entry.id,
            key,
            layout: Arc::clone(&layout),
            bytes,
        });
        while (inner.tool_output_layouts.len() > TOOL_OUTPUT_LAYOUT_CACHE_CAPACITY
            || inner.tool_output_layout_bytes > TOOL_OUTPUT_LAYOUT_CACHE_BYTES)
            && inner.tool_output_layouts.len() > 1
        {
            let Some(evicted) = inner.tool_output_layouts.pop_front() else {
                break;
            };
            inner.tool_output_layout_bytes =
                inner.tool_output_layout_bytes.saturating_sub(evicted.bytes);
        }
        Some(layout)
    }

    fn record_tool_output_rows(&self, count: usize) {
        let mut inner = self.inner.borrow_mut();
        inner.tool_output_rows_rendered = inner.tool_output_rows_rendered.saturating_add(count);
    }

    #[cfg(test)]
    fn tool_output_stats(&self) -> (usize, usize, usize) {
        let inner = self.inner.borrow();
        (
            inner.tool_output_layout_builds,
            inner.tool_output_rows_indexed,
            inner.tool_output_rows_rendered,
        )
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (inner.height_builds, inner.slice_builds)
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

    fn record_slice_build(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.slice_builds = inner.slice_builds.saturating_add(1);
    }

    fn markdown_layout(&self, entry: &Entry, width: usize) -> Option<Arc<MarkdownLayout>> {
        let EntryKind::Assistant(message) = &entry.kind else {
            return None;
        };
        if !matches!(
            message.status,
            AssistantStatus::Streaming | AssistantStatus::Completed | AssistantStatus::Interrupted
        ) {
            return None;
        }
        let key = MarkdownLayoutKey {
            revision: entry.revision(),
            width,
        };
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(index) = inner
                .markdown_layouts
                .iter()
                .position(|cached| cached.entry_id == entry.id && cached.key == key)
                && let Some(cached) = inner.markdown_layouts.remove(index)
            {
                let layout = Arc::clone(&cached.layout);
                inner.markdown_layouts.push_back(cached);
                return Some(layout);
            }
        }

        let layout = Arc::new(MarkdownLayout::new(
            &message.text,
            width.saturating_sub(2).max(1),
        ));
        let bytes = layout.bytes();
        let mut inner = self.inner.borrow_mut();
        inner.markdown_layout_builds = inner.markdown_layout_builds.saturating_add(1);
        if let Some(index) = inner
            .markdown_layouts
            .iter()
            .position(|cached| cached.entry_id == entry.id)
            && let Some(previous) = inner.markdown_layouts.remove(index)
        {
            inner.markdown_layout_bytes =
                inner.markdown_layout_bytes.saturating_sub(previous.bytes);
        }
        inner.markdown_layout_bytes = inner.markdown_layout_bytes.saturating_add(bytes);
        inner.markdown_layouts.push_back(CachedMarkdownLayout {
            entry_id: entry.id,
            key,
            layout: Arc::clone(&layout),
            bytes,
        });
        while inner.markdown_layouts.len() > MARKDOWN_LAYOUT_CACHE_CAPACITY
            || inner.markdown_layout_bytes > MARKDOWN_LAYOUT_CACHE_BYTES
        {
            let Some(evicted) = inner.markdown_layouts.pop_front() else {
                break;
            };
            inner.markdown_layout_bytes = inner.markdown_layout_bytes.saturating_sub(evicted.bytes);
        }
        Some(layout)
    }

    #[cfg(test)]
    fn markdown_layout_builds(&self) -> usize {
        self.inner.borrow().markdown_layout_builds
    }
}

struct RenderContext<'a> {
    theme: &'a Theme,
    area: Rect,
    buffer: &'a mut Buffer,
    visible_rows: Range<usize>,
}

trait Render {
    fn height(&self, width: usize) -> usize;
    fn render(&self, context: RenderContext<'_>);

    fn cacheable(&self) -> bool {
        true
    }

    fn clickable(&self, _width: usize) -> bool {
        false
    }
}

struct EntryRenderer<'a> {
    entry: &'a Entry,
    cache: &'a TranscriptRenderCache,
    expanded: bool,
    animation_frame: usize,
    markdown_layout: Option<Arc<MarkdownLayout>>,
    available_height: usize,
    output_scroll_from_bottom: usize,
    output_body_layout: Option<Arc<ToolOutputBodyLayout>>,
}

impl<'a> EntryRenderer<'a> {
    fn new(
        entry: &'a Entry,
        app: &App,
        cache: &'a TranscriptRenderCache,
        width: usize,
        available_height: usize,
    ) -> Self {
        let expanded = app.transcript_entry_is_expanded(entry);
        let output_body_layout = cache.tool_output_layout(entry, width);
        let output_maximum = match &entry.kind {
            EntryKind::Tool(tool) => ToolRenderer {
                tool,
                expanded,
                available_height,
                scroll_from_bottom: 0,
                body_layout: output_body_layout.as_deref(),
                cache: Some(cache),
            }
            .output_layout(width)
            .map(|layout| layout.maximum),
            _ => None,
        };
        let output_logical_lines = output_body_layout
            .as_ref()
            .map(|layout| layout.logical_lines());
        Self {
            entry,
            cache,
            expanded,
            animation_frame: app.animation_frame,
            markdown_layout: cache.markdown_layout(entry, width),
            available_height,
            output_scroll_from_bottom: output_maximum
                .map(|maximum| {
                    app.tool_output_scroll_offset_for_layout(
                        entry.id,
                        maximum,
                        output_logical_lines.as_ref(),
                    )
                })
                .unwrap_or_default(),
            output_body_layout,
        }
    }

    fn dispatch<T>(&self, dispatch: impl FnOnce(&dyn Render) -> T) -> T {
        match &self.entry.kind {
            EntryKind::User(message) => dispatch(message),
            EntryKind::Assistant(message) => dispatch(&AssistantRenderer {
                message,
                layout: self.markdown_layout.as_deref(),
            }),
            EntryKind::Reasoning(reasoning) => dispatch(&ReasoningRenderer {
                reasoning,
                expanded: self.expanded,
                animation_frame: self.animation_frame,
            }),
            EntryKind::Tool(tool) => dispatch(&ToolRenderer {
                tool,
                expanded: self.expanded,
                available_height: self.available_height,
                scroll_from_bottom: self.output_scroll_from_bottom,
                body_layout: self.output_body_layout.as_deref(),
                cache: Some(self.cache),
            }),
            EntryKind::Retry(retry) => dispatch(retry),
        }
    }

    fn output_viewport(&self, width: usize) -> Option<Range<usize>> {
        let EntryKind::Tool(tool) = &self.entry.kind else {
            return None;
        };
        ToolRenderer {
            tool,
            expanded: self.expanded,
            available_height: self.available_height,
            scroll_from_bottom: self.output_scroll_from_bottom,
            body_layout: self.output_body_layout.as_deref(),
            cache: Some(self.cache),
        }
        .output_viewport(width)
    }

    fn output_scroll_maximum(&self, width: usize) -> Option<usize> {
        let EntryKind::Tool(tool) = &self.entry.kind else {
            return None;
        };
        ToolRenderer {
            tool,
            expanded: self.expanded,
            available_height: self.available_height,
            scroll_from_bottom: self.output_scroll_from_bottom,
            body_layout: self.output_body_layout.as_deref(),
            cache: Some(self.cache),
        }
        .output_layout(width)
        .map(|layout| layout.maximum)
    }

    fn output_scroll_metrics(&self, width: usize) -> Option<ToolOutputScrollMetrics> {
        Some(ToolOutputScrollMetrics {
            entry_id: self.entry.id,
            maximum: self.output_scroll_maximum(width)?,
            logical_lines: self.output_body_layout.as_ref()?.logical_lines(),
        })
    }
}

impl Render for EntryRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        self.dispatch(|renderer| renderer.height(width))
    }

    fn render(&self, context: RenderContext<'_>) {
        self.dispatch(|renderer| renderer.render(context));
    }

    fn cacheable(&self) -> bool {
        self.dispatch(|renderer| renderer.cacheable())
    }

    fn clickable(&self, width: usize) -> bool {
        self.dispatch(|renderer| renderer.clickable(width))
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
            outputs: Vec::new(),
            output_scroll_metrics: Vec::new(),
            scroll_maximum: 0,
        };
    }

    let width = content_area.width.max(1) as usize;
    let full_available_height = content_area.height as usize;
    let visibly_manual = app.transcript_has_manual_scroll_offset();
    let available_height = full_available_height.saturating_sub(usize::from(visibly_manual));
    ensure_layout_index(app, theme, width, available_height, cache);
    let index = cache.index.borrow();
    let next_line = index.entries.last().map_or(0, |entry| entry.end);
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
    let viewport_height = viewport_area.height as usize;
    let maximum_top = next_line.saturating_sub(viewport_height);
    let from_bottom = app.transcript_scroll_offset(maximum_top);
    let top = maximum_top.saturating_sub(from_bottom);
    let viewport_end = top.saturating_add(viewport_height);
    let first_visible = index.entries.partition_point(|entry| entry.end <= top);
    let mut regions = Vec::new();
    let mut outputs = Vec::new();
    let mut output_scroll_metrics = Vec::new();

    for (entry_index, measured) in index.entries.iter().enumerate().skip(first_visible) {
        if measured.start >= viewport_end {
            break;
        }
        let visible_start = measured.start.max(top);
        let visible_end = measured.end.min(viewport_end);
        if visible_start >= visible_end {
            continue;
        }

        let entry = &app.transcript.entries()[entry_index];
        let renderer = EntryRenderer::new(entry, app, cache, width, available_height);
        if let Some(metrics) = renderer.output_scroll_metrics(width) {
            output_scroll_metrics.push(metrics);
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
        let key = SliceKey {
            revision: entry.revision(),
            width,
            available_height,
            expanded: renderer.expanded,
            output_scroll_from_bottom: renderer.output_scroll_from_bottom,
            theme: theme.id(),
            visible_start: local.start,
            visible_height: local.len(),
        };
        let cached = renderer
            .cacheable()
            .then(|| cache.slice(entry.id, key))
            .flatten();
        let slice = cached.unwrap_or_else(|| {
            cache.record_slice_build();
            let slice_area = Rect::new(0, 0, area.width, area.height);
            let mut buffer = Buffer::empty(slice_area);
            renderer.render(RenderContext {
                theme,
                area: slice_area,
                buffer: &mut buffer,
                visible_rows: local.clone(),
            });
            if renderer.cacheable() {
                cache.store_slice(entry.id, key, buffer)
            } else {
                Arc::new(buffer)
            }
        });
        copy_buffer(&slice, frame.buffer_mut(), area);
        cache.record_visible_rows(local.len());

        if renderer.clickable(width) {
            regions.push(EntryRegion { id: entry.id, area });
        }
        if let Some(output) = renderer.output_viewport(width) {
            let output_start = output.start.max(local.start);
            let output_end = output.end.min(local.end);
            if output_start < output_end {
                outputs.push(OutputRegion {
                    id: entry.id,
                    area: Rect::new(
                        area.x,
                        area.y.saturating_add(
                            output_start
                                .saturating_sub(local.start)
                                .min(u16::MAX as usize) as u16,
                        ),
                        area.width,
                        output_end
                            .saturating_sub(output_start)
                            .min(u16::MAX as usize) as u16,
                    ),
                });
            }
        }
    }

    RenderResult {
        entries: regions,
        outputs,
        output_scroll_metrics,
        scroll_maximum: maximum_top,
    }
}

fn ensure_layout_index(
    app: &App,
    theme: &Theme,
    width: usize,
    available_height: usize,
    cache: &TranscriptRenderCache,
) {
    let key = IndexKey {
        width,
        available_height,
        theme: theme.id(),
    };
    let entries = app.transcript.entries();
    let valid_prefix = {
        let index = cache.index.borrow();
        if index.key.is_some_and(|previous| previous.width != width) {
            0
        } else {
            let available_height_changed = index
                .key
                .is_some_and(|previous| previous.available_height != available_height);
            index
                .entries
                .iter()
                .zip(entries)
                .take_while(|(cached, entry)| {
                    cached.id == entry.id
                        && cached.revision == entry.revision()
                        && cached.expanded == app.transcript_entry_is_expanded(entry)
                        && !(available_height_changed
                            && entry_height_depends_on_available_height(entry, app))
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
        let height = measured_entry_height(entry, app, width, available_height, cache);
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

fn entry_height_depends_on_available_height(entry: &Entry, app: &App) -> bool {
    app.transcript_entry_is_expanded(entry)
        && matches!(
            &entry.kind,
            EntryKind::Tool(tool)
                if !tool_is_summary_only(tool) && output_artifacts(tool).next().is_some()
        )
}

fn measured_entry_height(
    entry: &Entry,
    app: &App,
    width: usize,
    available_height: usize,
    cache: &TranscriptRenderCache,
) -> usize {
    let renderer = EntryRenderer::new(entry, app, cache, width, available_height);
    let key = HeightKey {
        revision: entry.revision(),
        width,
        available_height,
        expanded: renderer.expanded,
    };
    if let Some(height) = cache.height(entry.id, key) {
        return height;
    }
    let height = renderer.height(width);
    cache.store_height(entry.id, key, height);
    height
}

impl Render for UserMessage {
    fn height(&self, width: usize) -> usize {
        self.content
            .layout(width.saturating_sub(4).max(1))
            .total_rows()
            .saturating_add(2)
    }

    fn render(&self, context: RenderContext<'_>) {
        let layout = self
            .content
            .layout((context.area.width as usize).saturating_sub(4).max(1));
        let rows = visible_user_lines(&layout, context.visible_rows, context.theme);
        render_rows_at_top(&rows, context.area, context.buffer);
    }

    fn clickable(&self, _width: usize) -> bool {
        true
    }
}

struct AssistantRenderer<'a> {
    message: &'a AssistantMessage,
    layout: Option<&'a MarkdownLayout>,
}

impl Render for AssistantRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let header = LinesRenderer::new(assistant_header(&theme));
        let footer = LinesRenderer::new(assistant_footer(&theme));
        let body = match &self.message.status {
            AssistantStatus::Queued => LinesRenderer::new(vec![Line::styled(
                "│ queued…",
                theme.style(ThemeRole::Accent),
            )])
            .height(width),
            AssistantStatus::Thinking => LinesRenderer::new(vec![Line::styled(
                "│ thinking…",
                theme.style(ThemeRole::Accent),
            )])
            .height(width),
            AssistantStatus::Streaming | AssistantStatus::Completed => {
                self.layout.map_or(1, MarkdownMessageRenderer::height)
            }
            AssistantStatus::Interrupted => self
                .layout
                .map_or(1, MarkdownMessageRenderer::height)
                .saturating_add(
                    LinesRenderer::new(vec![Line::styled(
                        "│ [interrupted]",
                        theme.style(ThemeRole::Warning),
                    )])
                    .height(width),
                ),
            AssistantStatus::Failed(message) => LinesRenderer::new(vec![Line::styled(
                format!("│ [failed: {message}]"),
                theme.style(ThemeRole::Warning),
            )])
            .height(width),
        };
        header
            .height(width)
            .saturating_add(body)
            .saturating_add(footer.height(width))
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let mut cursor = render_child(
            &LinesRenderer::new(assistant_header(context.theme)),
            0,
            &mut context,
        );
        cursor = match &self.message.status {
            AssistantStatus::Queued => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    "│ queued…",
                    context.theme.style(ThemeRole::Accent),
                )]),
                cursor,
                &mut context,
            ),
            AssistantStatus::Thinking => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    "│ thinking…",
                    context.theme.style(ThemeRole::Accent),
                )]),
                cursor,
                &mut context,
            ),
            AssistantStatus::Streaming | AssistantStatus::Completed => {
                self.layout.map_or(cursor, |layout| {
                    render_child(&MarkdownMessageRenderer { layout }, cursor, &mut context)
                })
            }
            AssistantStatus::Interrupted => {
                let cursor = self.layout.map_or(cursor, |layout| {
                    render_child(&MarkdownMessageRenderer { layout }, cursor, &mut context)
                });
                render_child(
                    &LinesRenderer::new(vec![Line::styled(
                        "│ [interrupted]",
                        context.theme.style(ThemeRole::Warning),
                    )]),
                    cursor,
                    &mut context,
                )
            }
            AssistantStatus::Failed(message) => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    format!("│ [failed: {message}]"),
                    context.theme.style(ThemeRole::Warning),
                )]),
                cursor,
                &mut context,
            ),
        };
        render_child(
            &LinesRenderer::new(assistant_footer(context.theme)),
            cursor,
            &mut context,
        );
    }
}

struct MarkdownMessageRenderer<'a> {
    layout: &'a MarkdownLayout,
}

impl MarkdownMessageRenderer<'_> {
    fn height(layout: &MarkdownLayout) -> usize {
        layout.height()
    }
}

impl Render for MarkdownMessageRenderer<'_> {
    fn height(&self, _width: usize) -> usize {
        self.layout.height()
    }

    fn render(&self, context: RenderContext<'_>) {
        let start = context.visible_rows.start.min(self.layout.height());
        let end = context.visible_rows.end.min(self.layout.height());
        for (destination_row, source_row) in (start..end).enumerate() {
            let Some(line) = self.layout.line(source_row, context.theme) else {
                continue;
            };
            let mut spans = vec![Span::styled("│ ", context.theme.style(ThemeRole::Text))];
            spans.extend(line.spans);
            context.buffer.set_line(
                context.area.x,
                context.area.y.saturating_add(destination_row as u16),
                &Line::from(spans),
                context.area.width,
            );
        }
    }
}

struct ReasoningRenderer<'a> {
    reasoning: &'a Reasoning,
    expanded: bool,
    animation_frame: usize,
}

impl Render for ReasoningRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let header = LinesRenderer::new(reasoning_header(self, &theme));
        let footer = LinesRenderer::new(vec![Line::styled("└", theme.style(ThemeRole::Accent))]);
        let body = if self.expanded {
            if self.reasoning.summary.is_empty() {
                LinesRenderer::new(vec![reasoning_empty_line(self, &theme)]).height(width)
            } else {
                MessageRenderer::new(&self.reasoning.summary, ThemeRole::MutedText).height(width)
            }
        } else {
            LinesRenderer::new(vec![reasoning_collapsed_line(self, &theme)]).height(width)
        };
        header
            .height(width)
            .saturating_add(body)
            .saturating_add(footer.height(width))
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let mut cursor = render_child(
            &LinesRenderer::new(reasoning_header(self, context.theme)),
            0,
            &mut context,
        );
        cursor = if self.expanded && !self.reasoning.summary.is_empty() {
            render_child(
                &MessageRenderer::new(&self.reasoning.summary, ThemeRole::MutedText),
                cursor,
                &mut context,
            )
        } else {
            let body = if self.expanded {
                reasoning_empty_line(self, context.theme)
            } else {
                reasoning_collapsed_line(self, context.theme)
            };
            render_child(&LinesRenderer::new(vec![body]), cursor, &mut context)
        };
        render_child(
            &LinesRenderer::new(vec![Line::styled(
                "└",
                context.theme.style(ThemeRole::Accent),
            )]),
            cursor,
            &mut context,
        );
    }

    fn cacheable(&self) -> bool {
        self.reasoning.status != ActivityStatus::Running
    }

    fn clickable(&self, _width: usize) -> bool {
        true
    }
}

struct ToolRenderer<'a> {
    tool: &'a ToolCall,
    expanded: bool,
    available_height: usize,
    scroll_from_bottom: usize,
    body_layout: Option<&'a ToolOutputBodyLayout>,
    cache: Option<&'a TranscriptRenderCache>,
}

impl Render for ToolRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        if tool_is_summary_only(self.tool) {
            return LinesRenderer::new(tool_summary_lines(self.tool, &Theme::default()))
                .height(width);
        }
        let Some(layout) = self.output_layout(width) else {
            return LinesRenderer::new(generic_tool_lines(self.tool, &Theme::default()))
                .height(width);
        };
        layout.chrome_height.saturating_add(layout.viewport_height)
    }

    fn render(&self, mut context: RenderContext<'_>) {
        if tool_is_summary_only(self.tool) {
            LinesRenderer::new(tool_summary_lines(self.tool, context.theme)).render(context);
            return;
        }
        let Some(layout) = self.output_layout(context.area.width as usize) else {
            LinesRenderer::new(generic_tool_lines(self.tool, context.theme)).render(context);
            return;
        };
        let body = ToolOutputBodyRenderer { tool: self.tool };
        let scroll_from_bottom = self.scroll_from_bottom.min(layout.maximum);
        let header = LinesRenderer::new(output_tool_header_lines(
            self.tool,
            self.expanded,
            layout.can_expand,
            context.theme,
        ));
        let mut cursor = render_child(&header, 0, &mut context);
        cursor = render_child(
            &LinesRenderer::new(bounded_output_artifact_header_lines(
                self.tool,
                context.area.width as usize,
                layout.artifact_header_limit,
                context.theme,
            )),
            cursor,
            &mut context,
        );
        cursor = render_child(
            &OutputViewportRenderer {
                body,
                body_layout: self.body_layout,
                cache: self.cache,
                height: layout.viewport_height,
                scroll_from_bottom,
            },
            cursor,
            &mut context,
        );
        let footer = LinesRenderer::new(output_footer_lines(
            self.tool,
            layout.body_height,
            layout.viewport_height,
            scroll_from_bottom,
            context.area.width as usize,
            context.theme,
        ));
        render_child(&footer, cursor, &mut context);
    }

    fn clickable(&self, width: usize) -> bool {
        self.can_expand(width)
    }
}

const COMPACT_OUTPUT_ROWS: usize = 10;
const MAX_ARTIFACT_HEADER_ROWS: usize = 4;

impl ToolRenderer<'_> {
    fn output_viewport(&self, width: usize) -> Option<Range<usize>> {
        let layout = self.output_layout(width)?;
        let start = layout
            .header_height
            .saturating_add(layout.artifact_header_height);
        Some(start..start.saturating_add(layout.viewport_height))
    }

    fn can_expand(&self, width: usize) -> bool {
        self.output_layout(width)
            .is_some_and(|layout| layout.can_expand)
    }

    fn output_layout(&self, width: usize) -> Option<ToolOutputLayout> {
        if tool_is_summary_only(self.tool) {
            return None;
        }
        output_artifacts(self.tool).next()?;
        let body_height = self.body_layout.map_or_else(
            || ToolOutputBodyRenderer { tool: self.tool }.height(width),
            ToolOutputBodyLayout::height,
        );
        let base_header_height = LinesRenderer::new(output_tool_header_lines(
            self.tool,
            self.expanded,
            false,
            &Theme::default(),
        ))
        .height(width);
        let base_artifact_header_limit =
            artifact_header_limit(self.available_height, base_header_height);
        let base_artifact_header_height = LinesRenderer::new(bounded_output_artifact_header_lines(
            self.tool,
            width,
            base_artifact_header_limit,
            &Theme::default(),
        ))
        .height(width);
        let base_chrome = base_header_height
            .saturating_add(base_artifact_header_height)
            .saturating_add(1);
        let base_compact_height = output_viewport_height(false, self.available_height, base_chrome);
        let can_expand = body_height > base_compact_height;
        let header_height = LinesRenderer::new(output_tool_header_lines(
            self.tool,
            self.expanded,
            can_expand,
            &Theme::default(),
        ))
        .height(width);
        let artifact_header_limit = artifact_header_limit(self.available_height, header_height);
        let artifact_header_height = LinesRenderer::new(bounded_output_artifact_header_lines(
            self.tool,
            width,
            artifact_header_limit,
            &Theme::default(),
        ))
        .height(width);
        let chrome_height = header_height
            .saturating_add(artifact_header_height)
            .saturating_add(1);
        let viewport_height =
            output_viewport_height(self.expanded, self.available_height, chrome_height);
        Some(ToolOutputLayout {
            header_height,
            artifact_header_limit,
            artifact_header_height,
            chrome_height,
            viewport_height,
            body_height,
            maximum: body_height.saturating_sub(viewport_height),
            can_expand,
        })
    }
}

struct ToolOutputLayout {
    header_height: usize,
    artifact_header_limit: usize,
    artifact_header_height: usize,
    chrome_height: usize,
    viewport_height: usize,
    body_height: usize,
    maximum: usize,
    can_expand: bool,
}

fn artifact_header_limit(available_height: usize, header_height: usize) -> usize {
    let body_rows =
        COMPACT_OUTPUT_ROWS.min(available_height.saturating_sub(header_height.saturating_add(1)));
    available_height
        .saturating_sub(header_height.saturating_add(1).saturating_add(body_rows))
        .min(MAX_ARTIFACT_HEADER_ROWS)
}

struct ArtifactBodyRenderer<'a> {
    artifact: &'a ToolArtifact,
}

impl Render for ArtifactBodyRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        match self.artifact {
            ToolArtifact::Patch(artifact) => {
                wrapped_iter_height(patch_body_line_iter(artifact, &Theme::default()), width)
            }
            ToolArtifact::SearchResults(artifact) => {
                MessageRenderer::new(&artifact.matches, ThemeRole::MutedText).height(width)
            }
            ToolArtifact::Terminal(artifact) => {
                MessageRenderer::new(&artifact.output, ThemeRole::Text).height(width)
            }
            ToolArtifact::TextDetail(artifact) => {
                MessageRenderer::new(&artifact.text, ThemeRole::MutedText).height(width)
            }
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => 0,
        }
    }

    fn render(&self, context: RenderContext<'_>) {
        match self.artifact {
            ToolArtifact::Patch(artifact) => {
                render_wrapped_iter(patch_body_line_iter(artifact, context.theme), context)
            }
            ToolArtifact::SearchResults(artifact) => {
                MessageRenderer::new(&artifact.matches, ThemeRole::MutedText).render(context)
            }
            ToolArtifact::Terminal(artifact) => {
                MessageRenderer::new(&artifact.output, ThemeRole::Text).render(context)
            }
            ToolArtifact::TextDetail(artifact) => {
                MessageRenderer::new(&artifact.text, ThemeRole::MutedText).render(context)
            }
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => {}
        }
    }
}

struct ToolOutputBodyRenderer<'a> {
    tool: &'a ToolCall,
}

impl Render for ToolOutputBodyRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        output_artifacts(self.tool).fold(0usize, |height, artifact| {
            height.saturating_add(ArtifactBodyRenderer { artifact }.height(width))
        })
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let mut cursor = 0;
        for artifact in output_artifacts(self.tool) {
            cursor = render_child(&ArtifactBodyRenderer { artifact }, cursor, &mut context);
        }
    }
}

struct OutputViewportRenderer<'a> {
    body: ToolOutputBodyRenderer<'a>,
    body_layout: Option<&'a ToolOutputBodyLayout>,
    cache: Option<&'a TranscriptRenderCache>,
    height: usize,
    scroll_from_bottom: usize,
}

impl Render for OutputViewportRenderer<'_> {
    fn height(&self, _width: usize) -> usize {
        self.height
    }

    fn render(&self, context: RenderContext<'_>) {
        let body_height = self.body_layout.map_or_else(
            || self.body.height(context.area.width as usize),
            ToolOutputBodyLayout::height,
        );
        let maximum = body_height.saturating_sub(self.height);
        let top = maximum.saturating_sub(self.scroll_from_bottom.min(maximum));
        let source_start = top.saturating_add(context.visible_rows.start);
        let source_end = top
            .saturating_add(context.visible_rows.end)
            .min(body_height);
        if source_start < source_end {
            let body_context = RenderContext {
                theme: context.theme,
                area: context.area,
                buffer: context.buffer,
                visible_rows: source_start..source_end,
            };
            if let Some(layout) = self.body_layout {
                layout.render(body_context);
                if let Some(cache) = self.cache {
                    cache.record_tool_output_rows(source_end.saturating_sub(source_start));
                }
            } else {
                self.body.render(body_context);
            }
        }
    }
}

impl Render for RetryAttempt {
    fn height(&self, width: usize) -> usize {
        LinesRenderer::new(retry_lines(self, &Theme::default())).height(width)
    }

    fn render(&self, context: RenderContext<'_>) {
        LinesRenderer::new(retry_lines(self, context.theme)).render(context);
    }
}

impl Render for ToolArtifact {
    fn height(&self, width: usize) -> usize {
        self.renderer().height(width)
    }

    fn render(&self, context: RenderContext<'_>) {
        self.renderer().render(context);
    }
}

impl ToolArtifact {
    fn renderer(&self) -> &dyn Render {
        match self {
            Self::CodeRange(artifact) => artifact,
            Self::Patch(artifact) => artifact,
            Self::SearchResults(artifact) => artifact,
            Self::Terminal(artifact) => artifact,
            Self::TextDetail(artifact) => artifact,
            Self::FileReference(artifact) => artifact,
        }
    }
}

impl Render for CodeRangeArtifact {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let mut height = LinesRenderer::new(code_range_header(self, &theme)).height(width);
        if let Some(preview) = &self.preview {
            height = height
                .saturating_add(MessageRenderer::new(preview, ThemeRole::MutedText).height(width));
        }
        height
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let cursor = render_child(
            &LinesRenderer::new(code_range_header(self, context.theme)),
            0,
            &mut context,
        );
        if let Some(preview) = &self.preview {
            render_child(
                &MessageRenderer::new(preview, ThemeRole::MutedText),
                cursor,
                &mut context,
            );
        }
    }
}

impl Render for PatchArtifact {
    fn height(&self, width: usize) -> usize {
        wrapped_iter_height(patch_line_iter(self, &Theme::default()), width)
    }

    fn render(&self, context: RenderContext<'_>) {
        render_wrapped_iter(patch_line_iter(self, context.theme), context);
    }
}

impl Render for SearchResultsArtifact {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        LinesRenderer::new(search_results_header(self, &theme))
            .height(width)
            .saturating_add(MessageRenderer::new(&self.matches, ThemeRole::MutedText).height(width))
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let cursor = render_child(
            &LinesRenderer::new(search_results_header(self, context.theme)),
            0,
            &mut context,
        );
        render_child(
            &MessageRenderer::new(&self.matches, ThemeRole::MutedText),
            cursor,
            &mut context,
        );
    }
}

impl Render for TerminalArtifact {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let mut height = LinesRenderer::new(terminal_header(self, &theme)).height(width);
        if !self.output.is_empty() {
            height = height
                .saturating_add(MessageRenderer::new(&self.output, ThemeRole::Text).height(width));
        }
        if let Some(exit_code) = self.exit_code {
            height = height.saturating_add(
                LinesRenderer::new(vec![terminal_exit_line(exit_code, &theme)]).height(width),
            );
        }
        height
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let mut cursor = render_child(
            &LinesRenderer::new(terminal_header(self, context.theme)),
            0,
            &mut context,
        );
        if !self.output.is_empty() {
            cursor = render_child(
                &MessageRenderer::new(&self.output, ThemeRole::Text),
                cursor,
                &mut context,
            );
        }
        if let Some(exit_code) = self.exit_code {
            render_child(
                &LinesRenderer::new(vec![terminal_exit_line(exit_code, context.theme)]),
                cursor,
                &mut context,
            );
        }
    }
}

impl Render for TextDetailArtifact {
    fn height(&self, width: usize) -> usize {
        MessageRenderer::new(&self.text, ThemeRole::MutedText).height(width)
    }

    fn render(&self, context: RenderContext<'_>) {
        MessageRenderer::new(&self.text, ThemeRole::MutedText).render(context);
    }
}

impl Render for FileReferenceArtifact {
    fn height(&self, width: usize) -> usize {
        LinesRenderer::new(file_reference_lines(self, &Theme::default())).height(width)
    }

    fn render(&self, context: RenderContext<'_>) {
        LinesRenderer::new(file_reference_lines(self, context.theme)).render(context);
    }
}

struct LinesRenderer {
    lines: Vec<Line<'static>>,
}

impl LinesRenderer {
    fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }
}

impl Render for LinesRenderer {
    fn height(&self, width: usize) -> usize {
        wrapped_iter_height(self.lines.iter().cloned(), width)
    }

    fn render(&self, context: RenderContext<'_>) {
        render_wrapped_iter(self.lines.iter().cloned(), context);
    }
}

struct MessageRenderer<'a> {
    text: &'a str,
    role: ThemeRole,
}

impl<'a> MessageRenderer<'a> {
    fn new(text: &'a str, role: ThemeRole) -> Self {
        Self { text, role }
    }
}

impl Render for MessageRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        wrapped_iter_height(
            message_line_iter(self.text, ratatui::style::Style::default()),
            width,
        )
    }

    fn render(&self, context: RenderContext<'_>) {
        render_wrapped_iter(
            message_line_iter(self.text, context.theme.style(self.role)),
            context,
        );
    }
}

fn render_child(child: &impl Render, start: usize, context: &mut RenderContext<'_>) -> usize {
    let height = child.height(context.area.width as usize);
    let end = start.saturating_add(height);
    let visible_start = start.max(context.visible_rows.start);
    let visible_end = end.min(context.visible_rows.end);
    if visible_start < visible_end {
        let destination_offset = visible_start.saturating_sub(context.visible_rows.start);
        let area = Rect::new(
            context.area.x,
            context
                .area
                .y
                .saturating_add(destination_offset.min(u16::MAX as usize) as u16),
            context.area.width,
            (visible_end - visible_start).min(u16::MAX as usize) as u16,
        );
        child.render(RenderContext {
            theme: context.theme,
            area,
            buffer: &mut *context.buffer,
            visible_rows: visible_start.saturating_sub(start)..visible_end.saturating_sub(start),
        });
    }
    end
}

fn visible_user_lines(
    layout: &crate::composer::ComposerLayout,
    range: Range<usize>,
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
        .collect()
}

fn assistant_header(theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        "┌─ funcode",
        theme
            .style(ThemeRole::Agent)
            .add_modifier(ratatui::style::Modifier::BOLD),
    )]
}

fn assistant_footer(theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        "└",
        theme
            .style(ThemeRole::Agent)
            .add_modifier(ratatui::style::Modifier::BOLD),
    )]
}

fn reasoning_header(renderer: &ReasoningRenderer<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let status = status_label(&renderer.reasoning.status);
    vec![Line::from(vec![
        Span::styled("┌─ thinking", theme.style(ThemeRole::Accent)),
        Span::styled(
            format!(
                " · {status} · click to {}",
                if renderer.expanded {
                    "collapse"
                } else {
                    "expand"
                }
            ),
            theme.style(ThemeRole::MutedText),
        ),
    ])]
}

fn reasoning_empty_line(renderer: &ReasoningRenderer<'_>, theme: &Theme) -> Line<'static> {
    if renderer.reasoning.status == ActivityStatus::Running {
        Line::styled(
            format!("│ Working{}", spinner(renderer.animation_frame)),
            theme.style(ThemeRole::Accent),
        )
    } else {
        Line::styled(
            "│ No reasoning summary was provided",
            theme.style(ThemeRole::MutedText),
        )
    }
}

fn reasoning_collapsed_line(renderer: &ReasoningRenderer<'_>, theme: &Theme) -> Line<'static> {
    if renderer.reasoning.status == ActivityStatus::Running {
        Line::styled(
            format!("│ Thinking{}", spinner(renderer.animation_frame)),
            theme.style(ThemeRole::Accent),
        )
    } else {
        Line::styled("│ Summary hidden", theme.style(ThemeRole::MutedText))
    }
}

fn output_artifacts(tool: &ToolCall) -> impl Iterator<Item = &ToolArtifact> {
    tool.artifacts.iter().filter(|artifact| {
        matches!(
            artifact,
            ToolArtifact::Patch(_)
                | ToolArtifact::SearchResults(_)
                | ToolArtifact::Terminal(_)
                | ToolArtifact::TextDetail(_)
        )
    })
}

fn build_tool_output_layout(tool: &ToolCall, width: usize) -> ToolOutputBodyLayout {
    let mut layout = ToolOutputBodyLayout::default();
    let mut logical_line = 0usize;
    for artifact in output_artifacts(tool) {
        match artifact {
            ToolArtifact::Patch(artifact) => {
                for source in artifact.diff.lines() {
                    let role = if source.starts_with('+') && !source.starts_with("+++") {
                        ThemeRole::DiffAdded
                    } else if source.starts_with('-') && !source.starts_with("---") {
                        ThemeRole::DiffRemoved
                    } else {
                        ThemeRole::MutedText
                    };
                    let line = Line::from(format!(
                        "│ {}",
                        crate::composer::safe_single_line(source, 2)
                    ));
                    push_indexed_output_line(&mut layout, line, role, logical_line, width);
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::SearchResults(artifact) => {
                for line in message_line_iter(&artifact.matches, ratatui::style::Style::default()) {
                    push_indexed_output_line(
                        &mut layout,
                        line,
                        ThemeRole::MutedText,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::Terminal(artifact) => {
                for line in message_line_iter(&artifact.output, ratatui::style::Style::default()) {
                    push_indexed_output_line(
                        &mut layout,
                        line,
                        ThemeRole::Text,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::TextDetail(artifact) => {
                for line in message_line_iter(&artifact.text, ratatui::style::Style::default()) {
                    push_indexed_output_line(
                        &mut layout,
                        line,
                        ThemeRole::MutedText,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => {}
        }
    }
    layout.logical_lines = layout
        .rows
        .iter()
        .map(|row| row.logical_line)
        .collect::<Vec<_>>()
        .into();
    layout
}

fn push_indexed_output_line(
    layout: &mut ToolOutputBodyLayout,
    line: Line<'static>,
    role: ThemeRole,
    logical_line: usize,
    width: usize,
) {
    layout.rows.extend(
        wrap_lines(&[line], width)
            .into_iter()
            .map(|line| ToolOutputRow {
                line,
                role,
                logical_line,
            }),
    );
}

fn output_viewport_height(expanded: bool, available_height: usize, chrome: usize) -> usize {
    let available = available_height.saturating_sub(chrome);
    if expanded {
        available
    } else {
        COMPACT_OUTPUT_ROWS.min(available)
    }
}

fn output_tool_header_lines(
    tool: &ToolCall,
    expanded: bool,
    can_expand: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let title = if tool.name == "terminal" {
        "┌─ terminal".to_owned()
    } else {
        format!("┌─ tool · {}", tool.name)
    };
    let action = can_expand.then_some(if expanded { "collapse" } else { "expand" });
    vec![Line::from(vec![
        Span::styled(title, theme.style(ThemeRole::Accent)),
        Span::styled(
            format!(" · {}", status_label(&tool.status)),
            theme.style(ThemeRole::MutedText),
        ),
        Span::styled(
            action.map_or_else(String::new, |action| format!(" · click to {action}")),
            theme.style(ThemeRole::MutedText),
        ),
    ])]
}

fn output_artifact_header_lines(tool: &ToolCall, theme: &Theme) -> Vec<Line<'static>> {
    output_artifacts(tool)
        .flat_map(|artifact| match artifact {
            ToolArtifact::Patch(artifact) => vec![Line::styled(
                format!("│ Edited {}", artifact.path.display()),
                theme.style(ThemeRole::Accent),
            )],
            ToolArtifact::SearchResults(artifact) => search_results_header(artifact, theme),
            ToolArtifact::Terminal(artifact) => terminal_header(artifact, theme),
            ToolArtifact::TextDetail(_) => Vec::new(),
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => Vec::new(),
        })
        .collect()
}

fn bounded_output_artifact_header_lines(
    tool: &ToolCall,
    width: usize,
    maximum_rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if maximum_rows == 0 {
        return Vec::new();
    }
    let mut rows = wrap_lines(&output_artifact_header_lines(tool, theme), width);
    if rows.len() <= maximum_rows {
        return rows;
    }
    rows.truncate(maximum_rows);
    if let Some(last) = rows.last_mut() {
        *last = Line::styled(
            if width >= 3 { "│ …" } else { "…" },
            theme.style(ThemeRole::MutedText),
        );
    }
    rows
}

fn output_footer_lines(
    tool: &ToolCall,
    body_height: usize,
    viewport_height: usize,
    scroll_from_bottom: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let maximum = body_height.saturating_sub(viewport_height);
    let from_bottom = scroll_from_bottom.min(maximum);
    let top = maximum.saturating_sub(from_bottom);
    let visible_end = top.saturating_add(viewport_height).min(body_height);
    let mut spans = vec![Span::styled("└", theme.style(ThemeRole::Accent))];
    if let Some(exit_code) = output_artifacts(tool)
        .filter_map(|artifact| match artifact {
            ToolArtifact::Terminal(TerminalArtifact { exit_code, .. }) => *exit_code,
            _ => None,
        })
        .last()
    {
        spans.push(Span::styled(
            format!(" exit {exit_code}"),
            if exit_code == 0 {
                theme.style(ThemeRole::DiffAdded)
            } else {
                theme.style(ThemeRole::Warning)
            },
        ));
    }
    if maximum > 0 {
        let full_state = if from_bottom == 0 {
            "latest".to_owned()
        } else {
            "paused · End to follow".to_owned()
        };
        let full_indicator = format!(
            " · lines {}-{visible_end}/{body_height} · {full_state}",
            top + 1
        );
        let used = spans.iter().fold(0usize, |used, span| {
            used.saturating_add(UnicodeWidthStr::width(span.content.as_ref()))
        });
        let indicator =
            if used.saturating_add(UnicodeWidthStr::width(full_indicator.as_str())) <= width {
                full_indicator
            } else if from_bottom == 0 {
                format!(" · {visible_end}/{body_height} · latest")
            } else {
                " · paused · End to follow".to_owned()
            };
        spans.push(Span::styled(indicator, theme.style(ThemeRole::MutedText)));
    }
    vec![Line::from(spans)]
}

fn generic_tool_lines(tool: &ToolCall, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            format!("Tool {}: ", tool.name),
            theme.style(ThemeRole::Accent),
        ),
        Span::styled(
            crate::composer::safe_single_line(&tool.summary, 2),
            theme.style(ThemeRole::Text),
        ),
        Span::styled(
            format!(" · {}", status_label(&tool.status)),
            theme.style(ThemeRole::MutedText),
        ),
    ])]
}

fn tool_is_summary_only(tool: &ToolCall) -> bool {
    tool.name == "read_file"
        || tool.artifacts.iter().any(|artifact| {
            matches!(
                artifact,
                ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_)
            )
        })
}

fn tool_summary_lines(tool: &ToolCall, theme: &Theme) -> Vec<Line<'static>> {
    let path = tool.artifacts.iter().find_map(|artifact| match artifact {
        ToolArtifact::CodeRange(artifact) => Some(artifact.path.display()),
        ToolArtifact::FileReference(artifact) => Some(artifact.path.display()),
        _ => None,
    });
    let path = path.unwrap_or_else(|| {
        tool.summary
            .strip_prefix("Reading ")
            .unwrap_or(&tool.summary)
            .to_owned()
    });
    let status = match &tool.status {
        ActivityStatus::Completed => String::new(),
        ActivityStatus::Running => " · running".to_owned(),
        ActivityStatus::Interrupted => " · interrupted".to_owned(),
        ActivityStatus::Failed(message) => format!(" · failed: {message}"),
    };
    vec![Line::from(vec![
        Span::styled("Read File: ", theme.style(ThemeRole::Accent)),
        Span::styled(path, theme.style(ThemeRole::Text)),
        Span::styled(status, theme.style(ThemeRole::MutedText)),
    ])]
}

fn retry_lines(retry: &RetryAttempt, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("↻ ", theme.style(ThemeRole::Accent)),
        Span::styled(
            format!("Attempt {}/{} failed: ", retry.attempt, retry.max_retries),
            theme.style(ThemeRole::Warning),
        ),
        Span::styled(retry.message.clone(), theme.style(ThemeRole::Warning)),
        Span::styled(" · Retrying…", theme.style(ThemeRole::Accent)),
    ])]
}

fn code_range_header(artifact: &CodeRangeArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!(
            "│ Read {}:{}-{}",
            artifact.path.display(),
            artifact.start_line,
            artifact.end_line
        ),
        theme.style(ThemeRole::Accent),
    )]
}

fn patch_line_iter<'a>(
    artifact: &'a PatchArtifact,
    theme: &'a Theme,
) -> impl Iterator<Item = Line<'static>> + 'a {
    std::iter::once(Line::styled(
        format!("│ Edited {}", artifact.path.display()),
        theme.style(ThemeRole::Accent),
    ))
    .chain(patch_body_line_iter(artifact, theme))
}

fn patch_body_line_iter<'a>(
    artifact: &'a PatchArtifact,
    theme: &'a Theme,
) -> impl Iterator<Item = Line<'static>> + 'a {
    artifact.diff.lines().map(|line| {
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
    })
}

fn search_results_header(artifact: &SearchResultsArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!(
            "│ Search /{}/",
            crate::composer::safe_single_line(&artifact.query, 10)
        ),
        theme.style(ThemeRole::Accent),
    )]
}

fn terminal_header(artifact: &TerminalArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            format!(
                "│ # {}",
                crate::composer::safe_single_line(&artifact.description, 2)
            ),
            theme.style(ThemeRole::MutedText),
        ),
        Line::styled(
            format!(
                "│ $ {}",
                crate::composer::safe_single_line(&artifact.command, 4)
            ),
            theme.style(ThemeRole::Text),
        ),
    ]
}

fn terminal_exit_line(exit_code: i32, theme: &Theme) -> Line<'static> {
    Line::styled(
        format!("│ exit {exit_code}"),
        if exit_code == 0 {
            theme.style(ThemeRole::DiffAdded)
        } else {
            theme.style(ThemeRole::Warning)
        },
    )
}

fn file_reference_lines(artifact: &FileReferenceArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!("│ File {}", artifact.path.display()),
        theme.style(ThemeRole::Accent),
    )]
}

fn message_line_iter(
    text: &str,
    style: ratatui::style::Style,
) -> Box<dyn Iterator<Item = Line<'static>> + '_> {
    if text.is_empty() {
        return Box::new(std::iter::once(Line::styled("│", style)));
    }
    Box::new(text.split('\n').enumerate().flat_map(move |(index, text)| {
        crate::composer::SubmittedContent::plain(text)
            .display_lines(if index == 0 { 2 } else { 0 })
            .into_iter()
            .map(move |line| {
                let mut spans = vec![Span::styled("│ ", style)];
                spans.extend(
                    line.runs
                        .into_iter()
                        .map(|run| Span::styled(run.text, style)),
                );
                Line::from(spans)
            })
    }))
}

fn wrapped_iter_height(lines: impl IntoIterator<Item = Line<'static>>, width: usize) -> usize {
    lines.into_iter().fold(0usize, |height, line| {
        height.saturating_add(wrapped_line_height(&line, width))
    })
}

fn render_wrapped_iter(lines: impl IntoIterator<Item = Line<'static>>, context: RenderContext<'_>) {
    let mut logical_row = 0usize;
    for line in lines {
        let line_height = wrapped_line_height(&line, context.area.width as usize);
        let line_end = logical_row.saturating_add(line_height);
        if line_end <= context.visible_rows.start {
            logical_row = line_end;
            continue;
        }
        if logical_row >= context.visible_rows.end {
            return;
        }
        let wrapped = wrap_lines(std::slice::from_ref(&line), context.area.width as usize);
        let visible_start = context.visible_rows.start.saturating_sub(logical_row);
        let visible_end = context
            .visible_rows
            .end
            .saturating_sub(logical_row)
            .min(wrapped.len());
        for (local_row, row) in wrapped[visible_start..visible_end].iter().enumerate() {
            let global_row = logical_row
                .saturating_add(visible_start)
                .saturating_add(local_row);
            let destination_row = global_row.saturating_sub(context.visible_rows.start);
            if destination_row < context.area.height as usize {
                context.buffer.set_line(
                    context.area.x,
                    context.area.y.saturating_add(destination_row as u16),
                    row,
                    context.area.width,
                );
            }
        }
        logical_row = line_end;
    }
}

fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
    let width = width.max(1);
    let mut height = 1usize;
    let mut column = 0usize;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if column > 0 && column.saturating_add(grapheme_width) > width {
                height = height.saturating_add(1);
                column = 0;
            }
            column = column.saturating_add(grapheme_width);
        }
    }
    height
}

fn render_rows_at_top(rows: &[Line<'static>], area: Rect, buffer: &mut Buffer) {
    for (row, line) in rows.iter().enumerate().take(area.height as usize) {
        buffer.set_line(area.x, area.y.saturating_add(row as u16), line, area.width);
    }
}

fn copy_buffer(source: &Buffer, destination: &mut Buffer, area: Rect) {
    for row in 0..area.height {
        for column in 0..area.width {
            let Some(source_cell) = source.cell((column, row)) else {
                continue;
            };
            if let Some(destination_cell) =
                destination.cell_mut((area.x.saturating_add(column), area.y.saturating_add(row)))
            {
                *destination_cell = source_cell.clone();
            }
        }
    }
}

fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = Vec::new();
    for line in lines {
        let mut spans: Vec<Span<'static>> = Vec::new();
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

fn status_label(status: &ActivityStatus) -> String {
    status.to_string()
}

fn spinner(frame: usize) -> &'static str {
    ["|", "/", "-", "\\"][(frame / 2) % 4]
}

#[cfg(test)]
mod tests {
    use super::{
        AssistantRenderer, ReasoningRenderer, Render, RenderContext, ToolRenderer,
        TranscriptRenderCache, render,
    };
    use crate::ui::markdown::MarkdownLayout;
    use crate::{
        agent::AgentEvent,
        app::App,
        composer::SubmittedContent,
        theme::{Theme, ThemeId, ThemeRole},
        transcript::{
            ActivityStatus, AssistantMessage, AssistantStatus, CodeRangeArtifact,
            FileReferenceArtifact, PatchArtifact, Reasoning, RetryAttempt, SearchResultsArtifact,
            TerminalArtifact, TextDetailArtifact, ToolArtifact, ToolCall, TranscriptEvent,
            UserMessage,
        },
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};

    fn render_widget(widget: &dyn Render, width: u16) -> Buffer {
        let theme = Theme::default();
        let height = widget.height(width as usize).min(u16::MAX as usize) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                widget.render(RenderContext {
                    theme: &theme,
                    area,
                    buffer: frame.buffer_mut(),
                    visible_rows: 0..height as usize,
                });
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_artifact(artifact: &ToolArtifact, width: u16) -> Buffer {
        render_widget(artifact, width)
    }

    fn render_transcript(app: &App, cache: &TranscriptRenderCache) -> String {
        render_transcript_with_theme(app, cache, &Theme::default())
    }

    fn render_transcript_with_theme(
        app: &App,
        cache: &TranscriptRenderCache,
        theme: &Theme,
    ) -> String {
        render_transcript_at(app, cache, theme, 80, 24).0
    }

    fn render_transcript_at(
        app: &App,
        cache: &TranscriptRenderCache,
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> (String, super::RenderResult) {
        let area = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut result = None;
        terminal
            .draw(|frame| {
                result = Some(render(frame, area, app, theme, cache));
            })
            .unwrap();
        (
            terminal.backend().to_string(),
            result.expect("transcript render result"),
        )
    }

    fn symbols(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_artifact_renders_its_specialized_content() {
        let artifacts = [
            ToolArtifact::CodeRange(CodeRangeArtifact {
                path: "src/main.rs".into(),
                start_line: 1,
                end_line: 2,
                preview: Some("fn main() {}".into()),
            }),
            ToolArtifact::Patch(PatchArtifact {
                path: "src/main.rs".into(),
                diff: "-old\n+new".into(),
            }),
            ToolArtifact::SearchResults(SearchResultsArtifact {
                query: "main".into(),
                matches: "src/main.rs:1".into(),
            }),
            ToolArtifact::Terminal(TerminalArtifact {
                description: "Run tests".into(),
                command: "cargo test".into(),
                output: "ok".into(),
                exit_code: Some(0),
            }),
            ToolArtifact::TextDetail(TextDetailArtifact {
                text: "details".into(),
            }),
            ToolArtifact::FileReference(FileReferenceArtifact {
                path: "Cargo.toml".into(),
            }),
        ];
        let expected: [&[&str]; 6] = [
            &["Read src/main.rs:1-2", "fn main() {}"],
            &["Edited src/main.rs", "-old", "+new"],
            &["Search /main/", "src/main.rs:1"],
            &["# Run tests", "$ cargo test", "ok", "exit 0"],
            &["details"],
            &["File Cargo.toml"],
        ];

        for (artifact, expected) in artifacts.iter().zip(expected) {
            let rendered = symbols(&render_artifact(artifact, 80));
            for expected in expected {
                assert!(
                    rendered.contains(expected),
                    "missing {expected:?} in {rendered:?}"
                );
            }
        }
    }

    #[test]
    fn entries_render_status_expansion_and_retry_states() {
        let assistant_cases = [
            (AssistantStatus::Queued, "queued"),
            (AssistantStatus::Thinking, "thinking"),
            (AssistantStatus::Streaming, "response"),
            (AssistantStatus::Completed, "response"),
            (AssistantStatus::Interrupted, "interrupted"),
            (AssistantStatus::Failed("offline".into()), "failed: offline"),
        ];
        for (status, expected) in assistant_cases {
            let assistant = AssistantMessage {
                text: "response".into(),
                status,
            };
            let layout = MarkdownLayout::new(&assistant.text, 78);
            let renderer = AssistantRenderer {
                message: &assistant,
                layout: Some(&layout),
            };
            assert!(symbols(&render_widget(&renderer, 80)).contains(expected));
        }

        let reasoning = Reasoning {
            summary: "checked the code".into(),
            status: ActivityStatus::Completed,
        };
        let collapsed = ReasoningRenderer {
            reasoning: &reasoning,
            expanded: false,
            animation_frame: 0,
        };
        let expanded = ReasoningRenderer {
            reasoning: &reasoning,
            expanded: true,
            animation_frame: 0,
        };
        assert!(symbols(&render_widget(&collapsed, 80)).contains("Summary hidden"));
        assert!(symbols(&render_widget(&expanded, 80)).contains("checked the code"));

        let tool = ToolCall {
            call_id: 1,
            name: "details".into(),
            summary: "Showing details".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::TextDetail(TextDetailArtifact {
                text: "file contents".into(),
            })],
        };
        let collapsed = ToolRenderer {
            tool: &tool,
            expanded: false,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        let expanded = ToolRenderer {
            tool: &tool,
            expanded: true,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        assert!(symbols(&render_widget(&collapsed, 80)).contains("file contents"));
        assert!(symbols(&render_widget(&expanded, 80)).contains("file contents"));

        let user = UserMessage {
            content: SubmittedContent::plain("prompt"),
        };
        let retry = RetryAttempt {
            attempt: 2,
            max_retries: 3,
            message: "timeout".into(),
        };
        let user = render_widget(&user, 80);
        assert!(symbols(&user).contains("you"));
        let header = user.cell((3, 0)).expect("user header");
        let body = user.cell((2, 1)).expect("user message body");
        assert_eq!(
            header.style().fg,
            Theme::default().style(ThemeRole::User).fg
        );
        assert_eq!(body.style().fg, Theme::default().style(ThemeRole::Text).fg);
        assert!(symbols(&render_widget(&retry, 80)).contains("Attempt 2/3 failed"));
    }

    #[test]
    fn read_file_is_a_concise_non_expandable_transcript_record() {
        let mut app = App::new();
        app.transcript.submit(1, "inspect it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 7,
            name: "read_file".into(),
            summary: "Reading src/main.rs".into(),
            artifacts: Vec::new(),
        });
        app.transcript.apply(TranscriptEvent::ToolFinished {
            turn_id: 1,
            call_id: 7,
            summary: None,
            artifacts: vec![ToolArtifact::CodeRange(CodeRangeArtifact {
                path: "src/main.rs".into(),
                start_line: 1,
                end_line: 20,
                preview: Some("secret file contents".into()),
            })],
        });

        let rendered = render_transcript(&app, &TranscriptRenderCache::default());
        let cache = TranscriptRenderCache::default();
        let tool_entry = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry");
        let renderer = super::EntryRenderer::new(tool_entry, &app, &cache, 80, 24);

        assert!(rendered.contains("Read File: src/main.rs"));
        assert!(!rendered.contains("secret file contents"));
        assert!(!renderer.clickable(80));
    }

    #[test]
    fn read_file_stays_path_only_for_adversarial_mixed_artifacts() {
        let mut app = App::new();
        app.transcript.submit(1, "inspect it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 7,
            name: "read_file".into(),
            summary: "Reading src/secret.rs".into(),
            artifacts: Vec::new(),
        });
        app.transcript.apply(TranscriptEvent::ToolFinished {
            turn_id: 1,
            call_id: 7,
            summary: None,
            artifacts: vec![
                ToolArtifact::Terminal(TerminalArtifact {
                    description: "malformed terminal detail".into(),
                    command: "cat src/secret.rs".into(),
                    output: "terminal leaked contents".into(),
                    exit_code: Some(0),
                }),
                ToolArtifact::TextDetail(TextDetailArtifact {
                    text: "text leaked contents".into(),
                }),
                ToolArtifact::Patch(PatchArtifact {
                    path: "src/secret.rs".into(),
                    diff: "+patch leaked contents".into(),
                }),
                ToolArtifact::SearchResults(SearchResultsArtifact {
                    query: "secret".into(),
                    matches: "search leaked contents".into(),
                }),
                ToolArtifact::CodeRange(CodeRangeArtifact {
                    path: "src/secret.rs".into(),
                    start_line: 1,
                    end_line: 20,
                    preview: Some("preview leaked contents".into()),
                }),
                ToolArtifact::FileReference(FileReferenceArtifact {
                    path: "src/secret.rs".into(),
                }),
            ],
        });

        let cache = TranscriptRenderCache::default();
        let rendered = render_transcript(&app, &cache);
        let tool_entry = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry");
        let renderer = super::EntryRenderer::new(tool_entry, &app, &cache, 80, 24);

        assert!(rendered.contains("Read File: src/secret.rs"));
        assert!(!rendered.contains("leaked contents"));
        assert!(!rendered.contains("cat src/secret.rs"));
        assert!(!renderer.clickable(80));
        assert!(renderer.output_scroll_maximum(80).is_none());
    }

    #[test]
    fn tool_widgets_preserve_failure_interruption_and_late_event_behavior() {
        let mut failed_read = App::new();
        failed_read
            .transcript
            .submit(1, "read it".into(), Vec::new());
        failed_read
            .transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        failed_read.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 1,
            name: "read_file".into(),
            summary: "Reading missing.rs".into(),
            artifacts: Vec::new(),
        });
        failed_read.transcript.apply(TranscriptEvent::ToolFailed {
            turn_id: 1,
            call_id: 1,
            message: "not found".into(),
        });

        let failed = render_transcript(&failed_read, &TranscriptRenderCache::default());
        assert!(failed.contains("Read File: missing.rs"));
        assert!(failed.contains("failed: not found"));

        let mut interrupted_terminal = App::new();
        interrupted_terminal
            .transcript
            .submit(2, "run it".into(), Vec::new());
        interrupted_terminal
            .transcript
            .apply(TranscriptEvent::Started { turn_id: 2 });
        interrupted_terminal
            .transcript
            .apply(TranscriptEvent::ToolStarted {
                turn_id: 2,
                call_id: 2,
                name: "terminal".into(),
                summary: "Run command".into(),
                artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                    description: "Run command".into(),
                    command: "long-command".into(),
                    output: "before interrupt".into(),
                    exit_code: None,
                })],
            });
        interrupted_terminal
            .transcript
            .apply(TranscriptEvent::Interrupted { turn_id: 2 });
        interrupted_terminal
            .transcript
            .apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 2,
                call_id: 2,
                chunk: "late output".into(),
            });

        let interrupted =
            render_transcript(&interrupted_terminal, &TranscriptRenderCache::default());
        assert!(interrupted.contains("interrupted"));
        assert!(interrupted.contains("before interrupt"));
        assert!(!interrupted.contains("late output"));
    }

    #[test]
    fn compact_terminal_widget_has_a_ten_row_tail_viewport() {
        let output = (1..=30)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tool = ToolCall {
            call_id: 3,
            name: "terminal".into(),
            summary: "Running logs".into(),
            status: ActivityStatus::Running,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run logs".into(),
                command: "generate-logs".into(),
                output,
                exit_code: None,
            })],
        };
        let renderer = ToolRenderer {
            tool: &tool,
            expanded: false,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };

        let rendered = render_widget(&renderer, 80);
        let text = symbols(&rendered);

        assert_eq!(rendered.area.height, 14);
        assert!(renderer.clickable(80));
        assert!(text.contains("$ generate-logs"));
        assert!(text.contains("output 30"));
        assert!(!text.contains("output 20"));
    }

    #[test]
    fn short_output_keeps_fixed_height_without_offering_expansion() {
        let tool = ToolCall {
            call_id: 5,
            name: "terminal".into(),
            summary: "Run check".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run check".into(),
                command: "true".into(),
                output: "ok".into(),
                exit_code: Some(0),
            })],
        };
        let renderer = ToolRenderer {
            tool: &tool,
            expanded: false,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        let rendered = render_widget(&renderer, 80);
        let text = symbols(&rendered);

        assert_eq!(rendered.area.height, 14);
        assert!(!renderer.clickable(80));
        assert!(!text.contains("click to expand"));
        assert!(text.contains("ok"));
        assert!(text.contains("exit 0"));

        let empty_tool = ToolCall {
            call_id: 51,
            name: "terminal".into(),
            summary: "Run empty command".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run empty command".into(),
                command: "true".into(),
                output: String::new(),
                exit_code: Some(0),
            })],
        };
        let empty = ToolRenderer {
            tool: &empty_tool,
            expanded: false,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        assert_eq!(render_widget(&empty, 80).area.height, 14);
        assert!(!empty.clickable(80));
    }

    #[test]
    fn patch_and_search_widgets_share_the_ten_row_output_contract() {
        let lines = (1..=20)
            .map(|line| format!("result {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tools = [
            ToolCall {
                call_id: 6,
                name: "edit_file".into(),
                summary: "Edited value.txt".into(),
                status: ActivityStatus::Completed,
                artifacts: vec![ToolArtifact::Patch(PatchArtifact {
                    path: "value.txt".into(),
                    diff: lines.clone(),
                })],
            },
            ToolCall {
                call_id: 7,
                name: "search_files".into(),
                summary: "Found matches".into(),
                status: ActivityStatus::Completed,
                artifacts: vec![ToolArtifact::SearchResults(SearchResultsArtifact {
                    query: "result".into(),
                    matches: lines,
                })],
            },
        ];

        for tool in &tools {
            let renderer = ToolRenderer {
                tool,
                expanded: false,
                available_height: 24,
                scroll_from_bottom: 0,
                body_layout: None,
                cache: None,
            };
            let rendered = render_widget(&renderer, 80);
            let text = symbols(&rendered);

            assert_eq!(rendered.area.height, 13);
            assert!(text.contains("result 20"));
            assert!(!text.contains("result 10"));
        }
    }

    #[test]
    fn many_artifact_headers_are_bounded_without_consuming_the_output_body() {
        let tool = ToolCall {
            call_id: 71,
            name: "terminal".into(),
            summary: "Run many commands".into(),
            status: ActivityStatus::Completed,
            artifacts: (1..=8)
                .map(|command| {
                    ToolArtifact::Terminal(TerminalArtifact {
                        description: format!("Command {command}"),
                        command: format!("command-{command}"),
                        output: format!("output {command}"),
                        exit_code: Some(0),
                    })
                })
                .collect(),
        };
        let renderer = ToolRenderer {
            tool: &tool,
            expanded: false,
            available_height: 16,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };

        let layout = renderer.output_layout(80).expect("output layout");
        let text = symbols(&render_widget(&renderer, 80));

        assert_eq!(layout.viewport_height, super::COMPACT_OUTPUT_ROWS);
        assert!(layout.artifact_header_height <= 4);
        assert!(text.contains("output 8"));
        assert!(text.contains('…'));
    }

    #[test]
    fn multiple_output_artifacts_share_one_viewport() {
        let tool = ToolCall {
            call_id: 8,
            name: "inspect".into(),
            summary: "Inspect outputs".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![
                ToolArtifact::TextDetail(TextDetailArtifact {
                    text: "first output".into(),
                }),
                ToolArtifact::SearchResults(SearchResultsArtifact {
                    query: "second".into(),
                    matches: "second output".into(),
                }),
            ],
        };
        let renderer = ToolRenderer {
            tool: &tool,
            expanded: true,
            available_height: 24,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        let text = symbols(&render_widget(&renderer, 80));

        assert!(text.contains("first output"));
        assert!(text.contains("Search /second/"));
        assert!(text.contains("second output"));
    }

    #[test]
    fn expanded_output_uses_available_height_and_manual_scroll_reveals_history() {
        let output = (1..=30)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tool = ToolCall {
            call_id: 4,
            name: "terminal".into(),
            summary: "Running logs".into(),
            status: ActivityStatus::Running,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run logs".into(),
                command: "generate-logs".into(),
                output,
                exit_code: None,
            })],
        };
        let expanded = ToolRenderer {
            tool: &tool,
            expanded: true,
            available_height: 20,
            scroll_from_bottom: 0,
            body_layout: None,
            cache: None,
        };
        let paused = ToolRenderer {
            tool: &tool,
            expanded: false,
            available_height: 20,
            scroll_from_bottom: 5,
            body_layout: None,
            cache: None,
        };

        let expanded = render_widget(&expanded, 80);
        let paused = symbols(&render_widget(&paused, 80));

        assert_eq!(expanded.area.height, 20);
        assert!(paused.contains("output 16"));
        assert!(paused.contains("output 25"));
        assert!(!paused.contains("output 26"));
        assert!(paused.contains("paused · End to follow"));
    }

    #[test]
    fn paused_outer_transcript_resizes_expanded_widget_below_follow_banner() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (1..=80)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 93,
            name: "terminal".into(),
            summary: "Run output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run output".into(),
                command: "generate-output".into(),
                output,
                exit_code: None,
            })],
        });
        let tool_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry")
            .id;
        app.activate_transcript_entry(tool_id);

        let (_, following) = render_transcript_at(&app, &cache, &Theme::default(), 80, 20);
        app.update_transcript_scroll_maximum(following.scroll_maximum);
        assert!(app.scroll_transcript_up());
        let (paused, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 20);
        let index = cache.index.borrow();
        let measured = index
            .entries
            .iter()
            .find(|entry| entry.id == tool_id)
            .expect("indexed tool entry");

        assert!(paused.contains("End to follow"));
        assert_eq!(index.key.expect("index key").available_height, 19);
        assert_eq!(measured.end.saturating_sub(measured.start), 19);
    }

    #[test]
    fn collapse_restores_the_paused_anchor_after_expansion_exposes_the_tail() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (1..=40)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 10,
            name: "terminal".into(),
            summary: "Run output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run output".into(),
                command: "generate-output".into(),
                output,
                exit_code: None,
            })],
        });
        let tool_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry")
            .id;

        let (_, compact) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&compact.output_scroll_metrics);
        app.scroll_tool_output_by(tool_id, 1);
        assert_eq!(app.tool_output_scroll_offset(tool_id, 30), 5);

        app.activate_transcript_entry(tool_id);
        let (_, expanded) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&expanded.output_scroll_metrics);
        assert_eq!(app.tool_output_scroll_offset(tool_id, 20), 0);

        app.activate_transcript_entry(tool_id);
        let (_, collapsed) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&collapsed.output_scroll_metrics);
        assert_eq!(app.tool_output_scroll_offset(tool_id, 30), 5);
    }

    #[test]
    fn output_scroll_and_available_height_invalidate_the_relevant_render_cache_keys() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (1..=40)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 9,
            name: "terminal".into(),
            summary: "Run output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run output".into(),
                command: "generate-output".into(),
                output,
                exit_code: None,
            })],
        });
        let tool_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry")
            .id;

        let (tail, result) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&result.output_scroll_metrics);
        let before_scroll = cache.stats();
        app.scroll_tool_output_by(tool_id, 1);
        let (history, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let after_scroll = cache.stats();

        assert!(tail.contains("output 40"));
        assert!(!history.contains("output 40"));
        assert_eq!(after_scroll.0, before_scroll.0);
        assert!(after_scroll.1 > before_scroll.1);

        app.activate_transcript_entry(tool_id);
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 30);
        assert!(cache.stats().0 > after_scroll.0);

        let before_theme = cache.stats();
        let _ = render_transcript_at(&app, &cache, &Theme::resolve(ThemeId::Paper), 80, 30);
        assert!(cache.stats().1 > before_theme.1);
    }

    #[test]
    fn cached_large_output_redraw_and_scroll_only_touch_viewport_rows() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (1..=20_000)
            .map(|line| format!("large output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 91,
            name: "terminal".into(),
            summary: "Run large output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run large output".into(),
                command: "generate-large-output".into(),
                output,
                exit_code: None,
            })],
        });
        let tool_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry")
            .id;

        let (_, first) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&first.output_scroll_metrics);
        let indexed = cache.tool_output_stats();
        assert_eq!(indexed.0, 1);
        assert!(indexed.1 >= 20_000);

        app.scroll_tool_output_by(tool_id, 1);
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let scrolled = cache.tool_output_stats();
        assert_eq!(scrolled.0, indexed.0);
        assert_eq!(scrolled.1, indexed.1);
        assert!(scrolled.2.saturating_sub(indexed.2) <= super::COMPACT_OUTPUT_ROWS);

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        assert_eq!(cache.tool_output_stats(), scrolled);
    }

    #[test]
    fn paused_output_reflows_to_the_same_logical_source_line() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (1..=40)
            .map(|line| format!("source line {line}: {}", "content ".repeat(18)))
            .collect::<Vec<_>>()
            .join("\n");
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 92,
            name: "terminal".into(),
            summary: "Run wrapped output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run wrapped output".into(),
                command: "generate-wrapped-output".into(),
                output,
                exit_code: None,
            })],
        });
        let tool_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Tool(_)))
            .expect("tool entry")
            .id;

        let (_, wide) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&wide.output_scroll_metrics);
        app.scroll_tool_output_by(tool_id, 3);
        let anchor = app
            .tool_output_scroll_anchor(tool_id)
            .expect("paused logical-line anchor");

        let (_, narrow) = render_transcript_at(&app, &cache, &Theme::default(), 36, 24);
        let metrics = narrow
            .output_scroll_metrics
            .iter()
            .find(|metrics| metrics.entry_id == tool_id)
            .expect("narrow output metrics");
        let from_bottom = app.tool_output_scroll_offset_for_layout(
            tool_id,
            metrics.maximum,
            Some(&metrics.logical_lines),
        );
        let top = metrics.maximum.saturating_sub(from_bottom);

        assert_eq!(metrics.logical_lines[top], anchor);
    }

    #[test]
    fn running_reasoning_bypasses_rendered_slice_cache_between_animation_frames() {
        let mut app = App::new();
        app.transcript.submit(1, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::ReasoningDelta {
            request_id: 1,
            summary: "checking".into(),
        });
        let cache = TranscriptRenderCache::default();

        app.animation_frame = 0;
        let first = render_transcript(&app, &cache);
        let first_stats = cache.stats();
        app.animation_frame = 2;
        let second = render_transcript(&app, &cache);
        let second_stats = cache.stats();

        assert!(first.contains("Thinking|"));
        assert!(second.contains("Thinking/"));
        assert_eq!(second_stats.0, first_stats.0);
        assert_eq!(second_stats.1, first_stats.1 + 1);
    }

    #[test]
    fn assistant_markdown_renders_without_mutating_source_and_reuses_semantic_layout() {
        let source = "# Result\n\nUse **care** and `cargo test`.";
        let mut app = App::new();
        app.transcript.submit(21, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 21 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 21,
            text: source.into(),
        });
        let cache = TranscriptRenderCache::default();

        let terminal = render_transcript(&app, &cache);
        assert!(terminal.contains("Result"));
        assert!(terminal.contains("Use care and cargo test."));
        assert!(!terminal.contains("# Result"));
        assert!(!terminal.contains("**care**"));
        assert_eq!(cache.markdown_layout_builds(), 1);

        let _ = render_transcript_with_theme(&app, &cache, &Theme::resolve(ThemeId::Paper));
        assert_eq!(cache.markdown_layout_builds(), 1);

        let assistant = app
            .transcript
            .entries()
            .iter()
            .find_map(|entry| match &entry.kind {
                crate::transcript::EntryKind::Assistant(message) => Some(message),
                _ => None,
            })
            .expect("assistant response");
        assert_eq!(assistant.text, source);

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 21,
            text: " More.".into(),
        });
        let _ = render_transcript(&app, &cache);
        assert_eq!(cache.markdown_layout_builds(), 2);
    }

    #[test]
    fn concrete_renderers_preserve_diff_and_exit_status_styles() {
        let theme = Theme::default();
        let patch = render_artifact(
            &ToolArtifact::Patch(PatchArtifact {
                path: "value.txt".into(),
                diff: "-old\n+new".into(),
            }),
            40,
        );
        let terminal = render_artifact(
            &ToolArtifact::Terminal(TerminalArtifact {
                description: "Run".into(),
                command: "false".into(),
                output: String::new(),
                exit_code: Some(1),
            }),
            40,
        );

        let removed = patch
            .content()
            .iter()
            .find(|cell| cell.symbol() == "-")
            .expect("removed line");
        let failed = terminal
            .content()
            .iter()
            .find(|cell| cell.symbol() == "1")
            .expect("exit code");
        assert_eq!(removed.style().fg, theme.style(ThemeRole::DiffRemoved).fg);
        assert_eq!(failed.style().fg, theme.style(ThemeRole::Warning).fg);
    }
}
