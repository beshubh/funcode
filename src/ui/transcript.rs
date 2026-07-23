use super::markdown::{MarkdownAnchor, MarkdownLayout};
use crate::{
    app::{
        App, ToolOutputLayoutId, ToolOutputRowAnchor, ToolOutputRowIndex, ToolOutputScrollMetrics,
    },
    composer::DisplayRunKind,
    theme::{Theme, ThemeId, ThemeRole},
    transcript::{
        ActivityStatus, AssistantMessage, AssistantStatus, CodeRangeArtifact, Entry, EntryId,
        EntryKind, FileReferenceArtifact, PatchArtifact, RetryAttempt, SearchResultsArtifact,
        TerminalArtifact, TextDetailArtifact, ToolArtifact, ToolCall, UserMessage,
    },
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
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
const MARKDOWN_STREAM_REBUILD_MIN_GROWTH: usize = 4 * 1024;
const MARKDOWN_FOREGROUND_REPROJECT_BYTES: usize = 4 * 1024;
const TOOL_OUTPUT_LAYOUT_CACHE_CAPACITY: usize = 32;
const TOOL_OUTPUT_LAYOUT_CACHE_BYTES: usize = 8 * 1024 * 1024;
const TOOL_OUTPUT_SPARSE_SOURCE_BYTES: usize = 512 * 1024;
// Keep dense admission conservative: each row owns styled text and an anchor in
// addition to the row allocation itself. Sparse storage is preferable before
// those allocations approach the shared cache budget.
const TOOL_OUTPUT_DENSE_ROW_ESTIMATED_BYTES: usize = 512;
const TOOL_ADJACENT_GAP_ROWS: usize = 1;
const TOOL_OUTPUT_VERTICAL_PADDING_ROWS: usize = 1;
const TOOL_OUTPUT_HORIZONTAL_PADDING_COLUMNS: usize = 1;
const ASSISTANT_MESSAGE_VERTICAL_PADDING_ROWS: usize = 1;
const ASSISTANT_MESSAGE_HORIZONTAL_PADDING_COLUMNS: usize = 1;

fn horizontally_padded_content_width(width: usize, padding: usize) -> usize {
    let padding = padding.min(width.saturating_sub(1) / 2);
    width.saturating_sub(padding.saturating_mul(2)).max(1)
}

fn horizontal_padding_for_width(width: u16, padding: usize) -> u16 {
    (padding.min(width.saturating_sub(1) as usize / 2)) as u16
}

fn horizontally_inset_area(area: Rect, padding: usize) -> Rect {
    let padding = horizontal_padding_for_width(area.width, padding);
    Rect::new(
        area.x.saturating_add(padding),
        area.y,
        area.width.saturating_sub(padding.saturating_mul(2)),
        area.height,
    )
}

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
    output_epoch: u64,
    layout: Arc<ToolOutputBodyLayout>,
    bytes: usize,
}

#[derive(Debug)]
struct ToolOutputRow {
    line: Line<'static>,
    role: ThemeRole,
    anchor: ToolOutputRowAnchor,
}

#[derive(Debug)]
struct ToolOutputArtifactLayout {
    rows: ToolOutputArtifactRows,
}

#[derive(Debug)]
enum ToolOutputArtifactRows {
    Dense(Vec<ToolOutputRow>),
    Sparse {
        encoded: Arc<String>,
        row_count: usize,
        default_role: ThemeRole,
        line_roles: Vec<ThemeRole>,
    },
}

impl ToolOutputArtifactLayout {
    fn height(&self) -> usize {
        match &self.rows {
            ToolOutputArtifactRows::Dense(rows) => rows.len(),
            ToolOutputArtifactRows::Sparse { row_count, .. } => *row_count,
        }
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(match &self.rows {
            ToolOutputArtifactRows::Dense(rows) => rows
                .capacity()
                .saturating_mul(std::mem::size_of::<ToolOutputRow>())
                .saturating_add(rows.iter().fold(0usize, |bytes, row| {
                    bytes
                        .saturating_add(
                            row.line
                                .spans
                                .capacity()
                                .saturating_mul(std::mem::size_of::<Span<'static>>()),
                        )
                        .saturating_add(row.line.spans.iter().fold(0usize, |bytes, span| {
                            bytes.saturating_add(match &span.content {
                                Cow::Borrowed(_) => 0,
                                Cow::Owned(content) => content.capacity(),
                            })
                        }))
                })),
            ToolOutputArtifactRows::Sparse {
                encoded,
                line_roles,
                ..
            } => encoded.capacity().saturating_add(
                line_roles
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ThemeRole>()),
            ),
        })
    }
}

#[derive(Debug, Default)]
struct ToolOutputBodyLayout {
    artifacts: Vec<ToolOutputArtifactLayout>,
    row_index: Arc<ToolOutputRowIndex>,
    terminal: Option<TerminalOutputIndex>,
}

#[derive(Debug, Clone, Copy)]
struct TerminalOutputIndex {
    segment: usize,
    artifact_index: usize,
    source_len: usize,
    logical_lines: usize,
    tail_logical_column: usize,
    tail_rendered_bytes: usize,
}

impl ToolOutputBodyLayout {
    fn height(&self) -> usize {
        self.artifacts.iter().fold(0usize, |height, artifact| {
            height.saturating_add(artifact.height())
        })
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.artifacts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ToolOutputArtifactLayout>()),
            )
            .saturating_add(self.row_index.allocated_bytes())
            .saturating_add(self.artifacts.iter().fold(0usize, |bytes, artifact| {
                bytes.saturating_add(artifact.bytes())
            }))
    }

    fn row_index(&self) -> Arc<ToolOutputRowIndex> {
        Arc::clone(&self.row_index)
    }

    fn append_terminal_output(&mut self, tool: &ToolCall, width: usize) -> Option<usize> {
        let terminal = *self.terminal.as_ref()?;
        let ToolArtifact::Terminal(artifact) = tool.artifacts.get(terminal.artifact_index)? else {
            return None;
        };
        let previous_len = terminal.source_len;
        if artifact.output.len() < previous_len || !artifact.output.is_char_boundary(previous_len) {
            return None;
        }
        if artifact.output.len() == previous_len {
            return Some(0);
        }
        let suffix = &artifact.output[previous_len..];
        if self
            .artifacts
            .iter()
            .any(|artifact| matches!(artifact.rows, ToolOutputArtifactRows::Dense(_)))
            && self.dense_append_should_promote(tool, suffix, width)
        {
            let replacement = build_sparse_tool_output_layout(tool, width);
            let indexed_rows = replacement.height();
            *self = replacement;
            return Some(indexed_rows);
        }

        let segment = terminal.segment;
        let appended = match &mut self.artifacts.get_mut(segment)?.rows {
            ToolOutputArtifactRows::Dense(rows) => {
                if previous_len == 0 {
                    rows.clear();
                    let mut logical_line = 0usize;
                    for line in
                        output_message_line_iter(&artifact.output, ratatui::style::Style::default())
                    {
                        push_indexed_output_line(
                            rows,
                            line,
                            ThemeRole::Text,
                            segment,
                            logical_line,
                            width,
                        );
                        logical_line = logical_line.saturating_add(1);
                    }
                    let mut state = terminal_tail_state(&artifact.output);
                    state.indexed_rows = rows.len();
                    self.row_index
                        .replace_segment(segment, rows.iter().map(|row| row.anchor));
                    state
                } else {
                    let previous_rows = rows.len();
                    let changes_tail = suffix
                        .split('\n')
                        .next()
                        .is_some_and(|part| !part.is_empty());
                    let appended = append_terminal_suffix(rows, suffix, width, &terminal);
                    let first_changed_row = if changes_tail {
                        previous_rows.saturating_sub(1)
                    } else {
                        previous_rows
                    };
                    self.row_index.refresh_dense_segment_tail(
                        segment,
                        first_changed_row,
                        rows.iter().skip(first_changed_row).map(|row| row.anchor),
                    );
                    appended
                }
            }
            ToolOutputArtifactRows::Sparse {
                encoded, row_count, ..
            } => {
                if previous_len == 0 {
                    let (replacement, replacement_rows, _, _) = build_sparse_artifact_rows(
                        tool.artifacts.get(terminal.artifact_index)?,
                        width,
                    );
                    *encoded = Arc::new(replacement);
                    *row_count = replacement_rows;
                    self.row_index
                        .replace_sparse_segment(segment, encoded, *row_count);
                    let mut state = terminal_tail_state(&artifact.output);
                    state.indexed_rows = *row_count;
                    state
                } else {
                    let old_count = *row_count;
                    let sparse = append_sparse_terminal_suffix(
                        Arc::make_mut(encoded),
                        row_count,
                        suffix,
                        width,
                        &terminal,
                    );
                    self.row_index.refresh_sparse_segment_tail(
                        segment,
                        encoded,
                        *row_count,
                        sparse.first_changed_row.min(old_count),
                    );
                    sparse.appended
                }
            }
        };
        let terminal = self.terminal.as_mut()?;
        terminal.source_len = artifact.output.len();
        terminal.logical_lines = appended.logical_lines;
        terminal.tail_logical_column = appended.tail_logical_column;
        terminal.tail_rendered_bytes = appended.tail_rendered_bytes;
        Some(appended.indexed_rows)
    }

    fn dense_append_should_promote(&self, tool: &ToolCall, suffix: &str, width: usize) -> bool {
        if tool_output_source_bytes(tool) >= TOOL_OUTPUT_SPARSE_SOURCE_BYTES {
            return true;
        }
        self.height()
            .saturating_add(estimated_message_fragment_rows(suffix, width))
            .saturating_mul(TOOL_OUTPUT_DENSE_ROW_ESTIMATED_BYTES)
            >= TOOL_OUTPUT_LAYOUT_CACHE_BYTES
    }

    fn render(&self, context: RenderContext<'_>) {
        let mut global_start = 0usize;
        for (segment, artifact) in self.artifacts.iter().enumerate() {
            let global_end = global_start.saturating_add(artifact.height());
            if global_end <= context.visible_rows.start {
                global_start = global_end;
                continue;
            }
            if global_start >= context.visible_rows.end {
                break;
            }
            let local_start = context.visible_rows.start.saturating_sub(global_start);
            let local_end = context
                .visible_rows
                .end
                .saturating_sub(global_start)
                .min(artifact.height());
            match &artifact.rows {
                ToolOutputArtifactRows::Dense(rows) => {
                    for (local_row, row) in rows[local_start..local_end].iter().enumerate() {
                        let global_row = global_start
                            .saturating_add(local_start)
                            .saturating_add(local_row);
                        let destination_row = global_row.saturating_sub(context.visible_rows.start);
                        context.buffer.set_line(
                            context.area.x,
                            context.area.y.saturating_add(destination_row as u16),
                            &row.line.clone().style(context.theme.style(row.role)),
                            context.area.width,
                        );
                    }
                }
                ToolOutputArtifactRows::Sparse {
                    encoded,
                    default_role,
                    line_roles,
                    ..
                } => {
                    let ranges = self
                        .row_index
                        .sparse_row_ranges(segment, local_start..local_end);
                    for (local_row, range) in ranges.into_iter().enumerate() {
                        let global_row = global_start
                            .saturating_add(local_start)
                            .saturating_add(local_row);
                        let destination_row = global_row.saturating_sub(context.visible_rows.start);
                        let role = self
                            .row_index
                            .anchor_at(global_row)
                            .and_then(|anchor| line_roles.get(anchor.logical_line).copied())
                            .unwrap_or(*default_role);
                        context.buffer.set_line(
                            context.area.x,
                            context.area.y.saturating_add(destination_row as u16),
                            &Line::styled(encoded[range].to_owned(), context.theme.style(role)),
                            context.area.width,
                        );
                    }
                }
            }
            global_start = global_end;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalAppendResult {
    indexed_rows: usize,
    logical_lines: usize,
    tail_logical_column: usize,
    tail_rendered_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct SparseTerminalAppendResult {
    appended: TerminalAppendResult,
    first_changed_row: usize,
}

fn terminal_tail_state(output: &str) -> TerminalAppendResult {
    let logical_lines = output
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1);
    let tail = output.rsplit('\n').next().unwrap_or_default();
    let initial_column = if logical_lines == 1 { 1 } else { 0 };
    let safe = crate::composer::safe_single_line(tail, initial_column);
    TerminalAppendResult {
        indexed_rows: 0,
        logical_lines,
        tail_logical_column: initial_column.saturating_add(UnicodeWidthStr::width(safe.as_str())),
        tail_rendered_bytes: " ".len().saturating_add(safe.len()),
    }
}

fn append_terminal_suffix(
    rows: &mut Vec<ToolOutputRow>,
    suffix: &str,
    width: usize,
    terminal: &TerminalOutputIndex,
) -> TerminalAppendResult {
    let mut logical_lines = terminal.logical_lines;
    let mut logical_column = terminal.tail_logical_column;
    let mut rendered_bytes = terminal.tail_rendered_bytes;
    let mut indexed_rows = 0usize;

    for (part_index, part) in suffix.split('\n').enumerate() {
        if part_index > 0 {
            logical_lines = logical_lines.saturating_add(1);
            logical_column = 0;
            rendered_bytes = " ".len();
            let line = Line::from(format!(
                " {}",
                crate::composer::safe_single_line(part, logical_column)
            ));
            let before = rows.len();
            push_indexed_output_line(
                rows,
                line,
                ThemeRole::Text,
                terminal.segment,
                logical_lines.saturating_sub(1),
                width,
            );
            indexed_rows = indexed_rows.saturating_add(rows.len().saturating_sub(before));
        } else if !part.is_empty() {
            let safe = crate::composer::safe_single_line(part, logical_column);
            let Some(previous) = rows.pop() else {
                return terminal_tail_state(suffix);
            };
            let anchor = previous.anchor;
            let line = Line::from(format!("{}{}", previous.line, safe));
            let before = rows.len();
            push_indexed_output_line_from(rows, line, ThemeRole::Text, anchor, width);
            indexed_rows = indexed_rows.saturating_add(rows.len().saturating_sub(before));
        }

        let safe = crate::composer::safe_single_line(part, logical_column);
        logical_column = logical_column.saturating_add(UnicodeWidthStr::width(safe.as_str()));
        rendered_bytes = rendered_bytes.saturating_add(safe.len());
    }

    TerminalAppendResult {
        indexed_rows,
        logical_lines,
        tail_logical_column: logical_column,
        tail_rendered_bytes: rendered_bytes,
    }
}

fn append_sparse_terminal_suffix(
    encoded: &mut String,
    row_count: &mut usize,
    suffix: &str,
    width: usize,
    terminal: &TerminalOutputIndex,
) -> SparseTerminalAppendResult {
    let original_rows = *row_count;
    let mut first_changed_row = original_rows;
    let mut logical_lines = terminal.logical_lines;
    let mut logical_column = terminal.tail_logical_column;
    let mut rendered_bytes = terminal.tail_rendered_bytes;
    let mut indexed_rows = 0usize;

    for (part_index, part) in suffix.split('\n').enumerate() {
        let initial_column = if part_index == 0 { logical_column } else { 0 };
        let safe = crate::composer::safe_single_line(part, initial_column);
        if part_index == 0 {
            if !part.is_empty() {
                let row_start = encoded
                    .rfind(['\n', '\r'])
                    .map_or(0, |delimiter| delimiter.saturating_add(1));
                let mut replacement = encoded[row_start..].to_owned();
                replacement.push_str(&safe);
                encoded.truncate(row_start);
                let replacement_rows = append_sparse_wrapped_line(encoded, &replacement, width);
                *row_count = row_count.saturating_sub(1).saturating_add(replacement_rows);
                indexed_rows = indexed_rows.saturating_add(replacement_rows);
                first_changed_row = original_rows.saturating_sub(1);
            }
        } else {
            logical_lines = logical_lines.saturating_add(1);
            logical_column = 0;
            rendered_bytes = " ".len();
            encoded.push('\r');
            let decorated = format!(" {safe}");
            let appended_rows = append_sparse_wrapped_line(encoded, &decorated, width);
            *row_count = row_count.saturating_add(appended_rows);
            indexed_rows = indexed_rows.saturating_add(appended_rows);
        }
        logical_column = logical_column.saturating_add(UnicodeWidthStr::width(safe.as_str()));
        rendered_bytes = rendered_bytes.saturating_add(safe.len());
    }

    SparseTerminalAppendResult {
        appended: TerminalAppendResult {
            indexed_rows,
            logical_lines,
            tail_logical_column: logical_column,
            tail_rendered_bytes: rendered_bytes,
        },
        first_changed_row,
    }
}

fn enforce_tool_output_cache_bounds(inner: &mut RenderCacheInner) {
    while inner.tool_output_layouts.len() > TOOL_OUTPUT_LAYOUT_CACHE_CAPACITY
        || inner.tool_output_layout_bytes > TOOL_OUTPUT_LAYOUT_CACHE_BYTES
    {
        let Some(evicted) = inner.tool_output_layouts.pop_front() else {
            break;
        };
        inner.tool_output_layout_bytes =
            inner.tool_output_layout_bytes.saturating_sub(evicted.bytes);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MarkdownLayoutKey {
    revision: u64,
    width: usize,
}

#[derive(Debug)]
struct MarkdownLayoutRequest {
    entry_id: EntryId,
    key: MarkdownLayoutKey,
    source: MarkdownSourceUpdate,
    content_width: usize,
}

#[derive(Debug)]
enum MarkdownSourceUpdate {
    Replace(String),
    Append { from_len: usize, suffix: String },
    Reuse { source_len: usize },
}

impl MarkdownSourceUpdate {
    fn merge(&mut self, newer: Self) {
        match newer {
            Self::Replace(source) => *self = Self::Replace(source),
            Self::Append { from_len, suffix } => match self {
                Self::Replace(source) => source.push_str(&suffix),
                Self::Append {
                    from_len: existing_from,
                    suffix: existing,
                } if existing_from.saturating_add(existing.len()) == from_len => {
                    existing.push_str(&suffix);
                }
                Self::Append { .. } | Self::Reuse { .. } => {
                    *self = Self::Append { from_len, suffix };
                }
            },
            Self::Reuse { source_len } => {
                if matches!(self, Self::Reuse { .. }) {
                    *self = Self::Reuse { source_len };
                }
            }
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Replace(source) => source.len(),
            Self::Append { suffix, .. } => suffix.len(),
            Self::Reuse { .. } => 0,
        }
    }

    fn assembled_bytes(&self) -> usize {
        match self {
            Self::Replace(source) => source.len(),
            Self::Append { from_len, suffix } => from_len.saturating_add(suffix.len()),
            Self::Reuse { source_len } => *source_len,
        }
    }
}

#[derive(Debug)]
struct MarkdownLayoutResult {
    entry_id: EntryId,
    key: MarkdownLayoutKey,
    layout: Option<MarkdownLayout>,
    retry: bool,
}

struct MarkdownLayoutRunner {
    requests: Arc<(Mutex<MarkdownRunnerState>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct MarkdownWorkerSource {
    entry_id: EntryId,
    revision: u64,
    source: String,
}

#[derive(Debug)]
struct MarkdownRunnerState {
    pending: VecDeque<MarkdownLayoutRequest>,
    pending_bytes: usize,
    active: Option<(EntryId, MarkdownLayoutKey)>,
    sources: VecDeque<MarkdownWorkerSource>,
    source_bytes: usize,
    results: VecDeque<MarkdownLayoutResult>,
    result_bytes: usize,
    accept_results: bool,
    shutdown: bool,
    stopped: bool,
}

impl Default for MarkdownRunnerState {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            pending_bytes: 0,
            active: None,
            sources: VecDeque::new(),
            source_bytes: 0,
            results: VecDeque::new(),
            result_bytes: 0,
            accept_results: true,
            shutdown: false,
            stopped: false,
        }
    }
}

impl std::fmt::Debug for MarkdownLayoutRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MarkdownLayoutRunner")
            .finish_non_exhaustive()
    }
}

impl Default for MarkdownLayoutRunner {
    fn default() -> Self {
        Self::spawn_with(MarkdownLayout::new)
    }
}

impl MarkdownLayoutRunner {
    fn spawn_with(build: impl Fn(&str, usize) -> MarkdownLayout + Send + 'static) -> Self {
        let requests = Arc::new((Mutex::new(MarkdownRunnerState::default()), Condvar::new()));
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            loop {
                let (request, source) = {
                    let (state, ready) = &*worker_requests;
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    while state.pending.is_empty() && !state.shutdown {
                        let Ok(next) = ready.wait(state) else {
                            return;
                        };
                        state = next;
                    }
                    if state.shutdown && state.pending.is_empty() {
                        state.stopped = true;
                        return;
                    }
                    let Some(mut request) = state.pending.pop_front() else {
                        continue;
                    };
                    state.pending_bytes =
                        state.pending_bytes.saturating_sub(request.source.bytes());
                    state.active = Some((request.entry_id, request.key));
                    let source = take_worker_source(&mut state, &mut request);
                    (request, source)
                };
                let (layout, retry) = match source.as_ref() {
                    Some(source) => {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            build(&source.source, request.content_width)
                        })) {
                            Ok(layout) => (Some(layout), false),
                            Err(_) => (None, false),
                        }
                    }
                    None => (None, true),
                };
                let layout = layout.filter(|layout| layout.bytes() <= MARKDOWN_LAYOUT_CACHE_BYTES);
                {
                    let (state, _) = &*worker_requests;
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    state.active = None;
                    if let Some(source) = source {
                        store_worker_source(&mut state, source);
                    }
                    if state.shutdown || !state.accept_results {
                        state.stopped = true;
                        return;
                    }
                    let superseded = state.pending.iter().any(|pending| {
                        pending.entry_id == request.entry_id
                            && pending.content_width == request.content_width
                            && pending.key.revision > request.key.revision
                    });
                    if superseded {
                        continue;
                    }
                    let result = MarkdownLayoutResult {
                        entry_id: request.entry_id,
                        key: request.key,
                        layout,
                        retry,
                    };
                    let bytes = result.layout.as_ref().map_or(0, MarkdownLayout::bytes);
                    if let Some(index) = state.results.iter().position(|queued| {
                        queued.entry_id == result.entry_id && queued.key.width == result.key.width
                    }) && let Some(previous) = state.results.remove(index)
                    {
                        state.result_bytes = state.result_bytes.saturating_sub(
                            previous.layout.as_ref().map_or(0, MarkdownLayout::bytes),
                        );
                    }
                    state.result_bytes = state.result_bytes.saturating_add(bytes);
                    state.results.push_back(result);
                    while state.results.len() > MARKDOWN_LAYOUT_CACHE_CAPACITY
                        || state.result_bytes > MARKDOWN_LAYOUT_CACHE_BYTES
                    {
                        let Some(evicted) = state.results.pop_front() else {
                            break;
                        };
                        state.result_bytes = state.result_bytes.saturating_sub(
                            evicted.layout.as_ref().map_or(0, MarkdownLayout::bytes),
                        );
                    }
                }
            }
        });
        Self {
            requests,
            worker: Some(worker),
        }
    }

    fn request(
        &self,
        request: MarkdownLayoutRequest,
    ) -> Result<Vec<(EntryId, MarkdownLayoutKey)>, ()> {
        let (state, ready) = &*self.requests;
        let mut state = state.lock().map_err(|_| ())?;
        if state.shutdown || state.stopped {
            return Err(());
        }
        if request.source.bytes() > MARKDOWN_LAYOUT_CACHE_BYTES
            || request.source.assembled_bytes() > MARKDOWN_LAYOUT_CACHE_BYTES
        {
            return Err(());
        }
        let mut superseded = Vec::new();
        if let Some((entry_id, key)) = state.active
            && entry_id == request.entry_id
            && key.width == request.key.width
            && key != request.key
        {
            superseded.push((entry_id, key));
        }
        if let Some(pending) = state.pending.iter_mut().find(|pending| {
            pending.entry_id == request.entry_id && pending.content_width == request.content_width
        }) {
            let previous_bytes = pending.source.bytes();
            superseded.push((pending.entry_id, pending.key));
            pending.key = request.key;
            pending.source.merge(request.source);
            let next_bytes = pending.source.bytes();
            state.pending_bytes = state
                .pending_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(next_bytes);
            ready.notify_one();
        } else {
            state.pending_bytes = state.pending_bytes.saturating_add(request.source.bytes());
            state.pending.push_back(request);
        }
        while state.pending.len() > MARKDOWN_LAYOUT_CACHE_CAPACITY
            || state.pending_bytes > MARKDOWN_LAYOUT_CACHE_BYTES
        {
            let Some(evicted) = state.pending.pop_front() else {
                break;
            };
            state.pending_bytes = state.pending_bytes.saturating_sub(evicted.source.bytes());
            superseded.push((evicted.entry_id, evicted.key));
        }
        ready.notify_one();
        Ok(superseded)
    }

    fn try_result(&self) -> Option<MarkdownLayoutResult> {
        let mut state = self.requests.0.lock().ok()?;
        let result = state.results.pop_front()?;
        state.result_bytes = state
            .result_bytes
            .saturating_sub(result.layout.as_ref().map_or(0, MarkdownLayout::bytes));
        Some(result)
    }

    fn outstanding(&self) -> HashSet<(EntryId, MarkdownLayoutKey)> {
        let Ok(state) = self.requests.0.lock() else {
            return HashSet::new();
        };
        state
            .pending
            .iter()
            .map(|request| (request.entry_id, request.key))
            .chain(state.active)
            .chain(
                state
                    .results
                    .iter()
                    .map(|result| (result.entry_id, result.key)),
            )
            .collect()
    }

    fn stop(&mut self, join: bool) {
        let (state, ready) = &*self.requests;
        if let Ok(mut state) = state.lock() {
            state.shutdown = true;
            state.pending_bytes = 0;
            state.pending.clear();
            ready.notify_all();
        }
        if join && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        } else {
            self.worker.take();
        }
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, usize, usize, usize, bool) {
        let state = self.requests.0.lock().expect("Markdown runner state");
        (
            state.pending.len(),
            state.pending_bytes,
            state.sources.len(),
            state.source_bytes,
            state.stopped,
        )
    }

    #[cfg(test)]
    fn disconnect_results(&mut self) {
        if let Ok(mut state) = self.requests.0.lock() {
            state.accept_results = false;
            state.results.clear();
            state.result_bytes = 0;
        }
    }

    #[cfg(test)]
    fn result_stats(&self) -> (usize, usize) {
        let state = self.requests.0.lock().expect("Markdown runner state");
        (state.results.len(), state.result_bytes)
    }
}

