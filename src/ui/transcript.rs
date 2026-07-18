use crate::{
    app::App,
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

pub(super) struct RenderResult {
    pub entries: Vec<EntryRegion>,
    pub scroll_maximum: usize,
}

const RENDERED_SLICE_CACHE_CAPACITY: usize = 32;
const RENDERED_SLICE_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeightKey {
    revision: u64,
    width: usize,
    expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceKey {
    revision: u64,
    width: usize,
    expanded: bool,
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

#[derive(Debug, Default)]
struct RenderCacheInner {
    heights: HashMap<EntryId, CachedHeight>,
    slices: VecDeque<CachedSlice>,
    slice_bytes: usize,
    height_builds: usize,
    slice_builds: usize,
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

    fn clickable(&self) -> bool {
        false
    }
}

struct EntryRenderer<'a> {
    entry: &'a Entry,
    expanded: bool,
    animation_frame: usize,
}

impl<'a> EntryRenderer<'a> {
    fn new(entry: &'a Entry, app: &App) -> Self {
        Self {
            entry,
            expanded: app.transcript_entry_is_expanded(entry),
            animation_frame: app.animation_frame,
        }
    }

    fn dispatch<T>(&self, dispatch: impl FnOnce(&dyn Render) -> T) -> T {
        match &self.entry.kind {
            EntryKind::User(message) => dispatch(message),
            EntryKind::Assistant(message) => dispatch(message),
            EntryKind::Reasoning(reasoning) => dispatch(&ReasoningRenderer {
                reasoning,
                expanded: self.expanded,
                animation_frame: self.animation_frame,
            }),
            EntryKind::Tool(tool) => dispatch(&ToolRenderer {
                tool,
                expanded: self.expanded,
            }),
            EntryKind::Retry(retry) => dispatch(retry),
        }
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

    fn clickable(&self) -> bool {
        self.dispatch(|renderer| renderer.clickable())
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
    let viewport_height = viewport_area.height as usize;
    let maximum_top = next_line.saturating_sub(viewport_height);
    let from_bottom = app.transcript_scroll_offset(maximum_top);
    let top = maximum_top.saturating_sub(from_bottom);
    let viewport_end = top.saturating_add(viewport_height);
    let first_visible = index.entries.partition_point(|entry| entry.end <= top);
    let mut regions = Vec::new();

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
        let renderer = EntryRenderer::new(entry, app);
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
            expanded: renderer.expanded,
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

        if renderer.clickable() {
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
        let height = measured_entry_height(entry, app, width, cache);
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
    width: usize,
    cache: &TranscriptRenderCache,
) -> usize {
    let renderer = EntryRenderer::new(entry, app);
    let key = HeightKey {
        revision: entry.revision(),
        width,
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

    fn clickable(&self) -> bool {
        true
    }
}

impl Render for AssistantMessage {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let header = LinesRenderer::new(assistant_header(&theme));
        let footer = LinesRenderer::new(assistant_footer(&theme));
        let body = match &self.status {
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
                MessageRenderer::new(&self.text, ThemeRole::Text).height(width)
            }
            AssistantStatus::Interrupted => MessageRenderer::new(&self.text, ThemeRole::Text)
                .height(width)
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
        cursor = match &self.status {
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
            AssistantStatus::Streaming | AssistantStatus::Completed => render_child(
                &MessageRenderer::new(&self.text, ThemeRole::Text),
                cursor,
                &mut context,
            ),
            AssistantStatus::Interrupted => {
                let cursor = render_child(
                    &MessageRenderer::new(&self.text, ThemeRole::Text),
                    cursor,
                    &mut context,
                );
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

    fn clickable(&self) -> bool {
        true
    }
}

struct ToolRenderer<'a> {
    tool: &'a ToolCall,
    expanded: bool,
}

impl Render for ToolRenderer<'_> {
    fn height(&self, width: usize) -> usize {
        let theme = Theme::default();
        let header = LinesRenderer::new(tool_header_lines(self.tool, self.expanded, &theme));
        let footer = LinesRenderer::new(vec![Line::styled("└", theme.style(ThemeRole::Accent))]);
        let mut height = header.height(width);
        if self.expanded {
            height = height.saturating_add(
                MessageRenderer::new(&self.tool.summary, ThemeRole::MutedText).height(width),
            );
            for artifact in &self.tool.artifacts {
                height = height.saturating_add(artifact.height(width));
            }
        } else {
            height = height.saturating_add(
                LinesRenderer::new(vec![Line::styled(
                    format!(
                        "│ {}",
                        crate::composer::safe_single_line(&self.tool.summary, 2)
                    ),
                    theme.style(ThemeRole::MutedText),
                )])
                .height(width),
            );
        }
        height.saturating_add(footer.height(width))
    }

    fn render(&self, mut context: RenderContext<'_>) {
        let header = LinesRenderer::new(tool_header_lines(self.tool, self.expanded, context.theme));
        let mut cursor = render_child(&header, 0, &mut context);
        if self.expanded {
            cursor = render_child(
                &MessageRenderer::new(&self.tool.summary, ThemeRole::MutedText),
                cursor,
                &mut context,
            );
            for artifact in &self.tool.artifacts {
                cursor = render_child(artifact, cursor, &mut context);
            }
        } else {
            let summary = LinesRenderer::new(vec![Line::styled(
                format!(
                    "│ {}",
                    crate::composer::safe_single_line(&self.tool.summary, 2)
                ),
                context.theme.style(ThemeRole::MutedText),
            )]);
            cursor = render_child(&summary, cursor, &mut context);
        }
        let footer = LinesRenderer::new(vec![Line::styled(
            "└",
            context.theme.style(ThemeRole::Accent),
        )]);
        render_child(&footer, cursor, &mut context);
    }

    fn clickable(&self) -> bool {
        true
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
            Self::Patch(artifact)=> artifact,
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

fn tool_header_lines(tool: &ToolCall, expanded: bool, theme: &Theme) -> Vec<Line<'static>> {
    let title = if tool.name == "terminal" {
        "┌─ terminal".to_owned()
    } else {
        format!("┌─ tool · {}", tool.name)
    };
    vec![Line::from(vec![
        Span::styled(title, theme.style(ThemeRole::Accent)),
        Span::styled(
            format!(
                " · {} · click to {}",
                status_label(&tool.status),
                if expanded { "collapse" } else { "expand" }
            ),
            theme.style(ThemeRole::MutedText),
        ),
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
    .chain(artifact.diff.lines().map(|line| {
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
    }))
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
        ReasoningRenderer, Render, RenderContext, ToolRenderer, TranscriptRenderCache, render,
    };
    use crate::{
        agent::AgentEvent,
        app::App,
        composer::SubmittedContent,
        theme::{Theme, ThemeRole},
        transcript::{
            ActivityStatus, AssistantMessage, AssistantStatus, CodeRangeArtifact,
            FileReferenceArtifact, PatchArtifact, Reasoning, RetryAttempt, SearchResultsArtifact,
            TerminalArtifact, TextDetailArtifact, ToolArtifact, ToolCall, UserMessage,
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
        let theme = Theme::default();
        let area = Rect::new(0, 0, 80, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render(frame, area, app, &theme, cache);
            })
            .unwrap();
        terminal.backend().to_string()
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
            assert!(symbols(&render_widget(&assistant, 80)).contains(expected));
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
            name: "read_file".into(),
            summary: "Reading file".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::TextDetail(TextDetailArtifact {
                text: "file contents".into(),
            })],
        };
        let collapsed = ToolRenderer {
            tool: &tool,
            expanded: false,
        };
        let expanded = ToolRenderer {
            tool: &tool,
            expanded: true,
        };
        assert!(!symbols(&render_widget(&collapsed, 80)).contains("file contents"));
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