impl Drop for MarkdownLayoutRunner {
    fn drop(&mut self) {
        self.stop(true);
    }
}

fn take_worker_source(
    state: &mut MarkdownRunnerState,
    request: &mut MarkdownLayoutRequest,
) -> Option<MarkdownWorkerSource> {
    let existing = state
        .sources
        .iter()
        .position(|source| source.entry_id == request.entry_id)
        .and_then(|index| state.sources.remove(index));
    if let Some(existing) = &existing {
        state.source_bytes = state.source_bytes.saturating_sub(existing.source.len());
    }
    let update = std::mem::replace(
        &mut request.source,
        MarkdownSourceUpdate::Reuse { source_len: 0 },
    );
    let mut source = match (existing, update) {
        (Some(existing), _) if existing.revision > request.key.revision => {
            store_worker_source(state, existing);
            return None;
        }
        (_, MarkdownSourceUpdate::Replace(source)) => MarkdownWorkerSource {
            entry_id: request.entry_id,
            revision: request.key.revision,
            source,
        },
        (Some(existing), _) if existing.revision == request.key.revision => existing,
        (Some(mut existing), MarkdownSourceUpdate::Append { from_len, suffix })
            if existing.revision < request.key.revision && existing.source.len() == from_len =>
        {
            existing.source.push_str(&suffix);
            existing.revision = request.key.revision;
            existing
        }
        (Some(mut existing), MarkdownSourceUpdate::Reuse { source_len })
            if existing.revision < request.key.revision && existing.source.len() == source_len =>
        {
            existing.revision = request.key.revision;
            existing
        }
        (Some(existing), _) => {
            store_worker_source(state, existing);
            return None;
        }
        (None, _) => return None,
    };
    source.revision = request.key.revision;
    Some(source)
}

fn store_worker_source(state: &mut MarkdownRunnerState, source: MarkdownWorkerSource) {
    if let Some(index) = state
        .sources
        .iter()
        .position(|cached| cached.entry_id == source.entry_id)
        && let Some(previous) = state.sources.remove(index)
    {
        state.source_bytes = state.source_bytes.saturating_sub(previous.source.len());
    }
    state.source_bytes = state.source_bytes.saturating_add(source.source.len());
    state.sources.push_back(source);
    while (state.sources.len() > MARKDOWN_LAYOUT_CACHE_CAPACITY
        || state.source_bytes > MARKDOWN_LAYOUT_CACHE_BYTES)
        && state.sources.len() > 1
    {
        let Some(evicted) = state.sources.pop_front() else {
            break;
        };
        state.source_bytes = state.source_bytes.saturating_sub(evicted.source.len());
    }
    if state.source_bytes > MARKDOWN_LAYOUT_CACHE_BYTES
        && state.sources.len() == 1
        && let Some(evicted) = state.sources.pop_front()
    {
        state.source_bytes = state.source_bytes.saturating_sub(evicted.source.len());
    }
}

#[derive(Debug)]
struct CachedMarkdownLayout {
    entry_id: EntryId,
    key: MarkdownLayoutKey,
    layout: Arc<MarkdownLayout>,
    bytes: usize,
}

#[derive(Debug)]
struct LiteralMarkdownFallback {
    entry_id: EntryId,
    width: usize,
    revision: u64,
    source_len: usize,
    layout: Arc<MarkdownLayout>,
    bytes: usize,
    pending_syntax: bool,
}

#[derive(Debug, Clone, Copy)]
struct MarkdownDispatch {
    revision: u64,
    source_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownCacheToken {
    Semantic(EntryId, MarkdownLayoutKey),
    Fallback(EntryId, usize),
    OversizedFallback(EntryId, usize),
}

#[derive(Debug, Default)]
struct RenderCacheInner {
    heights: HashMap<EntryId, CachedHeight>,
    slices: VecDeque<CachedSlice>,
    slice_bytes: usize,
    markdown_layouts: VecDeque<CachedMarkdownLayout>,
    markdown_layout_bytes: usize,
    markdown_pending: HashSet<(EntryId, MarkdownLayoutKey)>,
    markdown_force_replace: HashSet<EntryId>,
    markdown_dispatches: HashMap<(EntryId, usize), MarkdownDispatch>,
    markdown_skipped: HashSet<(EntryId, MarkdownLayoutKey)>,
    markdown_fallbacks: VecDeque<LiteralMarkdownFallback>,
    markdown_oversized_fallback: Option<LiteralMarkdownFallback>,
    markdown_fallback_bytes: usize,
    markdown_recency: VecDeque<MarkdownCacheToken>,
    tool_output_layouts: VecDeque<CachedToolOutputLayout>,
    tool_output_layout_bytes: usize,
    height_builds: usize,
    slice_builds: usize,
    markdown_layout_builds: usize,
    markdown_requests: usize,
    markdown_request_bytes: usize,
    visible_rows_copied: usize,
    tool_output_layout_builds: usize,
    tool_output_rows_indexed: usize,
    tool_output_rows_rendered: usize,
}

impl RenderCacheInner {
    fn touch_markdown(&mut self, token: MarkdownCacheToken) {
        if let Some(index) = self
            .markdown_recency
            .iter()
            .position(|cached| *cached == token)
        {
            self.markdown_recency.remove(index);
        }
        self.markdown_recency.push_back(token);
    }

    fn enforce_markdown_bounds(&mut self) {
        while self.markdown_layouts.len() + self.markdown_fallbacks.len()
            > MARKDOWN_LAYOUT_CACHE_CAPACITY
            || self
                .markdown_layout_bytes
                .saturating_add(self.markdown_fallback_bytes)
                > MARKDOWN_LAYOUT_CACHE_BYTES
        {
            let Some(token) = self.markdown_recency.pop_front() else {
                break;
            };
            match token {
                MarkdownCacheToken::Semantic(entry_id, key) => {
                    if let Some(index) = self
                        .markdown_layouts
                        .iter()
                        .position(|cached| cached.entry_id == entry_id && cached.key == key)
                        && let Some(evicted) = self.markdown_layouts.remove(index)
                    {
                        self.markdown_layout_bytes =
                            self.markdown_layout_bytes.saturating_sub(evicted.bytes);
                    }
                }
                MarkdownCacheToken::Fallback(entry_id, width) => {
                    if let Some(index) = self
                        .markdown_fallbacks
                        .iter()
                        .position(|cached| cached.entry_id == entry_id && cached.width == width)
                        && let Some(evicted) = self.markdown_fallbacks.remove(index)
                    {
                        self.markdown_fallback_bytes =
                            self.markdown_fallback_bytes.saturating_sub(evicted.bytes);
                    }
                }
                MarkdownCacheToken::OversizedFallback(entry_id, width) => {
                    if self
                        .markdown_oversized_fallback
                        .as_ref()
                        .is_some_and(|cached| cached.entry_id == entry_id && cached.width == width)
                        && let Some(evicted) = self.markdown_oversized_fallback.take()
                    {
                        self.markdown_fallback_bytes =
                            self.markdown_fallback_bytes.saturating_sub(evicted.bytes);
                    }
                }
            }
        }
    }

    fn skip_markdown(&mut self, entry_id: EntryId, key: MarkdownLayoutKey) {
        self.markdown_skipped.retain(|(cached_id, cached_key)| {
            *cached_id != entry_id || cached_key.width != key.width || *cached_key == key
        });
        if self.markdown_skipped.len() >= MARKDOWN_LAYOUT_CACHE_CAPACITY
            && let Some(evicted) = self.markdown_skipped.iter().next().copied()
        {
            self.markdown_skipped.remove(&evicted);
        }
        self.markdown_skipped.insert((entry_id, key));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexKey {
    width: usize,
    available_height: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReflowAnchor {
    entry_id: EntryId,
    local_row: usize,
    markdown_anchor: Option<MarkdownAnchor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollAnchor {
    key: IndexKey,
    reference_top: usize,
    anchored_top: usize,
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
    scroll_anchor: RefCell<Option<ScrollAnchor>>,
    pending_reflow_anchor: RefCell<Option<ReflowAnchor>>,
    markdown_runner: MarkdownLayoutRunner,
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
        let output_epoch = entry.tool_output_epoch();
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
        let incremental = inner
            .tool_output_layouts
            .iter()
            .rposition(|cached| {
                cached.entry_id == entry.id
                    && cached.key.width == width
                    && cached.output_epoch == output_epoch
            })
            .and_then(|index| inner.tool_output_layouts.remove(index));
        if let Some(cached) = &incremental {
            inner.tool_output_layout_bytes =
                inner.tool_output_layout_bytes.saturating_sub(cached.bytes);
        }
        drop(inner);

        let content_width =
            horizontally_padded_content_width(width, TOOL_OUTPUT_HORIZONTAL_PADDING_COLUMNS);
        if let Some(cached) = incremental
            && let Ok(mut layout) = Arc::try_unwrap(cached.layout)
            && let Some(indexed_rows) = layout.append_terminal_output(tool, content_width)
        {
            let bytes = layout.bytes();
            let layout = Arc::new(layout);
            let mut inner = self.inner.borrow_mut();
            inner.tool_output_rows_indexed =
                inner.tool_output_rows_indexed.saturating_add(indexed_rows);
            inner.tool_output_layout_bytes = inner.tool_output_layout_bytes.saturating_add(bytes);
            inner.tool_output_layouts.push_back(CachedToolOutputLayout {
                entry_id: entry.id,
                key,
                output_epoch,
                layout: Arc::clone(&layout),
                bytes,
            });
            enforce_tool_output_cache_bounds(&mut inner);
            return Some(layout);
        }

        let layout = Arc::new(build_tool_output_layout(tool, content_width));
        let bytes = layout.bytes();
        let mut inner = self.inner.borrow_mut();
        let mut retained = VecDeque::with_capacity(inner.tool_output_layouts.len());
        while let Some(cached) = inner.tool_output_layouts.pop_front() {
            if cached.entry_id == entry.id && cached.key.width == width {
                inner.tool_output_layout_bytes =
                    inner.tool_output_layout_bytes.saturating_sub(cached.bytes);
            } else {
                retained.push_back(cached);
            }
        }
        inner.tool_output_layouts = retained;
        inner.tool_output_layout_builds = inner.tool_output_layout_builds.saturating_add(1);
        inner.tool_output_rows_indexed = inner
            .tool_output_rows_indexed
            .saturating_add(layout.height());
        inner.tool_output_layout_bytes = inner.tool_output_layout_bytes.saturating_add(bytes);
        inner.tool_output_layouts.push_back(CachedToolOutputLayout {
            entry_id: entry.id,
            key,
            output_epoch,
            layout: Arc::clone(&layout),
            bytes,
        });
        enforce_tool_output_cache_bounds(&mut inner);
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
    fn tool_output_cache_stats(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (
            inner.tool_output_layouts.len(),
            inner.tool_output_layout_bytes,
        )
    }

    #[cfg(test)]
    fn tool_output_sparse_layouts(&self) -> usize {
        self.inner
            .borrow()
            .tool_output_layouts
            .iter()
            .filter(|cached| {
                cached
                    .layout
                    .artifacts
                    .iter()
                    .any(|artifact| matches!(artifact.rows, ToolOutputArtifactRows::Sparse { .. }))
            })
            .count()
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

    fn prune_markdown_pending(&self) {
        let outstanding = self.markdown_runner.outstanding();
        let mut inner = self.inner.borrow_mut();
        let expired = inner
            .markdown_pending
            .iter()
            .filter(|pending| !outstanding.contains(pending))
            .copied()
            .collect::<Vec<_>>();
        for (entry_id, key) in expired {
            inner.markdown_pending.remove(&(entry_id, key));
            if inner
                .markdown_dispatches
                .get(&(entry_id, key.width))
                .is_some_and(|dispatch| dispatch.revision == key.revision)
            {
                inner.markdown_dispatches.remove(&(entry_id, key.width));
            }
        }
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
                inner.touch_markdown(MarkdownCacheToken::Semantic(entry.id, key));
                return Some(layout);
            }
        }

        let content_width =
            horizontally_padded_content_width(width, ASSISTANT_MESSAGE_HORIZONTAL_PADDING_COLUMNS);
        self.prune_markdown_pending();
        let (fallback_layout, source_update) = {
            let mut inner = self.inner.borrow_mut();
            let force_replace = inner.markdown_force_replace.remove(&entry.id);
            inner.markdown_skipped.retain(|(cached_id, cached_key)| {
                *cached_id != entry.id || cached_key.width != width || *cached_key == key
            });
            let skipped = inner.markdown_skipped.contains(&(entry.id, key));
            let previous_dispatch = inner.markdown_dispatches.get(&(entry.id, width)).copied();
            let final_revision = !matches!(message.status, AssistantStatus::Streaming);
            let growth_allows_dispatch = previous_dispatch.is_none_or(|previous| {
                previous.revision != key.revision
                    && (final_revision
                        || message.text.len()
                            >= previous.source_len.saturating_add(
                                previous.source_len.max(MARKDOWN_STREAM_REBUILD_MIN_GROWTH),
                            ))
            });
            let should_dispatch = !skipped && (force_replace || growth_allows_dispatch);
            let existing = inner
                .markdown_fallbacks
                .iter()
                .position(|fallback| fallback.entry_id == entry.id && fallback.width == width)
                .and_then(|index| inner.markdown_fallbacks.remove(index))
                .or_else(|| {
                    inner
                        .markdown_oversized_fallback
                        .as_ref()
                        .filter(|fallback| fallback.entry_id == entry.id && fallback.width == width)
                        .map(|_| ())?;
                    inner.markdown_oversized_fallback.take()
                });
            let mut fallback = if let Some(fallback) = existing {
                inner.markdown_fallback_bytes =
                    inner.markdown_fallback_bytes.saturating_sub(fallback.bytes);
                fallback
            } else {
                let layout = Arc::new(markdown_foreground_layout(&message.text, content_width));
                LiteralMarkdownFallback {
                    entry_id: entry.id,
                    width,
                    revision: entry.revision(),
                    source_len: message.text.len(),
                    bytes: layout.bytes(),
                    layout,
                    pending_syntax: foreground_has_incomplete_syntax(&message.text),
                }
            };
            if fallback.revision != entry.revision()
                && message.text.len() >= fallback.source_len
                && message.text.is_char_boundary(fallback.source_len)
            {
                let suffix = message.text[fallback.source_len..].to_owned();
                if !fallback.layout.is_literal_projection()
                    || (fallback.pending_syntax && foreground_suffix_may_close_syntax(&suffix))
                {
                    fallback.layout =
                        Arc::new(markdown_foreground_layout(&message.text, content_width));
                } else if suffix.len() <= MARKDOWN_FOREGROUND_REPROJECT_BYTES
                    || foreground_suffix_requires_rebuild(&suffix)
                {
                    let projected = super::markdown::foreground_suffix(&suffix);
                    Arc::make_mut(&mut fallback.layout).append_literal(&projected, content_width);
                } else {
                    Arc::make_mut(&mut fallback.layout).append_literal(&suffix, content_width);
                }
                fallback.source_len = message.text.len();
                fallback.revision = entry.revision();
                fallback.pending_syntax = foreground_has_incomplete_syntax(&message.text);
            } else if fallback.revision != entry.revision() {
                fallback.source_len = message.text.len();
                fallback.layout =
                    Arc::new(markdown_foreground_layout(&message.text, content_width));
                fallback.revision = entry.revision();
                fallback.pending_syntax = foreground_has_incomplete_syntax(&message.text);
            }
            fallback.bytes = fallback.layout.bytes();
            let layout = Arc::clone(&fallback.layout);
            if fallback.bytes <= MARKDOWN_LAYOUT_CACHE_BYTES {
                inner.markdown_fallback_bytes =
                    inner.markdown_fallback_bytes.saturating_add(fallback.bytes);
                inner.markdown_fallbacks.push_back(fallback);
                inner.touch_markdown(MarkdownCacheToken::Fallback(entry.id, width));
                inner.enforce_markdown_bounds();
            } else {
                if let Some(previous) = inner.markdown_oversized_fallback.take() {
                    inner.markdown_fallback_bytes =
                        inner.markdown_fallback_bytes.saturating_sub(previous.bytes);
                }
                inner.markdown_oversized_fallback = Some(fallback);
                let oversized_bytes = inner
                    .markdown_oversized_fallback
                    .as_ref()
                    .map_or(0, |item| item.bytes);
                if oversized_bytes <= MARKDOWN_LAYOUT_CACHE_BYTES {
                    inner.markdown_fallback_bytes = inner
                        .markdown_fallback_bytes
                        .saturating_add(oversized_bytes);
                    inner.touch_markdown(MarkdownCacheToken::OversizedFallback(entry.id, width));
                    inner.enforce_markdown_bounds();
                } else {
                    inner.markdown_oversized_fallback = None;
                }
            }
            let source_update = if should_dispatch {
                let update = if force_replace {
                    MarkdownSourceUpdate::Replace(message.text.clone())
                } else if let Some(previous) = previous_dispatch {
                    if message.text.len() < previous.source_len
                        || !message.text.is_char_boundary(previous.source_len)
                    {
                        MarkdownSourceUpdate::Replace(message.text.clone())
                    } else {
                        MarkdownSourceUpdate::Append {
                            from_len: previous.source_len,
                            suffix: message.text[previous.source_len..].to_owned(),
                        }
                    }
                } else {
                    MarkdownSourceUpdate::Replace(message.text.clone())
                };
                if update.bytes() > MARKDOWN_LAYOUT_CACHE_BYTES
                    || update.assembled_bytes() > MARKDOWN_LAYOUT_CACHE_BYTES
                {
                    inner.skip_markdown(entry.id, key);
                    None
                } else {
                    inner.markdown_pending.insert((entry.id, key));
                    inner.markdown_dispatches.insert(
                        (entry.id, width),
                        MarkdownDispatch {
                            revision: key.revision,
                            source_len: message.text.len(),
                        },
                    );
                    Some(update)
                }
            } else {
                None
            };
            (layout, source_update)
        };
        if let Some(source_update) = source_update {
            {
                let mut inner = self.inner.borrow_mut();
                inner.markdown_requests = inner.markdown_requests.saturating_add(1);
                inner.markdown_request_bytes = inner
                    .markdown_request_bytes
                    .saturating_add(source_update.bytes());
            }
            match self.markdown_runner.request(MarkdownLayoutRequest {
                entry_id: entry.id,
                key,
                source: source_update,
                content_width,
            }) {
                Ok(superseded) => {
                    let mut inner = self.inner.borrow_mut();
                    for superseded in superseded {
                        inner.markdown_pending.remove(&superseded);
                        if superseded == (entry.id, key) {
                            inner.markdown_dispatches.remove(&(entry.id, width));
                            inner.skip_markdown(entry.id, key);
                        }
                    }
                }
                Err(()) => {
                    let mut inner = self.inner.borrow_mut();
                    inner.markdown_pending.remove(&(entry.id, key));
                    inner.markdown_dispatches.remove(&(entry.id, width));
                }
            }
        }
        Some(fallback_layout)
    }

    fn store_markdown_layout(
        &self,
        entry_id: EntryId,
        key: MarkdownLayoutKey,
        layout: Arc<MarkdownLayout>,
    ) -> bool {
        let bytes = layout.bytes();
        let mut inner = self.inner.borrow_mut();
        let is_current_fallback = inner
            .markdown_fallbacks
            .iter()
            .find(|fallback| fallback.entry_id == entry_id && fallback.width == key.width)
            .is_some_and(|fallback| fallback.revision == key.revision);
        let projection_changed = inner
            .markdown_fallbacks
            .iter()
            .find(|fallback| fallback.entry_id == entry_id && fallback.width == key.width)
            .is_some_and(|fallback| !fallback.layout.visually_eq(&layout));
        inner.markdown_pending.remove(&(entry_id, key));
        inner.markdown_layout_builds = inner.markdown_layout_builds.saturating_add(1);
        if let Some(index) = inner
            .markdown_layouts
            .iter()
            .position(|cached| cached.entry_id == entry_id && cached.key == key)
            && let Some(previous) = inner.markdown_layouts.remove(index)
        {
            inner.markdown_layout_bytes =
                inner.markdown_layout_bytes.saturating_sub(previous.bytes);
        }
        inner.markdown_layout_bytes = inner.markdown_layout_bytes.saturating_add(bytes);
        inner.markdown_layouts.push_back(CachedMarkdownLayout {
            entry_id,
            key,
            layout,
            bytes,
        });
        inner.touch_markdown(MarkdownCacheToken::Semantic(entry_id, key));
        inner.enforce_markdown_bounds();
        if is_current_fallback && projection_changed {
            inner.heights.remove(&entry_id);
            let mut retained = VecDeque::with_capacity(inner.slices.len());
            while let Some(slice) = inner.slices.pop_front() {
                if slice.entry_id == entry_id {
                    inner.slice_bytes = inner.slice_bytes.saturating_sub(slice.bytes);
                } else {
                    retained.push_back(slice);
                }
            }
            inner.slices = retained;
        }
        drop(inner);
        if is_current_fallback && projection_changed {
            let mut index = self.index.borrow_mut();
            if let Some(position) = index.entries.iter().position(|entry| entry.id == entry_id) {
                index.entries.truncate(position);
            }
        }
        is_current_fallback && projection_changed
    }

    fn drain_markdown_results_inner(&self, app: Option<&App>) -> bool {
        let mut changed = false;
        self.prune_markdown_pending();
        let reflow_anchor = app.and_then(|app| self.current_reflow_anchor(app));
        while let Some(result) = self.markdown_runner.try_result() {
            if let Some(layout) = result.layout {
                if self.store_markdown_layout(result.entry_id, result.key, Arc::new(layout))
                    && self.pending_reflow_anchor.borrow().is_none()
                {
                    *self.pending_reflow_anchor.borrow_mut() = reflow_anchor.clone();
                }
                changed = true;
            } else {
                let mut inner = self.inner.borrow_mut();
                inner
                    .markdown_pending
                    .remove(&(result.entry_id, result.key));
                if result.retry {
                    inner.markdown_force_replace.insert(result.entry_id);
                    inner
                        .markdown_dispatches
                        .remove(&(result.entry_id, result.key.width));
                } else {
                    inner.skip_markdown(result.entry_id, result.key);
                }
            }
        }
        changed
    }

    pub(crate) fn drain_markdown_results_for(&self, app: &App) -> bool {
        self.drain_markdown_results_inner(Some(app))
    }

    #[cfg(test)]
    fn drain_markdown_results(&self) -> bool {
        self.drain_markdown_results_inner(None)
    }

    #[cfg(test)]
    fn markdown_layout_builds(&self) -> usize {
        self.inner.borrow().markdown_layout_builds
    }

    #[cfg(test)]
    fn markdown_request_stats(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (inner.markdown_requests, inner.markdown_request_bytes)
    }

    #[cfg(test)]
    fn markdown_cache_stats(&self) -> (usize, usize, usize, usize) {
        let inner = self.inner.borrow();
        (
            inner.markdown_layouts.len() + inner.markdown_fallbacks.len(),
            inner
                .markdown_layout_bytes
                .saturating_add(inner.markdown_fallback_bytes),
            inner.markdown_pending.len(),
            inner.markdown_fallbacks.len(),
        )
    }

    fn markdown_anchor_for_row(
        &self,
        entry_id: EntryId,
        revision: u64,
        width: usize,
        row: usize,
    ) -> Option<MarkdownAnchor> {
        let inner = self.inner.borrow();
        let layout = inner
            .markdown_layouts
            .iter()
            .find(|cached| {
                cached.entry_id == entry_id && cached.key == MarkdownLayoutKey { revision, width }
            })
            .map(|cached| Arc::clone(&cached.layout))
            .or_else(|| {
                inner
                    .markdown_fallbacks
                    .iter()
                    .find(|fallback| fallback.entry_id == entry_id && fallback.width == width)
                    .filter(|fallback| fallback.revision == revision)
                    .map(|fallback| Arc::clone(&fallback.layout))
            })
            .or_else(|| {
                inner
                    .markdown_oversized_fallback
                    .as_ref()
                    .filter(|fallback| {
                        fallback.entry_id == entry_id
                            && fallback.width == width
                            && fallback.revision == revision
                    })
                    .map(|fallback| Arc::clone(&fallback.layout))
            })?;
        Some(layout.anchor_for_row(row))
    }

    fn current_reflow_anchor(&self, app: &App) -> Option<ReflowAnchor> {
        if app.transcript_is_following() {
            return None;
        }
        let index = self.index.borrow();
        let key = index.key?;
        let next_line = index.entries.last().map_or(0, |entry| entry.end);
        let viewport_height = key.available_height.saturating_sub(1);
        let maximum = next_line.saturating_sub(viewport_height);
        let top = maximum.saturating_sub(app.transcript_scroll_offset(maximum));
        let indexed = index
            .entries
            .iter()
            .find(|entry| entry.start <= top && top < entry.end)?;
        let local_row = top.saturating_sub(indexed.start);
        let source = app
            .transcript
            .entries()
            .iter()
            .find(|source| source.id == indexed.id)?;
        Some(ReflowAnchor {
            entry_id: indexed.id,
            local_row,
            markdown_anchor: matches!(source.kind, EntryKind::Assistant(_))
                .then(|| {
                    self.markdown_anchor_for_row(
                        indexed.id,
                        source.revision(),
                        key.width,
                        local_row,
                    )
                })
                .flatten(),
        })
    }

    fn markdown_row_for_anchor(
        &self,
        entry_id: EntryId,
        revision: u64,
        width: usize,
        anchor: &MarkdownAnchor,
    ) -> Option<usize> {
        let inner = self.inner.borrow();
        let layout = inner
            .markdown_layouts
            .iter()
            .find(|cached| {
                cached.entry_id == entry_id && cached.key == MarkdownLayoutKey { revision, width }
            })
            .map(|cached| Arc::clone(&cached.layout))
            .or_else(|| {
                inner
                    .markdown_fallbacks
                    .iter()
                    .find(|fallback| fallback.entry_id == entry_id && fallback.width == width)
                    .filter(|fallback| fallback.revision == revision)
                    .map(|fallback| Arc::clone(&fallback.layout))
            })
            .or_else(|| {
                inner
                    .markdown_oversized_fallback
                    .as_ref()
                    .filter(|fallback| {
                        fallback.entry_id == entry_id
                            && fallback.width == width
                            && fallback.revision == revision
                    })
                    .map(|fallback| Arc::clone(&fallback.layout))
            })?;
        Some(layout.row_for_anchor(anchor))
    }

    fn resolve_scroll_top(
        &self,
        key: IndexKey,
        default_top: usize,
        reflow_top: Option<usize>,
        manual: bool,
        maximum_top: usize,
    ) -> usize {
        if !manual {
            *self.scroll_anchor.borrow_mut() = None;
            return default_top;
        }
        if let Some(anchored_top) = reflow_top {
            *self.scroll_anchor.borrow_mut() = Some(ScrollAnchor {
                key,
                reference_top: default_top,
                anchored_top: anchored_top.min(maximum_top),
            });
        }
        let mut anchor = self.scroll_anchor.borrow_mut();
        let Some(state) = anchor.as_mut().filter(|state| state.key == key) else {
            *anchor = None;
            return default_top;
        };
        state.anchored_top = if default_top >= state.reference_top {
            state
                .anchored_top
                .saturating_add(default_top - state.reference_top)
        } else {
            state
                .anchored_top
                .saturating_sub(state.reference_top - default_top)
        }
        .min(maximum_top);
        state.reference_top = default_top;
        state.anchored_top
    }
}

fn markdown_foreground_layout(source: &str, width: usize) -> MarkdownLayout {
    MarkdownLayout::foreground(source, width)
}

fn foreground_suffix_requires_rebuild(suffix: &str) -> bool {
    suffix.bytes().any(|byte| {
        matches!(
            byte,
            b'#' | b'*' | b'_' | b'~' | b'`' | b'[' | b']' | b'(' | b')' | b'<' | b'>' | b'!'
        )
    }) || suffix
        .lines()
        .any(|line| line.starts_with("- ") || line.starts_with("+ "))
}

fn foreground_has_incomplete_syntax(source: &str) -> bool {
    let source = if source.len() <= 8192 {
        std::borrow::Cow::Borrowed(source)
    } else {
        let prefix_end = (0..=4096)
            .rev()
            .find(|offset| source.is_char_boundary(*offset))
            .unwrap_or(0);
        let suffix_start = (source.len().saturating_sub(4096)..=source.len())
            .find(|offset| source.is_char_boundary(*offset))
            .unwrap_or(source.len());
        std::borrow::Cow::Owned(format!(
            "{}{}",
            &source[..prefix_end],
            &source[suffix_start..]
        ))
    };
    let mut odd = false;
    for delimiter in ["```", "~~~", "**", "~~", "`", "*", "_"] {
        odd |= source.matches(delimiter).count() % 2 == 1;
    }
    odd || source.matches('[').count() > source.matches(']').count()
}

fn foreground_suffix_may_close_syntax(suffix: &str) -> bool {
    suffix
        .bytes()
        .any(|byte| matches!(byte, b'`' | b'*' | b'_' | b'~' | b']' | b')'))
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
        let output_row_index = output_body_layout.as_ref().map(|layout| layout.row_index());
        Self {
            entry,
            cache,
            expanded,
            markdown_layout: cache.markdown_layout(entry, width),
            available_height,
            output_scroll_from_bottom: output_maximum
                .map(|maximum| {
                    app.tool_output_scroll_offset_for_layout(
                        entry.id,
                        maximum,
                        ToolOutputLayoutId {
                            revision: entry.revision(),
                            width,
                        },
                        output_row_index.as_ref(),
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
            EntryKind::Reasoning(_) => dispatch(&HiddenRenderer),
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
            layout_id: ToolOutputLayoutId {
                revision: self.entry.revision(),
                width,
            },
            row_index: self.output_body_layout.as_ref()?.row_index(),
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
    cache.drain_markdown_results_for(app);
    let content_area = area;
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
    let (visibly_manual, available_height, reflow_top) = if app.transcript_is_following() {
        (
            false,
            full_available_height,
            ensure_layout_index(app, width, full_available_height, cache),
        )
    } else {
        let paused_height = full_available_height.saturating_sub(1);
        let paused_reflow_top = ensure_layout_index(app, width, paused_height, cache);
        let paused_next_line = cache
            .index
            .borrow()
            .entries
            .last()
            .map_or(0, |entry| entry.end);
        let paused_maximum = paused_next_line.saturating_sub(paused_height);
        if app.transcript_scroll_offset(paused_maximum) > 0 {
            (true, paused_height, paused_reflow_top)
        } else {
            (
                false,
                full_available_height,
                ensure_layout_index(app, width, full_available_height, cache),
            )
        }
    };
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
    let default_top = maximum_top.saturating_sub(from_bottom);
    let top = cache.resolve_scroll_top(
        IndexKey {
            width,
            available_height,
        },
        default_top,
        reflow_top,
        visibly_manual,
        maximum_top,
    );
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
    width: usize,
    available_height: usize,
    cache: &TranscriptRenderCache,
) -> Option<usize> {
    let key = IndexKey {
        width,
        available_height,
    };
    let entries = app.transcript.entries();
    let width_reflow_anchor = {
        let index = cache.index.borrow();
        index
            .key
            .filter(|previous| previous.width != width)
            .and_then(|previous| {
                let next_line = index.entries.last().map_or(0, |entry| entry.end);
                let viewport_height = previous.available_height.saturating_sub(1);
                let maximum = next_line.saturating_sub(viewport_height);
                let top = maximum.saturating_sub(app.transcript_scroll_offset(maximum));
                index
                    .entries
                    .iter()
                    .find(|entry| entry.start <= top && top < entry.end)
                    .map(|entry| ReflowAnchor {
                        entry_id: entry.id,
                        local_row: top.saturating_sub(entry.start),
                        markdown_anchor: entries
                            .iter()
                            .find(|source| source.id == entry.id)
                            .filter(|source| matches!(source.kind, EntryKind::Assistant(_)))
                            .and_then(|source| {
                                let local_row = top.saturating_sub(entry.start);
                                cache.markdown_anchor_for_row(
                                    entry.id,
                                    source.revision(),
                                    previous.width,
                                    local_row,
                                )
                            }),
                    })
            })
    };
    let reflow_anchor = cache
        .pending_reflow_anchor
        .borrow_mut()
        .take()
        .or(width_reflow_anchor);
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
        return None;
    }

    let mut updated = {
        let index = cache.index.borrow();
        index.entries[..valid_prefix.min(index.entries.len())].to_vec()
    };
    let mut next_line = updated.last().map_or(0, |entry| entry.end);
    let mut previous_visible_is_tool = updated
        .iter()
        .enumerate()
        .rev()
        .find(|(_, indexed)| indexed.start < indexed.end)
        .map(|(index, _)| matches!(entries[index].kind, EntryKind::Tool(_)));
    for entry in &entries[valid_prefix..] {
        let height = measured_entry_height(entry, app, width, available_height, cache);
        let is_tool = matches!(entry.kind, EntryKind::Tool(_));
        if height > 0 {
            if previous_visible_is_tool.is_some_and(|previous_is_tool| previous_is_tool || is_tool)
            {
                next_line = next_line.saturating_add(TOOL_ADJACENT_GAP_ROWS);
            }
            previous_visible_is_tool = Some(is_tool);
        }
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
    reflow_anchor.and_then(|anchor| {
        index
            .entries
            .iter()
            .find(|entry| entry.id == anchor.entry_id)
            .map(|entry| {
                let local_row = anchor
                    .markdown_anchor
                    .and_then(|markdown_anchor| {
                        entries
                            .iter()
                            .find(|source| source.id == entry.id)
                            .and_then(|source| {
                                cache.markdown_row_for_anchor(
                                    entry.id,
                                    source.revision(),
                                    width,
                                    &markdown_anchor,
                                )
                            })
                    })
                    .unwrap_or(anchor.local_row);
                entry.start.saturating_add(
                    local_row.min(entry.end.saturating_sub(entry.start).saturating_sub(1)),
                )
            })
    })
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
    let key = HeightKey {
        revision: entry.revision(),
        width,
        available_height,
        expanded: app.transcript_entry_is_expanded(entry),
    };
    if let Some(height) = cache.height(entry.id, key) {
        return height;
    }
    let renderer = EntryRenderer::new(entry, app, cache, width, available_height);
    let height = renderer.height(width);
    cache.store_height(entry.id, key, height);
    height
}

impl Render for UserMessage {
    fn height(&self, width: usize) -> usize {
        self.content
            .layout(width.saturating_sub(2).max(1))
            .total_rows()
            .saturating_add(2)
    }

    fn render(&self, context: RenderContext<'_>) {
        context
            .buffer
            .set_style(context.area, context.theme.transcript_surface());
        let layout = self
            .content
            .layout((context.area.width as usize).saturating_sub(2).max(1));
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
        match &self.message.status {
            AssistantStatus::Queued => LinesRenderer::new(vec![Line::styled(
                "queued…",
                theme.style(ThemeRole::Accent),
            )])
            .height(width),
            AssistantStatus::Thinking => LinesRenderer::new(vec![Line::styled(
                "thinking…",
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
                        "[interrupted]",
                        theme.style(ThemeRole::Warning),
                    )])
                    .height(width),
                ),
            AssistantStatus::Failed(message) => LinesRenderer::new(vec![Line::styled(
                format!("[failed: {message}]"),
                theme.style(ThemeRole::Warning),
            )])
            .height(width),
        }
    }

    fn render(&self, mut context: RenderContext<'_>) {
        context
            .buffer
            .set_style(context.area, context.theme.style(ThemeRole::Surface));
        match &self.message.status {
            AssistantStatus::Queued => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    "queued…",
                    context.theme.style(ThemeRole::Accent),
                )]),
                0,
                &mut context,
            ),
            AssistantStatus::Thinking => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    "thinking…",
                    context.theme.style(ThemeRole::Accent),
                )]),
                0,
                &mut context,
            ),
            AssistantStatus::Streaming | AssistantStatus::Completed => {
                self.layout.map_or(0, |layout| {
                    render_child(
                        &MarkdownMessageRenderer {
                            layout,
                            padded: true,
                        },
                        0,
                        &mut context,
                    )
                })
            }
            AssistantStatus::Interrupted => {
                let cursor = self.layout.map_or(0, |layout| {
                    render_child(
                        &MarkdownMessageRenderer {
                            layout,
                            padded: true,
                        },
                        0,
                        &mut context,
                    )
                });
                render_child(
                    &LinesRenderer::new(vec![Line::styled(
                        "[interrupted]",
                        context.theme.style(ThemeRole::Warning),
                    )]),
                    cursor,
                    &mut context,
                )
            }
            AssistantStatus::Failed(message) => render_child(
                &LinesRenderer::new(vec![Line::styled(
                    format!("[failed: {message}]"),
                    context.theme.style(ThemeRole::Warning),
                )]),
                0,
                &mut context,
            ),
        };
    }
}

struct MarkdownMessageRenderer<'a> {
    layout: &'a MarkdownLayout,
    padded: bool,
}

impl MarkdownMessageRenderer<'_> {
    fn height(layout: &MarkdownLayout) -> usize {
        layout
            .height()
            .saturating_add(ASSISTANT_MESSAGE_VERTICAL_PADDING_ROWS.saturating_mul(2))
    }
}

impl Render for MarkdownMessageRenderer<'_> {
    fn height(&self, _width: usize) -> usize {
        if self.padded {
            Self::height(self.layout)
        } else {
            self.layout.height()
        }
    }

    fn render(&self, context: RenderContext<'_>) {
        let padding = usize::from(self.padded) * ASSISTANT_MESSAGE_VERTICAL_PADDING_ROWS;
        let content_start = padding;
        let content_end = content_start.saturating_add(self.layout.height());
        let visible_start = context.visible_rows.start.max(content_start);
        let visible_end = context.visible_rows.end.min(content_end);

        let content_area = if self.padded {
            horizontally_inset_area(context.area, ASSISTANT_MESSAGE_HORIZONTAL_PADDING_COLUMNS)
        } else {
            context.area
        };
        for source_row in visible_start..visible_end {
            let Some(line) = self
                .layout
                .line(source_row.saturating_sub(content_start), context.theme)
            else {
                continue;
            };
            let destination_row = source_row.saturating_sub(context.visible_rows.start);
            context.buffer.set_line(
                content_area.x,
                content_area.y.saturating_add(destination_row as u16),
                &line,
                content_area.width,
            );
        }
    }
}

struct HiddenRenderer;

impl Render for HiddenRenderer {
    fn height(&self, _width: usize) -> usize {
        0
    }

    fn render(&self, _context: RenderContext<'_>) {}
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
        context
            .buffer
            .set_style(context.area, context.theme.transcript_surface());
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
            self.expanded,
            &layout,
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
        let content_width =
            horizontally_padded_content_width(width, TOOL_OUTPUT_HORIZONTAL_PADDING_COLUMNS);
        let raw_body_height = self.body_layout.map_or_else(
            || ToolOutputBodyRenderer { tool: self.tool }.height(content_width),
            ToolOutputBodyLayout::height,
        );
        let body_height = if raw_body_height == 0 {
            0
        } else {
            raw_body_height.saturating_add(TOOL_OUTPUT_VERTICAL_PADDING_ROWS.saturating_mul(2))
        };
        let base_header_height = LinesRenderer::new(output_tool_header_lines(
            self.tool,
            self.expanded,
            false,
            &Theme::default(),
        ))
        .height(width);
        let static_footer_height = usize::from(tool_exit_code(self.tool).is_some());
        let base_artifact_header_limit = artifact_header_limit(
            self.available_height,
            base_header_height,
            static_footer_height,
        );
        let base_artifact_header_height = LinesRenderer::new(bounded_output_artifact_header_lines(
            self.tool,
            width,
            base_artifact_header_limit,
            &Theme::default(),
        ))
        .height(width);
        let base_chrome = base_header_height
            .saturating_add(base_artifact_header_height)
            .saturating_add(static_footer_height);
        let base_compact_height =
            output_viewport_height(false, self.available_height, base_chrome, body_height);
        let can_expand = body_height > base_compact_height;
        let header_height = LinesRenderer::new(output_tool_header_lines(
            self.tool,
            self.expanded,
            can_expand,
            &Theme::default(),
        ))
        .height(width);
        let footer_height = usize::from(static_footer_height > 0 || can_expand);
        let artifact_header_limit =
            artifact_header_limit(self.available_height, header_height, footer_height);
        let artifact_header_height = LinesRenderer::new(bounded_output_artifact_header_lines(
            self.tool,
            width,
            artifact_header_limit,
            &Theme::default(),
        ))
        .height(width);
        let chrome_height = header_height
            .saturating_add(artifact_header_height)
            .saturating_add(footer_height);
        let viewport_height = output_viewport_height(
            self.expanded,
            self.available_height,
            chrome_height,
            body_height,
        );
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

fn artifact_header_limit(
    available_height: usize,
    header_height: usize,
    footer_height: usize,
) -> usize {
    let reserved_chrome = header_height.saturating_add(footer_height);
    let body_rows = COMPACT_OUTPUT_ROWS.min(available_height.saturating_sub(reserved_chrome));
    available_height
        .saturating_sub(reserved_chrome.saturating_add(body_rows))
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
                output_message_height(&artifact.matches, ThemeRole::MutedText, width)
            }
            ToolArtifact::Terminal(artifact) => {
                output_message_height(&artifact.output, ThemeRole::Text, width)
            }
            ToolArtifact::TextDetail(artifact) => {
                output_message_height(&artifact.text, ThemeRole::MutedText, width)
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
                render_output_message(&artifact.matches, ThemeRole::MutedText, context)
            }
            ToolArtifact::Terminal(artifact) => {
                render_output_message(&artifact.output, ThemeRole::Text, context)
            }
            ToolArtifact::TextDetail(artifact) => {
                render_output_message(&artifact.text, ThemeRole::MutedText, context)
            }
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => {}
        }
    }
}

fn output_message_height(text: &str, role: ThemeRole, width: usize) -> usize {
    if text.is_empty() {
        0
    } else {
        MessageRenderer::new(text, role).height(width)
    }
}

fn render_output_message(text: &str, role: ThemeRole, context: RenderContext<'_>) {
    if !text.is_empty() {
        MessageRenderer::new(text, role).render(context);
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
        let content_area =
            horizontally_inset_area(context.area, TOOL_OUTPUT_HORIZONTAL_PADDING_COLUMNS);
        let raw_body_height = self.body_layout.map_or_else(
            || self.body.height(content_area.width as usize),
            ToolOutputBodyLayout::height,
        );
        if raw_body_height == 0 {
            return;
        }

        let padding = TOOL_OUTPUT_VERTICAL_PADDING_ROWS.min(self.height / 2);
        let content_viewport_height = self.height.saturating_sub(padding.saturating_mul(2));
        let maximum = raw_body_height.saturating_sub(content_viewport_height);
        let top = maximum.saturating_sub(self.scroll_from_bottom.min(maximum));
        let visible_start = context.visible_rows.start.max(padding);
        let visible_end = context
            .visible_rows
            .end
            .min(padding.saturating_add(content_viewport_height));
        if visible_start >= visible_end {
            return;
        }

        let source_start = top.saturating_add(visible_start.saturating_sub(padding));
        let source_end = top
            .saturating_add(visible_end.saturating_sub(padding))
            .min(raw_body_height);
        if source_start >= source_end {
            return;
        }

        let offset = visible_start
            .saturating_sub(context.visible_rows.start)
            .min(u16::MAX as usize) as u16;
        let mut body_area = content_area;
        body_area.y = body_area.y.saturating_add(offset);
        body_area.height = body_area
            .height
            .saturating_sub(offset.min(body_area.height));
        let body_context = RenderContext {
            theme: context.theme,
            area: body_area,
            buffer: context.buffer,
            visible_rows: source_start..source_end,
        };
        if let Some(layout) = self.body_layout {
            layout.render(body_context);
        } else {
            self.body.render(body_context);
        }
        if let Some(cache) = self.cache {
            cache.record_tool_output_rows(source_end.saturating_sub(source_start));
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
                Some(Line::default())
            } else if row <= content_rows {
                let line = visible.next()?;
                let mut spans = vec![Span::raw(" ")];
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
                Some(Line::default())
            } else {
                None
            }
        })
        .collect()
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
    if tool_output_should_be_sparse(tool, width) {
        return build_sparse_tool_output_layout(tool, width);
    }
    build_dense_tool_output_layout(tool, width)
}

fn tool_output_should_be_sparse(tool: &ToolCall, width: usize) -> bool {
    tool_output_source_bytes(tool) >= TOOL_OUTPUT_SPARSE_SOURCE_BYTES
        || estimated_tool_output_rows(tool, width)
            .saturating_mul(TOOL_OUTPUT_DENSE_ROW_ESTIMATED_BYTES)
            >= TOOL_OUTPUT_LAYOUT_CACHE_BYTES
}

fn tool_output_source_bytes(tool: &ToolCall) -> usize {
    output_artifacts(tool).fold(0usize, |bytes, artifact| {
        bytes.saturating_add(match artifact {
            ToolArtifact::Patch(artifact) => artifact.diff.len(),
            ToolArtifact::SearchResults(artifact) => artifact.matches.len(),
            ToolArtifact::Terminal(artifact) => artifact.output.len(),
            ToolArtifact::TextDetail(artifact) => artifact.text.len(),
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => 0,
        })
    })
}

fn estimated_tool_output_rows(tool: &ToolCall, width: usize) -> usize {
    let width = width.max(1);
    output_artifacts(tool).fold(0usize, |rows, artifact| {
        let (source, logical_lines) = match artifact {
            ToolArtifact::Patch(artifact) => {
                (artifact.diff.as_str(), artifact.diff.lines().count())
            }
            ToolArtifact::SearchResults(artifact) => (
                artifact.matches.as_str(),
                message_logical_line_count(&artifact.matches),
            ),
            ToolArtifact::Terminal(artifact) => (
                artifact.output.as_str(),
                message_logical_line_count(&artifact.output),
            ),
            ToolArtifact::TextDetail(artifact) => (
                artifact.text.as_str(),
                message_logical_line_count(&artifact.text),
            ),
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => ("", 0),
        };
        let content_wraps = source.len().div_ceil(width);
        let prefix_wraps = logical_lines.saturating_mul(2).div_ceil(width);
        rows.saturating_add(logical_lines)
            .saturating_add(content_wraps)
            .saturating_add(prefix_wraps)
    })
}

fn estimated_message_fragment_rows(source: &str, width: usize) -> usize {
    let width = width.max(1);
    let logical_lines = message_logical_line_count(source);
    logical_lines
        .saturating_add(source.len().div_ceil(width))
        .saturating_add(logical_lines.saturating_mul(2).div_ceil(width))
}

fn message_logical_line_count(source: &str) -> usize {
    source
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1)
}

fn build_dense_tool_output_layout(tool: &ToolCall, width: usize) -> ToolOutputBodyLayout {
    let mut layout = ToolOutputBodyLayout::default();
    for (artifact_index, artifact) in tool.artifacts.iter().enumerate() {
        if !matches!(
            artifact,
            ToolArtifact::Patch(_)
                | ToolArtifact::SearchResults(_)
                | ToolArtifact::Terminal(_)
                | ToolArtifact::TextDetail(_)
        ) {
            continue;
        }
        let segment = layout.artifacts.len();
        let mut rows = Vec::new();
        let mut logical_line = 0usize;
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
                    let line =
                        Line::from(format!(" {}", crate::composer::safe_single_line(source, 1)));
                    push_indexed_output_line(&mut rows, line, role, segment, logical_line, width);
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::SearchResults(artifact) => {
                for line in
                    output_message_line_iter(&artifact.matches, ratatui::style::Style::default())
                {
                    push_indexed_output_line(
                        &mut rows,
                        line,
                        ThemeRole::MutedText,
                        segment,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::Terminal(artifact) => {
                for line in
                    output_message_line_iter(&artifact.output, ratatui::style::Style::default())
                {
                    push_indexed_output_line(
                        &mut rows,
                        line,
                        ThemeRole::Text,
                        segment,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
                if layout.terminal.is_none() {
                    let tail = terminal_tail_state(&artifact.output);
                    layout.terminal = Some(TerminalOutputIndex {
                        segment,
                        artifact_index,
                        source_len: artifact.output.len(),
                        logical_lines: tail.logical_lines,
                        tail_logical_column: tail.tail_logical_column,
                        tail_rendered_bytes: tail.tail_rendered_bytes,
                    });
                }
            }
            ToolArtifact::TextDetail(artifact) => {
                for line in
                    output_message_line_iter(&artifact.text, ratatui::style::Style::default())
                {
                    push_indexed_output_line(
                        &mut rows,
                        line,
                        ThemeRole::MutedText,
                        segment,
                        logical_line,
                        width,
                    );
                    logical_line = logical_line.saturating_add(1);
                }
            }
            ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => {}
        }
        layout.artifacts.push(ToolOutputArtifactLayout {
            rows: ToolOutputArtifactRows::Dense(rows),
        });
    }
    layout.row_index = Arc::new(ToolOutputRowIndex::new(
        layout
            .artifacts
            .iter()
            .map(|artifact| match &artifact.rows {
                ToolOutputArtifactRows::Dense(rows) => rows.iter().map(|row| row.anchor).collect(),
                ToolOutputArtifactRows::Sparse { .. } => Vec::new(),
            })
            .collect(),
    ));
    layout
}

fn build_sparse_tool_output_layout(tool: &ToolCall, width: usize) -> ToolOutputBodyLayout {
    let mut layout = ToolOutputBodyLayout::default();
    let mut sparse_segments = Vec::new();
    for (artifact_index, artifact) in tool.artifacts.iter().enumerate() {
        if !matches!(
            artifact,
            ToolArtifact::Patch(_)
                | ToolArtifact::SearchResults(_)
                | ToolArtifact::Terminal(_)
                | ToolArtifact::TextDetail(_)
        ) {
            continue;
        }
        let segment = layout.artifacts.len();
        let (encoded, row_count, default_role, line_roles) =
            build_sparse_artifact_rows(artifact, width);
        let encoded = Arc::new(encoded);
        sparse_segments.push((Arc::clone(&encoded), row_count));
        layout.artifacts.push(ToolOutputArtifactLayout {
            rows: ToolOutputArtifactRows::Sparse {
                encoded,
                row_count,
                default_role,
                line_roles,
            },
        });
        if layout.terminal.is_none()
            && let ToolArtifact::Terminal(artifact) = artifact
        {
            let tail = terminal_tail_state(&artifact.output);
            layout.terminal = Some(TerminalOutputIndex {
                segment,
                artifact_index,
                source_len: artifact.output.len(),
                logical_lines: tail.logical_lines,
                tail_logical_column: tail.tail_logical_column,
                tail_rendered_bytes: tail.tail_rendered_bytes,
            });
        }
    }
    layout.row_index = Arc::new(ToolOutputRowIndex::new_sparse(&sparse_segments));
    layout
}

fn build_sparse_artifact_rows(
    artifact: &ToolArtifact,
    width: usize,
) -> (String, usize, ThemeRole, Vec<ThemeRole>) {
    let mut encoded = String::new();
    let mut row_count = 0usize;
    let mut line_roles = Vec::new();
    let (source, default_role, patch) = match artifact {
        ToolArtifact::Patch(artifact) => (artifact.diff.as_str(), ThemeRole::MutedText, true),
        ToolArtifact::SearchResults(artifact) => {
            (artifact.matches.as_str(), ThemeRole::MutedText, false)
        }
        ToolArtifact::Terminal(artifact) => (artifact.output.as_str(), ThemeRole::Text, false),
        ToolArtifact::TextDetail(artifact) => (artifact.text.as_str(), ThemeRole::MutedText, false),
        ToolArtifact::CodeRange(_) | ToolArtifact::FileReference(_) => {
            return (encoded, row_count, ThemeRole::MutedText, line_roles);
        }
    };
    let lines: Box<dyn Iterator<Item = &str>> = if source.is_empty() {
        Box::new(std::iter::empty())
    } else if patch {
        Box::new(source.lines())
    } else {
        Box::new(source.split('\n'))
    };
    for (logical_line, source) in lines.enumerate() {
        if logical_line > 0 {
            encoded.push('\r');
        }
        let role = if patch {
            if source.starts_with('+') && !source.starts_with("+++") {
                ThemeRole::DiffAdded
            } else if source.starts_with('-') && !source.starts_with("---") {
                ThemeRole::DiffRemoved
            } else {
                ThemeRole::MutedText
            }
        } else {
            default_role
        };
        if patch {
            line_roles.push(role);
        }
        let decorated = format!(
            " {}",
            crate::composer::safe_single_line(
                source,
                if patch || logical_line == 0 { 1 } else { 0 }
            )
        );
        row_count =
            row_count.saturating_add(append_sparse_wrapped_line(&mut encoded, &decorated, width));
    }
    (encoded, row_count, default_role, line_roles)
}

fn append_sparse_wrapped_line(encoded: &mut String, line: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut column = 0usize;
    for grapheme in line.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if column > 0 && column.saturating_add(grapheme_width) > width {
            encoded.push('\n');
            rows = rows.saturating_add(1);
            column = 0;
        }
        encoded.push_str(grapheme);
        column = column.saturating_add(grapheme_width);
    }
    rows
}

fn push_indexed_output_line(
    rows: &mut Vec<ToolOutputRow>,
    line: Line<'static>,
    role: ThemeRole,
    artifact: usize,
    logical_line: usize,
    width: usize,
) {
    push_indexed_output_line_from(
        rows,
        line,
        role,
        ToolOutputRowAnchor {
            artifact,
            logical_line,
            source_byte: 0,
        },
        width,
    );
}

fn push_indexed_output_line_from(
    rows: &mut Vec<ToolOutputRow>,
    line: Line<'static>,
    role: ThemeRole,
    base_anchor: ToolOutputRowAnchor,
    width: usize,
) {
    rows.extend(
        wrap_indexed_line(line, width)
            .into_iter()
            .map(|(line, source_byte)| ToolOutputRow {
                line,
                role,
                anchor: ToolOutputRowAnchor {
                    source_byte: base_anchor.source_byte.saturating_add(source_byte),
                    ..base_anchor
                },
            }),
    );
}

fn output_viewport_height(
    expanded: bool,
    available_height: usize,
    chrome: usize,
    body_height: usize,
) -> usize {
    let available = available_height.saturating_sub(chrome);
    let limit = if expanded {
        available
    } else {
        COMPACT_OUTPUT_ROWS.min(available)
    };
    body_height.min(limit)
}

fn output_tool_header_lines(
    tool: &ToolCall,
    _expanded: bool,
    _can_expand: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let title = if tool.name == "terminal" {
        " terminal".to_owned()
    } else {
        format!(" tool · {}", tool.name)
    };
    vec![Line::from(vec![
        Span::styled(title, theme.style(ThemeRole::Accent)),
        Span::styled(
            format!(" · {}", status_label(&tool.status)),
            theme.style(ThemeRole::MutedText),
        ),
    ])]
}

fn output_artifact_header_lines(tool: &ToolCall, theme: &Theme) -> Vec<Line<'static>> {
    output_artifacts(tool)
        .flat_map(|artifact| match artifact {
            ToolArtifact::Patch(artifact) => vec![Line::styled(
                format!(" Edited {}", artifact.path.display()),
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
            if width >= 2 { " …" } else { "…" },
            theme.style(ThemeRole::MutedText),
        );
    }
    rows
}

fn output_footer_lines(
    tool: &ToolCall,
    expanded: bool,
    layout: &ToolOutputLayout,
    scroll_from_bottom: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let maximum = layout.body_height.saturating_sub(layout.viewport_height);
    let from_bottom = scroll_from_bottom.min(maximum);
    let top = maximum.saturating_sub(from_bottom);
    let visible_end = top
        .saturating_add(layout.viewport_height)
        .min(layout.body_height);
    let mut spans = vec![Span::raw(" ")];
    if let Some(exit_code) = tool_exit_code(tool) {
        spans.push(Span::styled(
            format!("exit {exit_code}"),
            if exit_code == 0 {
                theme.style(ThemeRole::DiffAdded)
            } else {
                theme.style(ThemeRole::Warning)
            },
        ));
    }
    if !expanded && layout.can_expand {
        let separator = if spans.len() > 1 { " · " } else { "" };
        spans.push(Span::styled(
            format!("{separator}Click to expand"),
            theme.style(ThemeRole::MutedText),
        ));
    } else if expanded && maximum == 0 && layout.can_expand {
        let separator = if spans.len() > 1 { " · " } else { "" };
        spans.push(Span::styled(
            format!("{separator}Click to collapse"),
            theme.style(ThemeRole::MutedText),
        ));
    }
    if expanded && maximum > 0 {
        let full_state = if from_bottom == 0 {
            "latest".to_owned()
        } else {
            "paused · End to follow".to_owned()
        };
        let full_indicator = format!(
            " · lines {}-{visible_end}/{} · {full_state}",
            top + 1,
            layout.body_height
        );
        let used = spans.iter().fold(0usize, |used, span| {
            used.saturating_add(UnicodeWidthStr::width(span.content.as_ref()))
        });
        let indicator =
            if used.saturating_add(UnicodeWidthStr::width(full_indicator.as_str())) <= width {
                full_indicator
            } else if from_bottom == 0 {
                format!(" · {visible_end}/{} · latest", layout.body_height)
            } else {
                " · paused · End to follow".to_owned()
            };
        spans.push(Span::styled(indicator, theme.style(ThemeRole::MutedText)));
    }
    if spans.len() == 1 {
        Vec::new()
    } else {
        vec![Line::from(spans)]
    }
}

fn tool_exit_code(tool: &ToolCall) -> Option<i32> {
    output_artifacts(tool)
        .filter_map(|artifact| match artifact {
            ToolArtifact::Terminal(TerminalArtifact { exit_code, .. }) => *exit_code,
            _ => None,
        })
        .last()
}

fn generic_tool_lines(tool: &ToolCall, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw(" "),
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
    tool.name == "read_file" || output_artifacts(tool).next().is_none()
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
        Span::raw(" "),
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
            " Read {}:{}-{}",
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
        format!(" Edited {}", artifact.path.display()),
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
            format!(" {}", crate::composer::safe_single_line(line, 1)),
            theme.style(role),
        )
    })
}

fn search_results_header(artifact: &SearchResultsArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!(
            " Search /{}/",
            crate::composer::safe_single_line(&artifact.query, 9)
        ),
        theme.style(ThemeRole::Accent),
    )]
}

fn terminal_header(artifact: &TerminalArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            format!(
                " # {}",
                crate::composer::safe_single_line(&artifact.description, 1)
            ),
            theme.style(ThemeRole::MutedText),
        ),
        Line::styled(
            format!(
                " $ {}",
                crate::composer::safe_single_line(&artifact.command, 3)
            ),
            theme.style(ThemeRole::Text),
        ),
    ]
}

fn terminal_exit_line(exit_code: i32, theme: &Theme) -> Line<'static> {
    Line::styled(
        format!(" exit {exit_code}"),
        if exit_code == 0 {
            theme.style(ThemeRole::DiffAdded)
        } else {
            theme.style(ThemeRole::Warning)
        },
    )
}

fn file_reference_lines(artifact: &FileReferenceArtifact, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!(" File {}", artifact.path.display()),
        theme.style(ThemeRole::Accent),
    )]
}

fn message_line_iter(
    text: &str,
    style: ratatui::style::Style,
) -> Box<dyn Iterator<Item = Line<'static>> + '_> {
    message_line_iter_from(text, style, 0)
}

fn output_message_line_iter(
    text: &str,
    style: ratatui::style::Style,
) -> Box<dyn Iterator<Item = Line<'static>> + '_> {
    if text.is_empty() {
        Box::new(std::iter::empty())
    } else {
        message_line_iter(text, style)
    }
}

fn message_line_iter_from(
    text: &str,
    style: ratatui::style::Style,
    starting_line: usize,
) -> Box<dyn Iterator<Item = Line<'static>> + '_> {
    if text.is_empty() {
        return Box::new(std::iter::once(Line::styled(" ", style)));
    }
    Box::new(text.split('\n').enumerate().flat_map(move |(index, text)| {
        let logical_line = starting_line.saturating_add(index);
        crate::composer::SubmittedContent::plain(text)
            .display_lines(if logical_line == 0 { 1 } else { 0 })
            .into_iter()
            .map(move |line| {
                let mut spans = vec![Span::styled(" ", style)];
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

fn wrap_indexed_line(line: Line<'static>, width: usize) -> Vec<(Line<'static>, usize)> {
    let width = width.max(1);
    let mut output = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut column = 0usize;
    let mut source_byte = 0usize;
    let mut row_source_byte = 0usize;
    for span in line.spans {
        let style = line.style.patch(span.style);
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if column > 0 && column.saturating_add(grapheme_width) > width {
                output.push((Line::from(std::mem::take(&mut spans)), row_source_byte));
                column = 0;
                row_source_byte = source_byte;
            }
            if let Some(previous) = spans.last_mut()
                && previous.style == style
            {
                previous.content.to_mut().push_str(grapheme);
            } else {
                spans.push(Span::styled(grapheme.to_owned(), style));
            }
            column = column.saturating_add(grapheme_width);
            source_byte = source_byte.saturating_add(grapheme.len());
        }
    }
    output.push((Line::from(spans), row_source_byte));
    output
}

fn status_label(status: &ActivityStatus) -> String {
    status.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        AssistantRenderer, Render, RenderContext, ToolRenderer, TranscriptRenderCache, render,
    };
    use crate::ui::markdown::MarkdownLayout;
    use crate::{
        agent::AgentEvent,
        app::App,
        composer::SubmittedContent,
        theme::{Theme, ThemeId, ThemeRole},
        transcript::{
            ActivityStatus, AssistantMessage, AssistantStatus, CodeRangeArtifact, EntryKind,
            FileReferenceArtifact, PatchArtifact, RetryAttempt, SearchResultsArtifact,
            TerminalArtifact, TextDetailArtifact, ToolArtifact, ToolCall, TranscriptEvent,
            UserMessage,
        },
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
    use std::{
        thread,
        time::{Duration, Instant},
    };

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

    fn wait_for_markdown(app: &App, cache: &TranscriptRenderCache) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let _ = render_transcript(app, cache);
            let (_, _, pending, _) = cache.markdown_cache_stats();
            if pending == 0 && cache.markdown_runner.stats().0 == 0 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "background Markdown layout timed out"
            );
            thread::sleep(Duration::from_millis(2));
        }
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
        assert!(symbols(&user).contains("prompt"));
        let padding = user.cell((0, 0)).expect("user padding");
        let body = user.cell((2, 1)).expect("user message body");
        assert_ne!(
            padding.style().bg,
            Theme::default().style(ThemeRole::Surface).bg
        );
        assert_eq!(body.style().fg, Theme::default().style(ThemeRole::Text).fg);
        assert!(symbols(&render_widget(&retry, 80)).contains("Attempt 2/3 failed"));
    }

    #[test]
    fn user_is_shaded_while_assistant_content_stays_borderless_and_unshaded() {
        let user = UserMessage {
            content: SubmittedContent::plain("prompt"),
        };
        let assistant = AssistantMessage {
            text: "response".into(),
            status: AssistantStatus::Completed,
        };
        let layout = MarkdownLayout::new(&assistant.text, 80);
        let assistant = AssistantRenderer {
            message: &assistant,
            layout: Some(&layout),
        };

        let user = render_widget(&user, 80);
        let assistant = render_widget(&assistant, 80);
        let user_symbols = symbols(&user);
        let assistant_symbols = symbols(&assistant);

        assert!(user_symbols.contains("prompt"));
        assert!(assistant_symbols.contains("response"));
        assert!(!user_symbols.contains(['┌', '└', '│']));
        assert!(!assistant_symbols.contains(['┌', '└', '│']));
        let user_background = user.cell((0, 0)).expect("user padding").style().bg;
        let assistant_background = assistant
            .cell((0, 0))
            .expect("assistant response")
            .style()
            .bg;
        assert_eq!(
            assistant_background,
            Theme::default().style(ThemeRole::Surface).bg
        );
        assert_ne!(user_background, assistant_background);
    }

    #[test]
    fn assistant_response_has_padding_on_all_four_sides() {
        let assistant = AssistantMessage {
            text: "x".repeat(79),
            status: AssistantStatus::Completed,
        };
        let layout = MarkdownLayout::new(&assistant.text, 78);
        let renderer = AssistantRenderer {
            message: &assistant,
            layout: Some(&layout),
        };

        let rendered = render_widget(&renderer, 80);

        assert_eq!(rendered.area.height, 4);
        assert_eq!(rendered.cell((0, 0)).expect("top padding").symbol(), " ");
        assert_eq!(rendered.cell((0, 1)).expect("left padding").symbol(), " ");
        assert_eq!(rendered.cell((1, 1)).expect("response body").symbol(), "x");
        assert_eq!(rendered.cell((79, 1)).expect("right padding").symbol(), " ");
        assert_eq!(rendered.cell((1, 3)).expect("bottom padding").symbol(), " ");
    }

    #[test]
    fn tool_output_has_padding_on_all_four_sides() {
        let tool = ToolCall {
            call_id: 41,
            name: "terminal".into(),
            summary: "Run output".into(),
            status: ActivityStatus::Running,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run output".into(),
                command: "emit-output".into(),
                output: "x".repeat(79),
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
        let viewport = renderer.output_viewport(80).expect("output viewport");

        let rendered = render_widget(&renderer, 80);
        let top = viewport.start as u16;
        let bottom = viewport.end.saturating_sub(1) as u16;

        assert_eq!(rendered.cell((1, top)).expect("top padding").symbol(), " ");
        assert_eq!(
            rendered
                .cell((0, top.saturating_add(1)))
                .expect("left padding")
                .symbol(),
            " "
        );
        assert_eq!(
            rendered
                .cell((2, top.saturating_add(1)))
                .expect("output body")
                .symbol(),
            "x"
        );
        assert_eq!(
            rendered
                .cell((79, top.saturating_add(1)))
                .expect("right padding")
                .symbol(),
            " "
        );
        assert_eq!(
            rendered.cell((1, bottom)).expect("bottom padding").symbol(),
            " "
        );
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
        assert!(text.contains("Click to expand"));
        assert!(!text.contains("latest"));
    }

    #[test]
    fn tool_output_is_shaded_without_border_glyphs() {
        let tool = ToolCall {
            call_id: 4,
            name: "terminal".into(),
            summary: "Run check".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run check".into(),
                command: "cargo check".into(),
                output: "Finished".into(),
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
        let card_background = rendered.cell((0, 0)).expect("tool card padding").style().bg;

        assert!(text.contains("$ cargo check"));
        assert!(text.contains("Finished"));
        assert!(!text.contains(['┌', '└', '│']));
        assert_eq!(
            rendered.cell((0, 0)).expect("one-cell gutter").symbol(),
            " "
        );
        assert_eq!(rendered.cell((1, 0)).expect("tool title").symbol(), "t");
        assert_ne!(
            card_background,
            Theme::default().style(ThemeRole::Surface).bg
        );
    }

    #[test]
    fn short_output_shrinks_to_its_content_without_offering_expansion() {
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

        assert_eq!(rendered.area.height, 7);
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
        assert_eq!(render_widget(&empty, 80).area.height, 4);
        assert!(!empty.clickable(80));
    }

    #[test]
    fn short_search_output_does_not_reserve_an_empty_footer() {
        let tool = ToolCall {
            call_id: 52,
            name: "search_files".into(),
            summary: "No matches".into(),
            status: ActivityStatus::Completed,
            artifacts: vec![ToolArtifact::SearchResults(SearchResultsArtifact {
                query: "missing".into(),
                matches: "No matches found.".into(),
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

        assert_eq!(render_widget(&renderer, 80).area.height, 5);
        assert!(!renderer.clickable(80));
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

        assert_eq!(layout.viewport_height, layout.body_height);
        assert!(layout.viewport_height <= super::COMPACT_OUTPUT_ROWS);
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
                ToolArtifact::CodeRange(CodeRangeArtifact {
                    path: "src/main.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    preview: Some("private read preview".into()),
                }),
                ToolArtifact::TextDetail(TextDetailArtifact {
                    text: "first output".into(),
                }),
                ToolArtifact::SearchResults(SearchResultsArtifact {
                    query: "second".into(),
                    matches: "second output".into(),
                }),
                ToolArtifact::FileReference(FileReferenceArtifact {
                    path: "src/other.rs".into(),
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
        assert!(!text.contains("private read preview"));
        assert!(!text.contains("Read File:"));
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
            expanded: true,
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
    fn retained_outer_scroll_anchor_at_the_tail_hides_the_follow_banner() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 95,
            name: "terminal".into(),
            summary: "Run output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Run output".into(),
                command: "generate-output".into(),
                output: (1..=40)
                    .map(|line| format!("output {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
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

        app.update_transcript_scroll_maximum(100);
        assert!(app.scroll_transcript_up());
        assert_eq!(app.transcript_scroll_offset(100), 5);

        let (rendered, result) = render_transcript_at(&app, &cache, &Theme::default(), 80, 20);
        assert_eq!(app.transcript_scroll_offset(result.scroll_maximum), 0);
        assert!(!rendered.contains("End to follow"));
        let index = cache.index.borrow();
        let measured = index
            .entries
            .iter()
            .find(|entry| entry.id == tool_id)
            .expect("indexed tool entry");
        assert_eq!(index.key.expect("index key").available_height, 20);
        assert_eq!(measured.end.saturating_sub(measured.start), 20);
    }

    #[test]
    fn collapse_returns_compact_output_to_its_latest_tail() {
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
        app.activate_transcript_entry(tool_id);
        let (_, expanded) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&expanded.output_scroll_metrics);
        app.scroll_tool_output_by(tool_id, 1);
        assert_eq!(app.tool_output_scroll_offset(tool_id, 22), 5);

        app.activate_transcript_entry(tool_id);
        let (collapsed_text, collapsed) =
            render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&collapsed.output_scroll_metrics);
        assert_eq!(app.tool_output_scroll_offset(tool_id, 30), 0);
        assert!(collapsed_text.contains("output 40"));
        assert!(!collapsed_text.contains("paused · End to follow"));
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

        let (_, compact) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&compact.output_scroll_metrics);
        app.activate_transcript_entry(tool_id);
        let (tail, expanded) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&expanded.output_scroll_metrics);
        let before_scroll = cache.stats();
        app.scroll_tool_output_by(tool_id, 1);
        let (history, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let after_scroll = cache.stats();

        assert!(tail.contains("output 40"));
        assert!(!history.contains("output 40"));
        assert_eq!(after_scroll.0, before_scroll.0);
        assert!(after_scroll.1 > before_scroll.1);

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
    fn oversized_output_reuses_bounded_indexing_across_redraw_and_scroll() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 98,
            name: "terminal".into(),
            summary: "Render oversized output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Oversized output".into(),
                command: "generate-oversized-output".into(),
                output: (0..100_000)
                    .map(|line| {
                        if line >= 99_980 {
                            format!("oversized {line}")
                        } else {
                            "oversized".to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
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
        app.activate_transcript_entry(tool_id);
        let (rendered, first) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&first.output_scroll_metrics);
        let first_stats = cache.tool_output_stats();
        app.scroll_tool_output_by(tool_id, 1);
        let (scrolled, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let second_stats = cache.tool_output_stats();
        let (entries, bytes) = cache.tool_output_cache_stats();

        assert!(rendered.contains("oversized 99999"));
        assert!(scrolled.contains("oversized 99994"));
        assert!(!scrolled.contains("oversized 99999"));
        assert_eq!(first_stats.0, 1);
        assert_eq!(second_stats.0, first_stats.0);
        assert_eq!(second_stats.1, first_stats.1);
        assert!(second_stats.2.saturating_sub(first_stats.2) <= 24);
        assert_eq!(entries, 1);
        assert!(bytes <= super::TOOL_OUTPUT_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn sparse_terminal_streaming_indexes_only_appended_chunks() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 99,
            name: "terminal".into(),
            summary: "Stream oversized output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Oversized stream".into(),
                command: "stream-oversized-output".into(),
                output: "seed\n".repeat(105_000),
                exit_code: None,
            })],
        });

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let initial = cache.tool_output_stats();
        assert_eq!(initial.0, 1);

        let mut rendered = String::new();
        for chunk in 0..5 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 99,
                chunk: format!("{}delta-{chunk}\n", "x\n".repeat(2_000)),
            });
            rendered = render_transcript_at(&app, &cache, &Theme::default(), 80, 24).0;
        }
        for chunk in ["partial-sparse-tail", "-continued"] {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 99,
                chunk: chunk.into(),
            });
            rendered = render_transcript_at(&app, &cache, &Theme::default(), 80, 24).0;
        }

        let final_stats = cache.tool_output_stats();
        let (entries, bytes) = cache.tool_output_cache_stats();
        assert!(rendered.contains("partial-sparse-tail-continued"));
        assert_eq!(
            final_stats.0, initial.0,
            "sparse deltas must not trigger whole-layout rebuilds"
        );
        assert!(
            final_stats.1.saturating_sub(initial.1) <= 10_055,
            "only appended sparse rows should be indexed: {initial:?} -> {final_stats:?}"
        );
        assert_eq!(entries, 1);
        assert!(bytes <= super::TOOL_OUTPUT_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn row_heavy_subthreshold_output_reuses_a_bounded_sparse_index() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = "x\n".repeat(250_000);
        assert!(output.len() < super::TOOL_OUTPUT_SPARSE_SOURCE_BYTES);
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 100,
            name: "terminal".into(),
            summary: "Render row-heavy output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Row-heavy output".into(),
                command: "generate-short-lines".into(),
                output,
                exit_code: None,
            })],
        });

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let first = cache.tool_output_stats();
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let second = cache.tool_output_stats();
        let (entries, bytes) = cache.tool_output_cache_stats();

        assert_eq!(first.0, 1);
        assert_eq!(second.0, first.0, "unchanged redraw must reuse its index");
        assert_eq!(second.1, first.1);
        assert_eq!(entries, 1);
        assert!(bytes <= super::TOOL_OUTPUT_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn dense_terminal_stream_promotes_once_before_exceeding_the_memory_bound() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 101,
            name: "terminal".into(),
            summary: "Grow a row-heavy stream".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Growing row-heavy stream".into(),
                command: "stream-short-lines".into(),
                output: "x\n".repeat(10_000),
                exit_code: None,
            })],
        });

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let initial = cache.tool_output_stats();
        assert_eq!(initial.0, 1);
        assert_eq!(
            cache.tool_output_sparse_layouts(),
            0,
            "the regression must begin with a dense layout"
        );

        let mut rendered = String::new();
        for chunk in 0..10 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 101,
                chunk: format!("{}promotion-{chunk}\n", "x\n".repeat(2_000)),
            });
            rendered = render_transcript_at(&app, &cache, &Theme::default(), 80, 24).0;
        }

        let final_stats = cache.tool_output_stats();
        let (entries, bytes) = cache.tool_output_cache_stats();
        assert!(rendered.contains("promotion-9"));
        assert_eq!(
            final_stats.0, initial.0,
            "dense-to-sparse promotion must stay inside the incremental layout path"
        );
        assert!(
            final_stats.1.saturating_sub(initial.1) <= 65_000,
            "promotion may index the current body once, never once per later delta"
        );
        assert_eq!(entries, 1);
        assert!(bytes <= super::TOOL_OUTPUT_LAYOUT_CACHE_BYTES);
        assert_eq!(
            cache.tool_output_sparse_layouts(),
            1,
            "the growing layout must promote to sparse storage"
        );
    }

    #[test]
    fn dense_terminal_stream_crosses_the_source_byte_threshold_once() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let initial_output = "x".repeat(super::TOOL_OUTPUT_SPARSE_SOURCE_BYTES - 16 * 1024);
        assert!(initial_output.len() < super::TOOL_OUTPUT_SPARSE_SOURCE_BYTES);
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 102,
            name: "terminal".into(),
            summary: "Cross the sparse source threshold".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Low-row-cost growing stream".into(),
                command: "stream-long-lines".into(),
                output: initial_output,
                exit_code: None,
            })],
        });

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let dense = cache.tool_output_stats();
        assert_eq!(dense.0, 1);
        assert_eq!(
            cache.tool_output_sparse_layouts(),
            0,
            "low-row-cost output below 512 KiB must begin dense"
        );

        app.transcript.apply(TranscriptEvent::ToolOutputDelta {
            turn_id: 1,
            call_id: 102,
            chunk: format!("{}\nsource-threshold-crossed\n", "y".repeat(16 * 1024)),
        });
        let promoted_render = render_transcript_at(&app, &cache, &Theme::default(), 80, 24).0;
        let promoted = cache.tool_output_stats();
        assert!(promoted_render.contains("source-threshold-crossed"));
        assert_eq!(promoted.0, dense.0, "promotion must not rebuild the layout");
        assert_eq!(
            cache.tool_output_sparse_layouts(),
            1,
            "crossing 512 KiB must promote the cached layout exactly once"
        );
        assert!(
            promoted.1.saturating_sub(dense.1) <= 7_000,
            "promotion should index the current body once: {dense:?} -> {promoted:?}"
        );

        let mut rendered = String::new();
        for chunk in 0..4 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 102,
                chunk: format!("{}\ntail-{chunk}\n", "z".repeat(4 * 1024)),
            });
            rendered = render_transcript_at(&app, &cache, &Theme::default(), 80, 24).0;
        }

        let final_stats = cache.tool_output_stats();
        let (entries, bytes) = cache.tool_output_cache_stats();
        assert!(rendered.contains("tail-3"));
        assert_eq!(final_stats.0, dense.0);
        assert_eq!(cache.tool_output_sparse_layouts(), 1);
        assert!(
            final_stats.1.saturating_sub(promoted.1) <= 240,
            "post-promotion deltas must index only their tails: {promoted:?} -> {final_stats:?}"
        );
        assert_eq!(entries, 1);
        assert!(bytes <= super::TOOL_OUTPUT_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn streaming_output_growth_indexes_only_each_new_chunk() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 94,
            name: "terminal".into(),
            summary: "Stream output".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Stream output".into(),
                command: "stream-output".into(),
                output: String::new(),
                exit_code: None,
            })],
        });
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);

        for chunk in 0..100 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 94,
                chunk: format!("chunk {chunk}\n"),
            });
            let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        }
        app.transcript.apply(TranscriptEvent::ToolOutputDelta {
            turn_id: 1,
            call_id: 94,
            chunk: "continued without a newline".into(),
        });
        let (continued, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let stats = cache.tool_output_stats();

        assert_eq!(stats.0, 1, "the full output should be indexed once");
        assert!(
            stats.1 <= 204,
            "only the appended rows should be indexed: {stats:?}"
        );
        assert!(continued.contains("continued without a newline"));

        app.transcript.apply(TranscriptEvent::ToolFinished {
            turn_id: 1,
            call_id: 94,
            summary: None,
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Stream output".into(),
                command: "stream-output".into(),
                output: "replacement final output".into(),
                exit_code: Some(0),
            })],
        });
        let (replaced, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        assert!(replaced.contains("replacement final output"));
        assert_eq!(cache.tool_output_stats().0, 2);
    }

    #[test]
    fn mixed_artifact_terminal_stream_updates_only_its_output_segment() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 96,
            name: "terminal".into(),
            summary: "Stream mixed output".into(),
            artifacts: vec![
                ToolArtifact::CodeRange(CodeRangeArtifact {
                    path: "src/app.rs".into(),
                    start_line: 1,
                    end_line: 2,
                    preview: Some("summary content stays out of the body".into()),
                }),
                ToolArtifact::Terminal(TerminalArtifact {
                    description: "Stream output".into(),
                    command: "stream-output".into(),
                    output: String::new(),
                    exit_code: None,
                }),
                ToolArtifact::TextDetail(TextDetailArtifact {
                    text: (0..2_000)
                        .map(|line| format!("stable detail {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                }),
            ],
        });
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);

        for chunk in 0..100 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 96,
                chunk: format!("chunk {chunk}\n"),
            });
            let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        }

        let stats = cache.tool_output_stats();
        assert_eq!(stats.0, 1, "mixed output should keep one full layout build");
        assert!(
            stats.1 <= 2_250,
            "stable detail rows must not be reindexed for each terminal delta"
        );
    }

    #[test]
    fn newline_free_terminal_stream_rewraps_only_the_trailing_visual_row() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        app.transcript.submit(1, "run it".into(), Vec::new());
        app.transcript
            .apply(TranscriptEvent::Started { turn_id: 1 });
        app.transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 97,
            name: "terminal".into(),
            summary: "Stream one long line".into(),
            artifacts: vec![ToolArtifact::Terminal(TerminalArtifact {
                description: "Stream output".into(),
                command: "stream-output".into(),
                output: String::new(),
                exit_code: None,
            })],
        });
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);

        for _ in 0..100 {
            app.transcript.apply(TranscriptEvent::ToolOutputDelta {
                turn_id: 1,
                call_id: 97,
                chunk: "abcdefghij".repeat(10),
            });
            let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        }

        let stats = cache.tool_output_stats();
        assert_eq!(
            stats.0, 1,
            "the growing logical line should not fully rebuild"
        );
        assert!(
            stats.1 <= 450,
            "each delta should touch only its suffix and trailing visual row"
        );
    }

    #[test]
    fn paused_output_reflows_to_the_same_position_inside_one_long_source_line() {
        let mut app = App::new();
        let cache = TranscriptRenderCache::default();
        let output = (0..800)
            .map(|word| format!("word-{word:03}"))
            .collect::<Vec<_>>()
            .join(" ");
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

        let (_, compact) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&compact.output_scroll_metrics);
        app.activate_transcript_entry(tool_id);
        let (_, wide) = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        app.update_tool_output_scroll_metrics(&wide.output_scroll_metrics);
        app.scroll_tool_output_by(tool_id, 8);
        let anchor = app
            .tool_output_source_anchor(tool_id)
            .expect("paused source anchor");
        assert_eq!(anchor.logical_line, 0);
        assert!(anchor.source_byte > 0);

        let (_, narrow) = render_transcript_at(&app, &cache, &Theme::default(), 36, 24);
        let metrics = narrow
            .output_scroll_metrics
            .iter()
            .find(|metrics| metrics.entry_id == tool_id)
            .expect("narrow output metrics");
        let from_bottom = app.tool_output_scroll_offset_for_layout(
            tool_id,
            metrics.maximum,
            metrics.layout_id,
            Some(&metrics.row_index),
        );
        let top = metrics.maximum.saturating_sub(from_bottom);
        let remapped = metrics.row_index.anchor_at(top).expect("remapped anchor");
        let next = metrics.row_index.anchor_at(top.saturating_add(1));

        assert_eq!(remapped.artifact, anchor.artifact);
        assert_eq!(remapped.logical_line, anchor.logical_line);
        assert!(remapped.source_byte <= anchor.source_byte);
        assert!(next.is_none_or(|next| {
            next.logical_line != anchor.logical_line || next.source_byte > anchor.source_byte
        }));
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

        let literal = render_transcript(&app, &cache);
        assert!(literal.contains("Result"));
        assert!(!literal.contains("# Result"));
        assert!(literal.contains("care"));
        assert!(!literal.contains("**care**"));
        assert!(!literal.contains("`cargo test`"));
        assert_eq!(cache.markdown_layout_builds(), 0);
        wait_for_markdown(&app, &cache);
        assert_eq!(cache.markdown_layout_builds(), 1);
        let terminal = render_transcript(&app, &cache);
        assert!(terminal.contains("Result"));
        assert!(terminal.contains("Use care and cargo test."));
        assert!(!terminal.contains("# Result"));
        assert!(!terminal.contains("**care**"));

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
        app.handle_agent_event(AgentEvent::Completed { request_id: 21 });
        let _ = render_transcript(&app, &cache);
        wait_for_markdown(&app, &cache);
        assert_eq!(cache.markdown_layout_builds(), 2);
    }

    #[test]
    fn markdown_layout_cache_reuses_an_older_width_as_a_true_lru_entry() {
        let mut app = App::new();
        app.transcript.submit(22, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 22 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 22,
            text: "# Result\n\nA response that wraps at different widths.".into(),
        });
        let cache = TranscriptRenderCache::default();

        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 60, 24);
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        wait_for_markdown(&app, &cache);

        assert_eq!(cache.markdown_layout_builds(), 2);
    }

    #[test]
    fn manual_scroll_keeps_the_same_entry_row_anchored_when_markdown_reflows() {
        let mut app = App::new();
        app.transcript.submit(31, "first".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 31 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 31,
            text: "wrapping words ".repeat(80),
        });
        app.handle_agent_event(AgentEvent::Completed { request_id: 31 });
        app.transcript.submit(32, "second".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 32 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 32,
            text: (0..40)
                .map(|index| format!("anchor-{index:02}"))
                .collect::<Vec<_>>()
                .join("  \n"),
        });
        let cache = TranscriptRenderCache::default();

        let (_, initial) = render_transcript_at(&app, &cache, &Theme::default(), 80, 12);
        app.update_transcript_scroll_maximum(initial.scroll_maximum);
        app.scroll_transcript_by(3);
        let (wide, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 12);
        let anchored = wide
            .lines()
            .find_map(|line| {
                line.find("anchor-")
                    .map(|start| line[start..start + "anchor-00".len()].to_owned())
            })
            .expect("an anchored second-message row is visible");

        let (narrow, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 12);
        let narrow_anchor = narrow
            .lines()
            .find_map(|line| {
                line.find("anchor-")
                    .map(|start| line[start..start + "anchor-00".len()].to_owned())
            })
            .expect("a second-message row remains visible after reflow");

        assert_eq!(narrow_anchor, anchored);
    }

    #[test]
    fn manual_scroll_keeps_the_same_source_content_inside_a_reflowed_markdown_entry() {
        let mut app = App::new();
        app.transcript.submit(33, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 33 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 33,
            text: (0..300)
                .map(|index| format!("word-{index:03}"))
                .collect::<Vec<_>>()
                .join(" "),
        });
        let cache = TranscriptRenderCache::default();

        let (_, initial) = render_transcript_at(&app, &cache, &Theme::default(), 80, 12);
        app.update_transcript_scroll_maximum(initial.scroll_maximum);
        app.scroll_transcript_by(4);
        let (wide, _) = render_transcript_at(&app, &cache, &Theme::default(), 80, 12);
        let anchored = wide
            .split_whitespace()
            .find(|word| word.starts_with("word-"))
            .expect("a source word is visible")
            .to_owned();

        let (narrow, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 12);

        assert!(
            narrow.contains(&anchored),
            "expected {anchored:?} in {narrow:?}"
        );
    }

    #[test]
    fn large_streaming_markdown_builds_off_thread_and_never_applies_a_stale_revision() {
        let mut app = App::new();
        app.transcript.submit(41, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 41 });
        let source = format!("# Result\n{}", "large plain line\n".repeat(10_000));
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 41,
            text: source,
        });
        let cache = TranscriptRenderCache::default();
        let entry = app.transcript.entries().last().expect("assistant entry");

        let dispatch_started = Instant::now();
        let first = cache.markdown_layout(entry, 80).expect("literal fallback");
        assert!(
            dispatch_started.elapsed() < Duration::from_millis(16),
            "foreground Markdown dispatch exceeded one frame: {:?}",
            dispatch_started.elapsed()
        );
        assert_eq!(cache.markdown_layout_builds(), 0);
        assert_eq!(
            first.line(0, &Theme::default()).unwrap().to_string(),
            "Result"
        );

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 41,
            text: "\nlatest revision".into(),
        });
        let entry = app.transcript.entries().last().expect("updated assistant");
        let pending = cache.markdown_layout(entry, 80).expect("updated fallback");
        let pending_text = (0..pending.height())
            .filter_map(|row| pending.line(row, &Theme::default()))
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pending_text.contains("Result"));
        assert!(!pending_text.contains("# Result"));
        assert!(pending_text.contains("latest revision"));
        app.handle_agent_event(AgentEvent::Completed { request_id: 41 });

        let deadline = Instant::now() + Duration::from_secs(10);
        let semantic = loop {
            cache.drain_markdown_results();
            let entry = app.transcript.entries().last().expect("updated assistant");
            let layout = cache.markdown_layout(entry, 80).expect("current layout");
            let text = (0..layout.height())
                .filter_map(|row| layout.line(row, &Theme::default()))
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if cache.markdown_layout_builds() > 0
                && cache.markdown_cache_stats().2 == 0
                && text.contains("latest revision")
                && !text.contains("# Result")
            {
                break text;
            }
            assert!(
                Instant::now() < deadline,
                "background Markdown layout timed out"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(semantic.contains("Result"));
        assert_eq!(cache.markdown_cache_stats().2, 0);
    }

    #[test]
    fn foreground_markdown_projection_keeps_incomplete_syntax_literal_and_is_frame_bounded() {
        let source = format!("**unfinished [link](target `code {}", "x".repeat(3_000));
        assert!(source.len() < super::MARKDOWN_STREAM_REBUILD_MIN_GROWTH);

        let started = Instant::now();
        let layout = super::markdown_foreground_layout(&source, 38);
        let elapsed = started.elapsed();
        let rendered = (0..layout.height())
            .filter_map(|row| layout.line(row, &Theme::default()))
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            elapsed < Duration::from_millis(16),
            "literal projection took {elapsed:?}"
        );
        assert_eq!(rendered.replace('\n', ""), source);
        assert!(rendered.contains("**unfinished"));
    }

    #[test]
    fn foreground_markdown_projection_hides_valid_markers_before_background_work_finishes() {
        let source = "# Result\n\n**bold** and `code`\n\n```rust\nfn main() {}\n```";
        let layout = super::markdown_foreground_layout(source, 38);
        let rendered = (0..layout.height())
            .filter_map(|row| layout.line(row, &Theme::default()))
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Result"));
        assert!(rendered.contains("bold and code"));
        assert!(rendered.contains("┌─ rust"));
        assert!(!rendered.contains('#'));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains('`'));
    }

    #[test]
    fn sustained_streaming_deltas_schedule_logarithmic_full_layout_work() {
        let mut app = App::new();
        app.transcript.submit(43, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 43 });
        let cache = TranscriptRenderCache::default();

        for _ in 0..100 {
            app.handle_agent_event(AgentEvent::TextDelta {
                request_id: 43,
                text: "x".repeat(1_024),
            });
            let entry = app
                .transcript
                .entries()
                .last()
                .expect("streaming assistant");
            drop(cache.markdown_layout(entry, 80));
        }
        app.handle_agent_event(AgentEvent::Completed { request_id: 43 });
        let entry = app
            .transcript
            .entries()
            .last()
            .expect("completed assistant");
        drop(cache.markdown_layout(entry, 80));

        let (requests, bytes) = cache.markdown_request_stats();
        assert!(requests <= 8, "scheduled {requests} full layout builds");
        assert!(bytes <= 2 * 100 * 1_024, "queued {bytes} source bytes");
    }

    #[test]
    fn punctuation_and_unicode_deltas_keep_one_incremental_foreground_projection() {
        let mut app = App::new();
        app.transcript.submit(46, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 46 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 46,
            text: "x".repeat(super::MARKDOWN_FOREGROUND_REPROJECT_BYTES + 1),
        });
        let cache = TranscriptRenderCache::default();
        let entry = app.transcript.entries().last().expect("assistant entry");
        let first = cache.markdown_layout(entry, 80).expect("foreground layout");
        let allocation = std::sync::Arc::as_ptr(&first) as usize;
        drop(first);

        for _ in 0..1_600 {
            app.handle_agent_event(AgentEvent::TextDelta {
                request_id: 46,
                text: "`_λ[]_`\n".into(),
            });
            let entry = app.transcript.entries().last().expect("assistant entry");
            let layout = cache.markdown_layout(entry, 80).expect("foreground layout");
            assert_eq!(std::sync::Arc::as_ptr(&layout) as usize, allocation);
        }
        let entry = app.transcript.entries().last().expect("assistant entry");
        assert!(matches!(
            &entry.kind,
            EntryKind::Assistant(message) if message.text.len() > 16 * 1024
        ));
        let (requests, _) = cache.markdown_request_stats();
        assert!(requests <= 8, "scheduled {requests} background builds");
    }

    #[test]
    fn an_uncacheable_semantic_layout_keeps_complete_literal_text_without_requeueing() {
        const SOURCE_LEN: usize = 300_000;
        let mut app = App::new();
        app.transcript.submit(44, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 44 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 44,
            text: "x".repeat(SOURCE_LEN),
        });
        let cache = TranscriptRenderCache::default();
        let entry = app.transcript.entries().last().expect("assistant entry");
        let literal = cache.markdown_layout(entry, 3).expect("literal projection");
        let content_width = super::horizontally_padded_content_width(
            3,
            super::ASSISTANT_MESSAGE_HORIZONTAL_PADDING_COLUMNS,
        );
        let expected_height = SOURCE_LEN.div_ceil(content_width);
        assert_eq!(literal.height(), expected_height);
        assert_eq!(literal.line(0, &Theme::default()).unwrap().to_string(), "x");

        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.markdown_cache_stats().2 != 0 {
            cache.drain_markdown_results();
            assert!(Instant::now() < deadline, "oversized layout did not settle");
            thread::sleep(Duration::from_millis(2));
        }
        let requests = cache.markdown_request_stats();
        for _ in 0..3 {
            let entry = app.transcript.entries().last().expect("assistant entry");
            let repeated = cache.markdown_layout(entry, 3).expect("literal projection");
            assert_eq!(repeated.height(), expected_height);
            assert!(std::sync::Arc::ptr_eq(&literal, &repeated));
        }
        assert_eq!(cache.markdown_request_stats(), requests);
    }

    #[test]
    fn oversized_and_unicode_heavy_foreground_projections_are_reused() {
        let mut app = App::new();
        app.transcript.submit(47, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 47 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 47,
            text: format!("**{}**", "x".repeat(super::MARKDOWN_LAYOUT_CACHE_BYTES + 1)),
        });
        let cache = TranscriptRenderCache::default();
        let entry = app.transcript.entries().last().expect("assistant entry");
        let oversized = cache
            .markdown_layout(entry, 80)
            .expect("oversized projection");
        assert!(
            !oversized
                .line(0, &Theme::default())
                .unwrap()
                .to_string()
                .contains("**")
        );
        drop(oversized);
        let entry = app.transcript.entries().last().expect("assistant entry");
        let repeated = cache
            .markdown_layout(entry, 80)
            .expect("oversized projection");
        assert!(
            !repeated
                .line(0, &Theme::default())
                .unwrap()
                .to_string()
                .contains("**")
        );
        let (_, bytes, _, _) = cache.markdown_cache_stats();
        assert!(bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);

        let unicode = MarkdownLayout::literal(&"λ".repeat(200_000), 1);
        assert_eq!(unicode.height(), 200_000);
        assert_eq!(
            unicode
                .line(199_999, &Theme::default())
                .unwrap()
                .to_string(),
            "λ"
        );
        assert!(unicode.bytes() < super::MARKDOWN_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn markdown_runner_coalesces_pending_deltas_and_rejects_work_after_shutdown() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let first_build = std::sync::Arc::clone(&first);
        let mut runner = super::MarkdownLayoutRunner::spawn_with(move |source, width| {
            if first_build.swap(false, std::sync::atomic::Ordering::SeqCst) {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            MarkdownLayout::literal(source, width)
        });

        runner
            .request(super::MarkdownLayoutRequest {
                entry_id: 1,
                key: super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Replace("blocked".into()),
                content_width: 78,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        runner
            .request(super::MarkdownLayoutRequest {
                entry_id: 2,
                key: super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Replace("old".into()),
                content_width: 78,
            })
            .unwrap();
        let superseded = runner
            .request(super::MarkdownLayoutRequest {
                entry_id: 2,
                key: super::MarkdownLayoutKey {
                    revision: 2,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Append {
                    from_len: 3,
                    suffix: " new".into(),
                },
                content_width: 78,
            })
            .unwrap();
        assert_eq!(
            superseded,
            vec![(
                2,
                super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80
                }
            )]
        );
        release_tx.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let coalesced = loop {
            if let Some(result) = runner.try_result()
                && result.entry_id == 2
            {
                break result;
            }
            assert!(Instant::now() < deadline, "coalesced result timed out");
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(coalesced.key.revision, 2);
        let layout = coalesced.layout.expect("coalesced layout");
        assert_eq!(
            layout.line(0, &Theme::default()).unwrap().to_string(),
            "old new"
        );

        runner.stop(true);
        assert!(
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id: 3,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80
                    },
                    source: super::MarkdownSourceUpdate::Replace("late".into()),
                    content_width: 78,
                })
                .is_err()
        );
    }

    #[test]
    fn markdown_runner_versions_sources_across_widths_without_duplicate_appends() {
        let mut runner = super::MarkdownLayoutRunner::spawn_with(MarkdownLayout::literal);
        let request = |revision, width, source| super::MarkdownLayoutRequest {
            entry_id: 7,
            key: super::MarkdownLayoutKey { revision, width },
            source,
            content_width: width.saturating_sub(2),
        };
        runner
            .request(request(
                1,
                80,
                super::MarkdownSourceUpdate::Replace("old".into()),
            ))
            .unwrap();
        let first = wait_for_runner_result(&runner, 7, 1, 80);
        assert!(first.layout.is_some());

        runner
            .request(request(
                2,
                80,
                super::MarkdownSourceUpdate::Append {
                    from_len: 3,
                    suffix: " new".into(),
                },
            ))
            .unwrap();
        let updated = wait_for_runner_result(&runner, 7, 2, 80);
        assert_eq!(runner_result_text(updated), "old new");

        runner
            .request(request(
                1,
                40,
                super::MarkdownSourceUpdate::Replace("old".into()),
            ))
            .unwrap();
        assert!(wait_for_runner_result(&runner, 7, 1, 40).layout.is_none());
        runner
            .request(request(
                2,
                40,
                super::MarkdownSourceUpdate::Reuse { source_len: 7 },
            ))
            .unwrap();
        assert_eq!(
            runner_result_text(wait_for_runner_result(&runner, 7, 2, 40)),
            "old new"
        );
        runner.stop(true);
    }

    #[test]
    fn markdown_runner_recovers_from_builder_panics_and_detects_result_disconnect() {
        let panic_once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let should_panic = std::sync::Arc::clone(&panic_once);
        let mut runner = super::MarkdownLayoutRunner::spawn_with(move |source, width| {
            assert!(
                !should_panic.swap(false, std::sync::atomic::Ordering::SeqCst),
                "injected Markdown builder panic"
            );
            MarkdownLayout::literal(source, width)
        });
        for revision in [1, 2] {
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id: 8,
                    key: super::MarkdownLayoutKey {
                        revision,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace(format!("revision {revision}")),
                    content_width: 78,
                })
                .unwrap();
            let result = wait_for_runner_result(&runner, 8, revision, 80);
            assert_eq!(result.layout.is_some(), revision == 2);
            if revision == 1 {
                assert!(!result.retry, "deterministic builder panics must not retry");
            }
        }

        runner.disconnect_results();
        runner
            .request(super::MarkdownLayoutRequest {
                entry_id: 9,
                key: super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Replace("disconnect".into()),
                content_width: 78,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runner.stats().4 {
            assert!(Instant::now() < deadline, "worker disconnect timed out");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id: 10,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace("late".into()),
                    content_width: 78,
                })
                .is_err()
        );
        runner.stop(true);
    }

    #[test]
    fn a_builder_panic_disables_background_work_for_that_exact_revision() {
        let mut app = App::new();
        app.transcript.submit(49, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 49 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 49,
            text: "# deterministic panic".into(),
        });
        let cache = TranscriptRenderCache {
            markdown_runner: super::MarkdownLayoutRunner::spawn_with(|_, _| {
                panic!("injected persistent builder panic")
            }),
            ..TranscriptRenderCache::default()
        };
        let entry = app.transcript.entries().last().expect("assistant entry");
        let _ = cache.markdown_layout(entry, 80);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            cache.drain_markdown_results();
            if cache.markdown_cache_stats().2 == 0 && cache.markdown_runner.stats().0 == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "builder panic did not settle");
            thread::sleep(Duration::from_millis(2));
        }
        let settled_requests = cache.markdown_request_stats();
        assert_eq!(settled_requests.0, 1);

        for _ in 0..3 {
            let entry = app.transcript.entries().last().expect("assistant entry");
            assert!(cache.markdown_layout(entry, 80).is_some());
        }
        assert_eq!(cache.markdown_request_stats(), settled_requests);
        assert_eq!(cache.markdown_cache_stats().2, 0);
    }

    #[test]
    fn markdown_runner_bounds_queued_and_retained_sources_and_cancels_pending_shutdown_work() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let builds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let build_count = std::sync::Arc::clone(&builds);
        let runner = super::MarkdownLayoutRunner::spawn_with(move |source, width| {
            if build_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            MarkdownLayout::literal(source, width)
        });
        runner
            .request(super::MarkdownLayoutRequest {
                entry_id: 1,
                key: super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Replace("blocked".into()),
                content_width: 78,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for entry_id in 2..50 {
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace("x".repeat(200_000)),
                    content_width: 78,
                })
                .unwrap();
        }
        let (pending, pending_bytes, _, _, _) = runner.stats();
        assert!(pending <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(pending_bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);

        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let stopper = thread::spawn(move || {
            let mut runner = runner;
            runner.stop(true);
            stopped_tx.send(()).unwrap();
        });
        assert!(
            stopped_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "shutdown returned before the active builder released"
        );
        release_tx.send(()).unwrap();
        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        stopper.join().unwrap();
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut runner = super::MarkdownLayoutRunner::spawn_with(MarkdownLayout::literal);
        for entry_id in 100..140 {
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace("x".repeat(150_000)),
                    content_width: 78,
                })
                .unwrap();
            let _ = wait_for_runner_result(&runner, entry_id, 1, 80);
        }
        let (_, _, sources, source_bytes, _) = runner.stats();
        assert!(sources <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(source_bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);
        runner.stop(true);
    }

    #[test]
    fn markdown_runner_rejects_one_oversized_request_and_bounds_undrained_results() {
        let runner = super::MarkdownLayoutRunner::spawn_with(MarkdownLayout::literal);
        assert!(
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id: 1,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace(
                        "x".repeat(super::MARKDOWN_LAYOUT_CACHE_BYTES + 1,)
                    ),
                    content_width: 78,
                })
                .is_err()
        );
        assert_eq!(runner.stats().0, 0);
        assert_eq!(runner.stats().1, 0);

        let builds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let build_count = std::sync::Arc::clone(&builds);
        let retained = super::MarkdownLayoutRunner::spawn_with(move |source, width| {
            build_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            MarkdownLayout::literal(source, width)
        });
        let retained_len = super::MARKDOWN_LAYOUT_CACHE_BYTES - 8;
        retained
            .request(super::MarkdownLayoutRequest {
                entry_id: 90,
                key: super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                source: super::MarkdownSourceUpdate::Replace("x".repeat(retained_len)),
                content_width: 78,
            })
            .unwrap();
        let _ = wait_for_runner_result(&retained, 90, 1, 80);
        assert!(
            retained
                .request(super::MarkdownLayoutRequest {
                    entry_id: 90,
                    key: super::MarkdownLayoutKey {
                        revision: 2,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Append {
                        from_len: retained_len,
                        suffix: "0123456789abcdef".into(),
                    },
                    content_width: 78,
                })
                .is_err()
        );
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);

        for entry_id in 2..80 {
            runner
                .request(super::MarkdownLayoutRequest {
                    entry_id,
                    key: super::MarkdownLayoutKey {
                        revision: 1,
                        width: 80,
                    },
                    source: super::MarkdownSourceUpdate::Replace("x".repeat(120_000)),
                    content_width: 78,
                })
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (pending, _, _, _, _) = runner.stats();
            if pending == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "result pressure build timed out");
            thread::sleep(Duration::from_millis(2));
        }
        let (results, result_bytes) = runner.result_stats();
        assert!(results <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(result_bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);
    }

    fn wait_for_runner_result(
        runner: &super::MarkdownLayoutRunner,
        entry_id: u64,
        revision: u64,
        width: usize,
    ) -> super::MarkdownLayoutResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = runner.try_result()
                && result.entry_id == entry_id
                && result.key == (super::MarkdownLayoutKey { revision, width })
            {
                return result;
            }
            assert!(Instant::now() < deadline, "Markdown result timed out");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn runner_result_text(result: super::MarkdownLayoutResult) -> String {
        let layout = result.layout.expect("semantic Markdown layout");
        (0..layout.height())
            .filter_map(|row| layout.line(row, &Theme::default()))
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn completed_fenced_layout_replaces_and_remeasures_its_literal_frame() {
        let mut app = App::new();
        app.transcript.submit(42, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 42 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 42,
            text: "```rust\nfn main() {}\n```".into(),
        });
        let cache = TranscriptRenderCache::default();

        let entry = app.transcript.entries().last().expect("assistant entry");
        let dispatch_started = Instant::now();
        let _ = cache.markdown_layout(entry, 80);
        assert!(
            dispatch_started.elapsed() < Duration::from_millis(16),
            "cold fenced dispatch initialized highlighting on the foreground thread"
        );

        let literal = render_transcript(&app, &cache);
        assert!(literal.contains("┌─ rust"));
        assert!(!literal.contains("```rust"));
        assert_eq!(cache.markdown_layout_builds(), 0);

        let deadline = Instant::now() + Duration::from_secs(10);
        let semantic = loop {
            if cache.drain_markdown_results() {
                let rendered = render_transcript(&app, &cache);
                if rendered.contains("┌─ rust") {
                    break rendered;
                }
            }
            assert!(
                Instant::now() < deadline,
                "background Markdown layout timed out"
            );
            thread::sleep(Duration::from_millis(5));
        };

        assert!(!semantic.contains("```rust"));
        assert_eq!(cache.markdown_layout_builds(), 1);
    }

    #[test]
    fn async_semantic_reflow_keeps_manual_scroll_on_the_same_source_content() {
        let mut app = App::new();
        app.transcript.submit(43, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 43 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 43,
            text: format!(
                "**{}",
                (0..250)
                    .map(|index| format!("word-{index:03} "))
                    .collect::<String>()
            ),
        });
        let cache = TranscriptRenderCache::default();
        let (_, initial) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        wait_for_markdown(&app, &cache);
        app.update_transcript_scroll_maximum(initial.scroll_maximum);
        app.scroll_transcript_by(8);
        let (before, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        let anchored = before
            .split_whitespace()
            .find(|word| word.starts_with("word-"))
            .expect("a source word is visible")
            .to_owned();

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 43,
            text: "**".into(),
        });
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        wait_for_markdown(&app, &cache);
        let (after, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);

        assert!(
            after.contains(&anchored),
            "expected {anchored:?} to remain visible after async reflow: {after:?}"
        );
    }

    #[test]
    fn literal_to_semantic_replacement_keeps_the_exact_visible_source_word() {
        let mut app = App::new();
        app.transcript.submit(45, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 45 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 45,
            text: format!(
                "**{}**",
                (0..400)
                    .map(|index| format!("anchor-{index:03} "))
                    .collect::<String>()
            ),
        });
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let first_build = std::sync::Arc::clone(&first);
        let cache = TranscriptRenderCache {
            markdown_runner: super::MarkdownLayoutRunner::spawn_with(move |source, width| {
                if first_build.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                MarkdownLayout::new(source, width)
            }),
            ..TranscriptRenderCache::default()
        };
        let (_, initial) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        app.update_transcript_scroll_maximum(initial.scroll_maximum);
        app.scroll_transcript_by(12);
        let (literal, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        let anchored = literal
            .split_whitespace()
            .find(|word| word.starts_with("anchor-"))
            .expect("a literal source word is visible")
            .trim_matches('*')
            .to_owned();

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            cache.drain_markdown_results_for(&app);
            let (_, _, pending, _) = cache.markdown_cache_stats();
            if pending == 0 && cache.markdown_runner.stats().0 == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "semantic layout timed out");
            thread::sleep(Duration::from_millis(2));
        }
        let (semantic, _) = render_transcript_at(&app, &cache, &Theme::default(), 60, 10);
        let semantic_anchor = semantic
            .split_whitespace()
            .find(|word| word.starts_with("anchor-"))
            .expect("a semantic source word is visible")
            .trim_matches('*');

        assert_eq!(semantic_anchor, anchored);
    }

    #[test]
    fn markdown_layout_lru_enforces_both_entry_and_byte_bounds() {
        let cache = TranscriptRenderCache::default();
        for revision in 0..=super::MARKDOWN_LAYOUT_CACHE_CAPACITY {
            cache.store_markdown_layout(
                1,
                super::MarkdownLayoutKey {
                    revision: revision as u64,
                    width: 80,
                },
                std::sync::Arc::new(MarkdownLayout::new("small", 78)),
            );
        }
        let (entries, bytes, _, _) = cache.markdown_cache_stats();
        assert_eq!(entries, super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);

        cache.store_markdown_layout(
            2,
            super::MarkdownLayoutKey {
                revision: 99,
                width: 1,
            },
            std::sync::Arc::new(MarkdownLayout::literal(&"x".repeat(170_000), 1)),
        );
        let (entries, bytes, _, _) = cache.markdown_cache_stats();
        assert!(entries <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);

        let mut app = App::new();
        let fallback_cache = TranscriptRenderCache::default();
        for request_id in 100..140 {
            app.transcript
                .submit(request_id, "prompt".into(), Vec::new());
            app.handle_agent_event(AgentEvent::Started { request_id });
            app.handle_agent_event(AgentEvent::TextDelta {
                request_id,
                text: "large literal response ".repeat(500),
            });
            app.handle_agent_event(AgentEvent::Completed { request_id });
        }
        for entry in app.transcript.entries() {
            let _ = fallback_cache.markdown_layout(entry, 80);
        }
        let (entries, bytes, _, fallbacks) = fallback_cache.markdown_cache_stats();
        assert!(entries <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(fallbacks <= super::MARKDOWN_LAYOUT_CACHE_CAPACITY);
        assert!(bytes <= super::MARKDOWN_LAYOUT_CACHE_BYTES);
    }

    #[test]
    fn markdown_layout_and_fallback_entries_share_one_recency_order() {
        let cache = TranscriptRenderCache::default();
        let oldest_semantic = super::MarkdownLayoutKey {
            revision: 1,
            width: 80,
        };
        cache.store_markdown_layout(
            1,
            oldest_semantic,
            std::sync::Arc::new(MarkdownLayout::new("old", 78)),
        );

        let mut app = App::new();
        app.transcript.submit(90, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 90 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 90,
            text: "fallback".into(),
        });
        let assistant = app.transcript.entries().last().expect("assistant entry");
        let _ = cache.markdown_layout(assistant, 80);

        cache
            .inner
            .borrow_mut()
            .touch_markdown(super::MarkdownCacheToken::Semantic(1, oldest_semantic));
        for entry_id in 100..(99 + super::MARKDOWN_LAYOUT_CACHE_CAPACITY as u64) {
            cache.store_markdown_layout(
                entry_id,
                super::MarkdownLayoutKey {
                    revision: 1,
                    width: 80,
                },
                std::sync::Arc::new(MarkdownLayout::new("new", 78)),
            );
        }
        let inner = cache.inner.borrow();
        assert!(
            inner
                .markdown_layouts
                .iter()
                .any(|cached| { cached.entry_id == 1 && cached.key == oldest_semantic })
        );
        assert!(
            inner
                .markdown_fallbacks
                .iter()
                .all(|fallback| fallback.entry_id != assistant.id),
            "the older fallback should be evicted before the recently touched semantic layout"
        );
    }

    #[test]
    fn a_stream_delta_rebuilds_only_its_assistant_and_theme_changes_only_rendered_slices() {
        let mut app = App::new();
        for request_id in [51, 52] {
            app.transcript
                .submit(request_id, "prompt".into(), Vec::new());
            app.handle_agent_event(AgentEvent::Started { request_id });
            app.handle_agent_event(AgentEvent::TextDelta {
                request_id,
                text: format!("# Response {request_id}"),
            });
            if request_id == 51 {
                app.handle_agent_event(AgentEvent::Completed { request_id });
            }
        }
        let cache = TranscriptRenderCache::default();
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        wait_for_markdown(&app, &cache);
        assert_eq!(cache.markdown_layout_builds(), 2);

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 52,
            text: " updated".into(),
        });
        app.handle_agent_event(AgentEvent::Completed { request_id: 52 });
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        wait_for_markdown(&app, &cache);
        assert_eq!(cache.markdown_layout_builds(), 3);
        let before_theme = cache.stats();

        let _ = render_transcript_at(&app, &cache, &Theme::resolve(ThemeId::Paper), 80, 24);
        assert_eq!(cache.markdown_layout_builds(), 3);
        assert_eq!(cache.stats().0, before_theme.0);
        assert!(cache.stats().1 > before_theme.1);
    }

    #[test]
    fn test_backend_renders_each_heading_with_its_exact_theme_role_in_every_theme() {
        let mut app = App::new();
        app.transcript.submit(61, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 61 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 61,
            text: (1..=6)
                .map(|level| format!("{} Heading {level}", "#".repeat(level)))
                .collect::<Vec<_>>()
                .join("\n\n"),
        });
        let cache = TranscriptRenderCache::default();
        let _ = render_transcript_at(&app, &cache, &Theme::default(), 80, 24);
        wait_for_markdown(&app, &cache);
        let roles = [
            ThemeRole::MarkdownHeading1,
            ThemeRole::MarkdownHeading2,
            ThemeRole::MarkdownHeading3,
            ThemeRole::MarkdownHeading4,
            ThemeRole::MarkdownHeading5,
            ThemeRole::MarkdownHeading6,
        ];

        for theme_id in ThemeId::ALL {
            let theme = Theme::resolve(theme_id);
            let area = Rect::new(0, 0, 80, 24);
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| {
                    render(frame, area, &app, &theme, &cache);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for (level, role) in (1..=6).zip(roles) {
                let label = format!("Heading {level}");
                let row = (0..area.height)
                    .find(|row| {
                        (0..area.width)
                            .filter_map(|column| buffer.cell((column, *row)))
                            .map(|cell| cell.symbol())
                            .collect::<String>()
                            .contains(&label)
                    })
                    .expect("heading row");
                let cell = (0..area.width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .find(|cell| cell.symbol() == "H")
                    .expect("heading cell");
                assert_eq!(
                    cell.style().fg,
                    theme.style(role).fg,
                    "heading {level} in {theme_id:?}"
                );
            }
        }
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
