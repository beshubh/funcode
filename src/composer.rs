use crate::workspace::{Attachment, WorkspacePath};
use ropey::{Rope, RopeSlice};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fmt,
    ops::Range,
    sync::{Arc, Mutex},
};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

pub const REQUEST_CONFIRM_BYTES: usize = 100 * 1024 * 1024;
pub const REQUEST_HARD_LIMIT_BYTES: usize = 1000 * 1024 * 1024;
const TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DocumentRevision(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CharIndex(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CharRange {
    start: CharIndex,
    end: CharIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryId {
    revision: DocumentRevision,
    cursor_epoch: u64,
    range: CharRange,
    kind: QueryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryKind {
    Command,
    FileReference,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryView {
    id: QueryId,
    text: String,
    standalone: bool,
}

impl QueryView {
    pub const fn id(&self) -> QueryId {
        self.id
    }

    pub const fn kind(&self) -> QueryKind {
        self.id.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn is_standalone(&self) -> bool {
        self.standalone
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerEditError {
    StaleQuery,
    StalePaste,
    WrongQueryKind,
    RequestTooLarge { bytes: usize, limit: usize },
    ProjectionOverflow,
    AllocationFailed,
}

impl fmt::Display for ComposerEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleQuery => formatter.write_str("the completion query is stale"),
            Self::StalePaste => formatter.write_str("the paste proposal is stale"),
            Self::WrongQueryKind => formatter.write_str("the active query is not a file query"),
            Self::RequestTooLarge { bytes, limit } => {
                write!(
                    formatter,
                    "the request is {bytes} bytes; the limit is {limit} bytes"
                )
            }
            Self::ProjectionOverflow => formatter.write_str("the request size overflowed"),
            Self::AllocationFailed => formatter.write_str("the composer could not allocate memory"),
        }
    }
}

impl std::error::Error for ComposerEditError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PasteId {
    revision: DocumentRevision,
    cursor_epoch: u64,
    cursor: CharIndex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteProposal {
    id: PasteId,
    normalized: Arc<str>,
    line_count: usize,
    projected_bytes: usize,
}

impl PasteProposal {
    pub const fn requires_confirmation(&self) -> bool {
        self.projected_bytes > REQUEST_CONFIRM_BYTES
    }

    pub const fn projected_bytes(&self) -> usize {
        self.projected_bytes
    }

    pub const fn is_multiline(&self) -> bool {
        self.line_count > 1
    }

    pub const fn line_count(&self) -> usize {
        self.line_count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteCommit {
    pub multiline: bool,
    pub projected_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PastedBlock {
    raw: Arc<str>,
    line_count: usize,
}

impl PastedBlock {
    fn summary(&self) -> String {
        if self.line_count == 1 {
            "[1 line pasted]".into()
        } else {
            format!("[{} lines pasted]", self.line_count)
        }
    }
}

#[derive(Debug, Clone)]
enum Segment {
    Text(Rope),
    FileReference(WorkspacePath),
    PastedBlock(PastedBlock),
}

impl Segment {
    fn semantic_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len_chars(),
            Self::FileReference(_) | Self::PastedBlock(_) => 1,
        }
    }

    fn submission_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len_bytes(),
            Self::FileReference(path) => 1usize.saturating_add(path.display().len()),
            Self::PastedBlock(block) => block.raw.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SubmittedSegment {
    Text(Rope),
    FileReference(WorkspacePath),
    PastedBlock(PastedBlock),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubmittedContentInner {
    segments: Vec<SubmittedSegment>,
}

#[derive(Debug, Clone)]
pub struct SubmittedContent {
    inner: Arc<SubmittedContentInner>,
    layout_cache: Arc<Mutex<HashMap<usize, Arc<ComposerLayout>>>>,
}

impl PartialEq for SubmittedContent {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for SubmittedContent {}

impl Default for SubmittedContent {
    fn default() -> Self {
        Self::plain("")
    }
}

impl SubmittedContent {
    pub fn plain(text: impl AsRef<str>) -> Self {
        Self {
            inner: Arc::new(SubmittedContentInner {
                segments: vec![SubmittedSegment::Text(Rope::from_str(text.as_ref()))],
            }),
            layout_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_attachments(text: impl AsRef<str>, attachments: &[Attachment]) -> Self {
        let mut segments =
            Vec::with_capacity(attachments.len().saturating_mul(2).saturating_add(1));
        let mut plain = text.as_ref().to_owned();
        for attachment in attachments {
            if !plain.is_empty() && !plain.ends_with(char::is_whitespace) {
                plain.push(' ');
            }
            if !plain.is_empty() {
                segments.push(SubmittedSegment::Text(Rope::from_str(&plain)));
                plain.clear();
            }
            segments.push(SubmittedSegment::FileReference(attachment.path().clone()));
        }
        if !plain.is_empty() || segments.is_empty() {
            segments.push(SubmittedSegment::Text(Rope::from_str(&plain)));
        }
        Self {
            inner: Arc::new(SubmittedContentInner { segments }),
            layout_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn submission_text(&self) -> String {
        let capacity = self.submission_bytes().unwrap_or_default();
        let mut projection = String::with_capacity(capacity);
        for segment in &self.inner.segments {
            match segment {
                SubmittedSegment::Text(text) => {
                    for chunk in text.chunks() {
                        projection.push_str(chunk);
                    }
                }
                SubmittedSegment::FileReference(path) => {
                    projection.push('@');
                    projection.push_str(&path.display());
                }
                SubmittedSegment::PastedBlock(block) => projection.push_str(&block.raw),
            }
        }
        projection
    }

    pub fn visible_text(&self) -> String {
        let mut projection = String::new();
        let mut column = 0usize;
        for segment in &self.inner.segments {
            match segment {
                SubmittedSegment::Text(text) => {
                    for chunk in text.chunks() {
                        push_safe_text(&mut projection, chunk, &mut column);
                    }
                }
                SubmittedSegment::FileReference(path) => {
                    let display = format!("@{}", path.display());
                    projection.push_str(&display);
                    column = column.saturating_add(UnicodeWidthStr::width(display.as_str()));
                }
                SubmittedSegment::PastedBlock(block) => {
                    push_safe_text(&mut projection, &block.raw, &mut column);
                }
            }
        }
        projection
    }

    pub fn attachments(&self) -> Vec<Attachment> {
        let mut seen = HashSet::new();
        self.inner
            .segments
            .iter()
            .filter_map(|segment| match segment {
                SubmittedSegment::FileReference(path) if seen.insert(path.clone()) => {
                    Some(Attachment::workspace_file(path.clone()))
                }
                SubmittedSegment::FileReference(_)
                | SubmittedSegment::Text(_)
                | SubmittedSegment::PastedBlock(_) => None,
            })
            .collect()
    }

    pub fn submission_bytes(&self) -> Result<usize, ComposerEditError> {
        self.inner
            .segments
            .iter()
            .try_fold(0usize, |total, segment| {
                let bytes = match segment {
                    SubmittedSegment::Text(text) => text.len_bytes(),
                    SubmittedSegment::FileReference(path) => 1usize
                        .checked_add(path.display().len())
                        .ok_or(ComposerEditError::ProjectionOverflow)?,
                    SubmittedSegment::PastedBlock(block) => block.raw.len(),
                };
                total
                    .checked_add(bytes)
                    .ok_or(ComposerEditError::ProjectionOverflow)
            })
    }

    pub fn is_effectively_empty(&self) -> bool {
        self.inner.segments.iter().all(|segment| match segment {
            SubmittedSegment::Text(text) => text.chars().all(char::is_whitespace),
            SubmittedSegment::PastedBlock(block) => block.raw.chars().all(char::is_whitespace),
            SubmittedSegment::FileReference(_) => false,
        })
    }

    pub fn display_lines(&self, initial_column: usize) -> Vec<DisplayLine> {
        let mut builder = DisplayLineBuilder::new(initial_column);
        for segment in &self.inner.segments {
            match segment {
                SubmittedSegment::Text(text) => {
                    for grapheme in RopeGraphemes::new(text.slice(..)) {
                        builder.push_text_grapheme(grapheme.text.as_ref(), DisplayRunKind::Text);
                    }
                }
                SubmittedSegment::FileReference(path) => builder.push_atom(
                    &format!("@{}", path.display()),
                    DisplayRunKind::FileReference,
                ),
                SubmittedSegment::PastedBlock(block) => {
                    for grapheme in block.raw.graphemes(true) {
                        builder.push_text_grapheme(grapheme, DisplayRunKind::Text);
                    }
                }
            }
        }
        builder.finish()
    }

    pub fn layout(&self, width: usize) -> Arc<ComposerLayout> {
        let width = width.max(1);
        if let Ok(cache) = self.layout_cache.lock()
            && let Some(layout) = cache.get(&width)
        {
            return layout.clone();
        }
        let segments = self
            .inner
            .segments
            .iter()
            .map(|segment| match segment {
                SubmittedSegment::Text(text) => Segment::Text(text.clone()),
                SubmittedSegment::FileReference(path) => Segment::FileReference(path.clone()),
                SubmittedSegment::PastedBlock(block) => Segment::Text(Rope::from_str(&block.raw)),
            })
            .collect::<Vec<_>>();
        let layout = Arc::new(build_layout(&segments, width));
        if let Ok(mut cache) = self.layout_cache.lock() {
            cache.clear();
            cache.insert(width, layout.clone());
        }
        layout
    }

    #[cfg(test)]
    fn segment_kinds(&self) -> Vec<&'static str> {
        self.inner
            .segments
            .iter()
            .map(|segment| match segment {
                SubmittedSegment::Text(_) => "text",
                SubmittedSegment::FileReference(_) => "file",
                SubmittedSegment::PastedBlock(_) => "paste",
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayRunKind {
    Text,
    FileReference,
    PastedBlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRun {
    pub kind: DisplayRunKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayLine {
    pub runs: Vec<DisplayRun>,
}

impl DisplayLine {
    fn push(&mut self, kind: DisplayRunKind, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.kind == kind
        {
            last.text.push_str(text);
        } else {
            self.runs.push(DisplayRun {
                kind,
                text: text.to_owned(),
            });
        }
    }
}

struct DisplayLineBuilder {
    lines: Vec<DisplayLine>,
    column: usize,
}

impl DisplayLineBuilder {
    fn new(initial_column: usize) -> Self {
        Self {
            lines: vec![DisplayLine::default()],
            column: initial_column,
        }
    }

    fn push_text_grapheme(&mut self, grapheme: &str, kind: DisplayRunKind) {
        if grapheme == "\n" {
            self.lines.push(DisplayLine::default());
            self.column = 0;
        } else if grapheme == "\t" {
            let spaces = TAB_WIDTH - self.column % TAB_WIDTH;
            self.lines
                .last_mut()
                .unwrap()
                .push(kind, &" ".repeat(spaces));
            self.column = self.column.saturating_add(spaces);
        } else {
            let safe = safe_grapheme(grapheme);
            self.lines.last_mut().unwrap().push(kind, safe.as_ref());
            self.column = self
                .column
                .saturating_add(UnicodeWidthStr::width(safe.as_ref()));
        }
    }

    fn push_atom(&mut self, display: &str, kind: DisplayRunKind) {
        for grapheme in display.graphemes(true) {
            self.push_text_grapheme(grapheme, kind);
        }
    }

    fn finish(self) -> Vec<DisplayLine> {
        self.lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorGeometry {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy)]
struct CursorAnchor {
    cursor: CharIndex,
    geometry: CursorGeometry,
}

#[derive(Debug, Clone, Copy)]
struct DisplaySpan {
    kind: DisplayRunKind,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct LogicalLineLayout {
    semantic_len: usize,
    display: String,
    spans: Vec<DisplaySpan>,
    row_ranges: Vec<Range<usize>>,
    anchors: Vec<CursorAnchor>,
    row_anchor_ranges: Vec<Range<usize>>,
}

impl LogicalLineLayout {
    fn total_rows(&self) -> usize {
        self.row_ranges.len()
    }

    fn materialize_row(&self, row: usize) -> DisplayLine {
        let mut line = DisplayLine::default();
        let Some(range) = self.row_ranges.get(row) else {
            return line;
        };
        if range.is_empty() {
            return line;
        }
        let first = self.spans.partition_point(|span| span.end <= range.start);
        for span in &self.spans[first..] {
            if span.start >= range.end {
                break;
            }
            let start = span.start.max(range.start);
            let end = span.end.min(range.end);
            if start < end {
                line.push(span.kind, &self.display[start..end]);
            }
        }
        line
    }

    fn geometry(&self, cursor: CharIndex) -> Option<CursorGeometry> {
        self.anchors
            .binary_search_by_key(&cursor.0, |anchor| anchor.cursor.0)
            .ok()
            .map(|index| self.anchors[index].geometry)
    }

    fn closest_cursor(&self, row: usize, preferred_column: usize) -> Option<CharIndex> {
        let range = self.row_anchor_ranges.get(row)?.clone();
        self.anchors[range]
            .iter()
            .min_by(|left, right| {
                left.geometry
                    .column
                    .abs_diff(preferred_column)
                    .cmp(&right.geometry.column.abs_diff(preferred_column))
                    .then_with(|| right.cursor.0.cmp(&left.cursor.0))
            })
            .map(|anchor| anchor.cursor)
    }
}

#[derive(Debug)]
pub struct ComposerLayout {
    width: usize,
    lines: Vec<Arc<LogicalLineLayout>>,
    line_cursor_starts: Vec<usize>,
    line_row_starts: Vec<usize>,
    total_rows: usize,
}

impl ComposerLayout {
    fn from_lines(width: usize, lines: Vec<Arc<LogicalLineLayout>>) -> Self {
        let mut line_cursor_starts = Vec::with_capacity(lines.len());
        let mut line_row_starts = Vec::with_capacity(lines.len());
        let mut cursor = 0usize;
        let mut row = 0usize;
        for (index, line) in lines.iter().enumerate() {
            line_cursor_starts.push(cursor);
            line_row_starts.push(row);
            cursor = cursor.saturating_add(line.semantic_len);
            if index + 1 < lines.len() {
                row = row.saturating_add(line.total_rows().saturating_sub(1));
            }
        }
        let total_rows = lines
            .last()
            .map_or(1, |line| row.saturating_add(line.total_rows()));
        Self {
            width,
            lines,
            line_cursor_starts,
            line_row_starts,
            total_rows,
        }
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn visible_rows(&self, start: usize, height: usize) -> Vec<DisplayLine> {
        let start = start.min(self.total_rows);
        let end = start.saturating_add(height).min(self.total_rows);
        (start..end)
            .filter_map(|row| {
                let (line, local_row) = self.line_at_row(row)?;
                Some(line.materialize_row(local_row))
            })
            .collect()
    }

    fn geometry(&self, cursor: CharIndex) -> Option<CursorGeometry> {
        let index = self
            .line_cursor_starts
            .partition_point(|start| *start <= cursor.0)
            .saturating_sub(1);
        let line = self.lines.get(index)?;
        let cursor_start = self.line_cursor_starts[index];
        let row_start = self.line_row_starts[index];
        let local = CharIndex(cursor.0.saturating_sub(cursor_start));
        line.geometry(local).map(|geometry| CursorGeometry {
            row: row_start.saturating_add(geometry.row),
            column: geometry.column,
        })
    }

    fn closest_cursor(&self, row: usize, preferred_column: usize) -> Option<CharIndex> {
        let index = self
            .line_row_starts
            .partition_point(|start| *start <= row)
            .saturating_sub(1);
        let line = self.lines.get(index)?;
        let local_row = row.saturating_sub(self.line_row_starts[index]);
        line.closest_cursor(local_row, preferred_column)
            .map(|cursor| CharIndex(self.line_cursor_starts[index].saturating_add(cursor.0)))
    }

    fn line_at_row(&self, row: usize) -> Option<(&LogicalLineLayout, usize)> {
        if row >= self.total_rows {
            return None;
        }
        let index = self
            .line_row_starts
            .partition_point(|start| *start <= row)
            .saturating_sub(1);
        Some((
            self.lines.get(index)?.as_ref(),
            row.saturating_sub(self.line_row_starts[index]),
        ))
    }
}

#[derive(Debug, Clone)]
enum LogicalLineSegment {
    Text { rope: Rope, range: Range<usize> },
    FileReference(WorkspacePath),
    PastedBlock(PastedBlock),
}

impl PartialEq for LogicalLineSegment {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Text {
                    rope: left,
                    range: left_range,
                },
                Self::Text {
                    rope: right,
                    range: right_range,
                },
            ) => {
                left_range.len() == right_range.len()
                    && left.slice(left_range.clone()) == right.slice(right_range.clone())
            }
            (Self::FileReference(left), Self::FileReference(right)) => left == right,
            (Self::PastedBlock(left), Self::PastedBlock(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for LogicalLineSegment {}

#[derive(Debug, Clone, Default)]
struct LogicalLineSource {
    segments: Vec<LogicalLineSegment>,
}

impl PartialEq for LogicalLineSource {
    fn eq(&self, other: &Self) -> bool {
        self.segments == other.segments
    }
}

impl Eq for LogicalLineSource {}

#[derive(Debug)]
struct LayoutCacheEntry {
    revision: DocumentRevision,
    width: usize,
    sources: Vec<LogicalLineSource>,
    layout: Arc<ComposerLayout>,
}

#[derive(Debug)]
pub struct ComposerDocument {
    segments: Vec<Segment>,
    cursor: CharIndex,
    revision: DocumentRevision,
    cursor_epoch: u64,
    preferred_column: Option<usize>,
    query_cache: RefCell<Option<(u64, Option<QueryView>)>>,
    layout_cache: RefCell<Option<LayoutCacheEntry>>,
    layout_builds: Cell<usize>,
}

impl Clone for ComposerDocument {
    fn clone(&self) -> Self {
        Self {
            segments: self.segments.clone(),
            cursor: self.cursor,
            revision: self.revision,
            cursor_epoch: self.cursor_epoch,
            preferred_column: self.preferred_column,
            query_cache: RefCell::new(None),
            layout_cache: RefCell::new(None),
            layout_builds: Cell::new(0),
        }
    }
}

impl Default for ComposerDocument {
    fn default() -> Self {
        Self {
            segments: vec![Segment::Text(Rope::new())],
            cursor: CharIndex(0),
            revision: DocumentRevision::default(),
            cursor_epoch: 0,
            preferred_column: None,
            query_cache: RefCell::new(None),
            layout_cache: RefCell::new(None),
            layout_builds: Cell::new(0),
        }
    }
}

impl ComposerDocument {
    pub const fn revision(&self) -> DocumentRevision {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.semantic_len() == 0
    }

    pub fn has_structural_atoms(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| !matches!(segment, Segment::Text(_)))
    }

    pub fn cursor_is_at_end(&self) -> bool {
        self.cursor.0 == self.semantic_len()
    }

    pub fn freeze(&self) -> SubmittedContent {
        SubmittedContent {
            inner: Arc::new(SubmittedContentInner {
                segments: self
                    .segments
                    .iter()
                    .map(|segment| match segment {
                        Segment::Text(text) => SubmittedSegment::Text(text.clone()),
                        Segment::FileReference(path) => {
                            SubmittedSegment::FileReference(path.clone())
                        }
                        Segment::PastedBlock(block) => SubmittedSegment::PastedBlock(block.clone()),
                    })
                    .collect(),
            }),
            layout_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn visible_text(&self) -> String {
        let mut projection = String::new();
        let mut column = 0usize;
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => {
                    for chunk in text.chunks() {
                        push_safe_text(&mut projection, chunk, &mut column);
                    }
                }
                Segment::FileReference(path) => {
                    let display = format!("@{}", path.display());
                    projection.push_str(&display);
                    column = column.saturating_add(UnicodeWidthStr::width(display.as_str()));
                }
                Segment::PastedBlock(block) => {
                    let summary = block.summary();
                    projection.push_str(&summary);
                    column = column.saturating_add(UnicodeWidthStr::width(summary.as_str()));
                }
            }
        }
        projection
    }

    pub fn submission_text(&self) -> String {
        self.freeze().submission_text()
    }

    pub fn attachments(&self) -> Vec<Attachment> {
        self.freeze().attachments()
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.insert_text_unchecked(text);
        self.finish_edit();
    }

    pub fn move_left(&mut self) {
        let old = self.cursor;
        if self.cursor.0 == 0 {
            return;
        }
        let mut offset = 0usize;
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 > offset && self.cursor.0 <= end {
                        let local = self.cursor.0 - offset;
                        self.cursor = CharIndex(
                            offset.saturating_add(previous_grapheme_boundary(text, local)),
                        );
                        break;
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset.saturating_add(1) {
                        self.cursor = CharIndex(offset);
                        break;
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
        self.finish_cursor_move(old);
    }

    pub fn move_right(&mut self) {
        let old = self.cursor;
        let mut offset = 0usize;
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 >= offset && self.cursor.0 < end {
                        let local = self.cursor.0 - offset;
                        self.cursor =
                            CharIndex(offset.saturating_add(next_grapheme_boundary(text, local)));
                        break;
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset {
                        self.cursor = CharIndex(offset.saturating_add(1));
                        break;
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
        self.finish_cursor_move(old);
    }

    pub fn move_home(&mut self) {
        let old = self.cursor;
        let target = self
            .text_location_at_cursor()
            .and_then(|(segment_index, local, global_start)| {
                let Segment::Text(text) = &self.segments[segment_index] else {
                    return None;
                };
                let line = text.char_to_line(local);
                Some(CharIndex(
                    global_start.saturating_add(text.line_to_char(line)),
                ))
            })
            .unwrap_or(CharIndex(0));
        self.cursor = target;
        self.finish_cursor_move(old);
    }

    pub fn move_end(&mut self) {
        let old = self.cursor;
        let target = self
            .text_location_at_cursor()
            .and_then(|(segment_index, local, global_start)| {
                let Segment::Text(text) = &self.segments[segment_index] else {
                    return None;
                };
                let line = text.char_to_line(local);
                let line_start = text.line_to_char(line);
                let mut line_end = line_start.saturating_add(text.line(line).len_chars());
                if line_end > line_start && text.char(line_end - 1) == '\n' {
                    line_end -= 1;
                }
                Some(CharIndex(global_start.saturating_add(line_end)))
            })
            .unwrap_or(CharIndex(self.semantic_len()));
        self.cursor = target;
        self.finish_cursor_move(old);
    }

    pub fn move_up(&mut self, width: usize) {
        self.move_vertical(width, -1);
    }

    pub fn move_down(&mut self, width: usize) {
        self.move_vertical(width, 1);
    }

    pub fn move_to_visual_position(
        &mut self,
        width: usize,
        viewport_height: usize,
        visible_row: usize,
        column: usize,
    ) {
        let layout = self.layout(width);
        let current = self.cursor_geometry(&layout);
        let vertical_scroll = current
            .row
            .saturating_sub(viewport_height.saturating_sub(1));
        let row = vertical_scroll
            .saturating_add(visible_row)
            .min(layout.total_rows().saturating_sub(1));
        let Some(cursor) = layout.closest_cursor(row, column) else {
            return;
        };
        let old = self.cursor;
        self.cursor = cursor;
        self.finish_cursor_move(old);
    }

    pub fn backspace(&mut self) {
        if self.cursor.0 == 0 {
            return;
        }
        let mut offset = 0usize;
        for index in 0..self.segments.len() {
            match &self.segments[index] {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 > offset && self.cursor.0 <= end {
                        let local_end = self.cursor.0 - offset;
                        let local_start = previous_grapheme_boundary(text, local_end);
                        if let Segment::Text(text) = &mut self.segments[index] {
                            text.remove(local_start..local_end);
                        }
                        self.cursor = CharIndex(offset.saturating_add(local_start));
                        self.finish_edit();
                        return;
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset.saturating_add(1) {
                        self.segments.remove(index);
                        self.cursor = CharIndex(offset);
                        self.finish_edit();
                        return;
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
    }

    pub fn delete(&mut self) {
        let mut offset = 0usize;
        for index in 0..self.segments.len() {
            match &self.segments[index] {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 >= offset && self.cursor.0 < end {
                        let local_start = self.cursor.0 - offset;
                        let local_end = next_grapheme_boundary(text, local_start);
                        if let Segment::Text(text) = &mut self.segments[index] {
                            text.remove(local_start..local_end);
                        }
                        self.finish_edit();
                        return;
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset {
                        self.segments.remove(index);
                        self.finish_edit();
                        return;
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
    }

    pub fn active_query(&self) -> Option<QueryView> {
        if let Some((epoch, cached)) = self.query_cache.borrow().as_ref()
            && *epoch == self.cursor_epoch
        {
            return cached.clone();
        }
        let query = self.detect_active_query();
        *self.query_cache.borrow_mut() = Some((self.cursor_epoch, query.clone()));
        query
    }

    pub fn complete_file_reference(
        &mut self,
        id: QueryId,
        path: WorkspacePath,
    ) -> Result<(), ComposerEditError> {
        let Some(active) = self.active_query() else {
            return Err(ComposerEditError::StaleQuery);
        };
        if active.id != id {
            return Err(ComposerEditError::StaleQuery);
        }
        if id.kind != QueryKind::FileReference {
            return Err(ComposerEditError::WrongQueryKind);
        }
        self.replace_range_with_atom(id.range, Segment::FileReference(path))?;
        self.finish_edit();
        Ok(())
    }

    pub fn discard_active_command(&mut self, id: QueryId) -> Result<(), ComposerEditError> {
        let Some(active) = self.active_query() else {
            return Err(ComposerEditError::StaleQuery);
        };
        if active.id != id {
            return Err(ComposerEditError::StaleQuery);
        }
        if id.kind != QueryKind::Command {
            return Err(ComposerEditError::WrongQueryKind);
        }
        self.clear();
        Ok(())
    }

    pub fn propose_paste(&self, raw: &str) -> Result<PasteProposal, ComposerEditError> {
        let normalized_bytes = normalized_len(raw)?;
        let projected_bytes = self
            .submission_bytes()?
            .checked_add(normalized_bytes)
            .ok_or(ComposerEditError::ProjectionOverflow)?;
        if projected_bytes > REQUEST_HARD_LIMIT_BYTES {
            return Err(ComposerEditError::RequestTooLarge {
                bytes: projected_bytes,
                limit: REQUEST_HARD_LIMIT_BYTES,
            });
        }
        let normalized = normalize_paste(raw)?;
        let line_count = normalized.bytes().filter(|byte| *byte == b'\n').count() + 1;
        Ok(PasteProposal {
            id: PasteId {
                revision: self.revision,
                cursor_epoch: self.cursor_epoch,
                cursor: self.cursor,
            },
            normalized,
            line_count,
            projected_bytes,
        })
    }

    pub fn commit_paste(
        &mut self,
        proposal: PasteProposal,
    ) -> Result<PasteCommit, ComposerEditError> {
        if proposal.id.revision != self.revision
            || proposal.id.cursor_epoch != self.cursor_epoch
            || proposal.id.cursor != self.cursor
        {
            return Err(ComposerEditError::StalePaste);
        }
        let multiline = proposal.is_multiline();
        if proposal.normalized.is_empty() {
            return Ok(PasteCommit {
                multiline,
                projected_bytes: proposal.projected_bytes,
            });
        }
        self.insert_atom_at_cursor(Segment::PastedBlock(PastedBlock {
            raw: proposal.normalized,
            line_count: proposal.line_count,
        }))?;
        self.finish_edit();
        Ok(PasteCommit {
            multiline,
            projected_bytes: proposal.projected_bytes,
        })
    }

    pub fn clear(&mut self) {
        if self.is_empty() {
            return;
        }
        self.segments = vec![Segment::Text(Rope::new())];
        self.cursor = CharIndex(0);
        self.finish_edit();
    }

    pub fn layout(&self, width: usize) -> Arc<ComposerLayout> {
        let width = width.max(1);
        if let Some(entry) = self.layout_cache.borrow().as_ref()
            && entry.width == width
            && entry.revision == self.revision
        {
            return entry.layout.clone();
        }
        let sources = split_logical_lines(&self.segments);
        let previous = self
            .layout_cache
            .borrow_mut()
            .take()
            .filter(|entry| entry.width == width);
        let (layout, builds) = rebuild_layout(width, &sources, previous.as_ref());
        self.layout_builds
            .set(self.layout_builds.get().saturating_add(builds));
        *self.layout_cache.borrow_mut() = Some(LayoutCacheEntry {
            revision: self.revision,
            width,
            sources,
            layout: layout.clone(),
        });
        layout
    }

    pub fn cursor_geometry(&self, layout: &ComposerLayout) -> CursorGeometry {
        layout
            .geometry(self.cursor)
            .unwrap_or(CursorGeometry { row: 0, column: 0 })
    }

    pub fn submission_bytes(&self) -> Result<usize, ComposerEditError> {
        self.segments.iter().try_fold(0usize, |total, segment| {
            total
                .checked_add(segment.submission_bytes())
                .ok_or(ComposerEditError::ProjectionOverflow)
        })
    }

    fn semantic_len(&self) -> usize {
        self.segments
            .iter()
            .map(Segment::semantic_len)
            .fold(0usize, usize::saturating_add)
    }

    fn insert_text_unchecked(&mut self, text: &str) {
        let mut offset = 0usize;
        for index in 0..self.segments.len() {
            match &self.segments[index] {
                Segment::Text(segment) => {
                    let end = offset.saturating_add(segment.len_chars());
                    if self.cursor.0 <= end {
                        let local = self.cursor.0.saturating_sub(offset);
                        if let Segment::Text(segment) = &mut self.segments[index] {
                            segment.insert(local, text);
                        }
                        self.cursor = CharIndex(self.cursor.0.saturating_add(text.chars().count()));
                        return;
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset {
                        self.segments
                            .insert(index, Segment::Text(Rope::from_str(text)));
                        self.cursor = CharIndex(self.cursor.0.saturating_add(text.chars().count()));
                        return;
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
        self.segments.push(Segment::Text(Rope::from_str(text)));
        self.cursor = CharIndex(self.cursor.0.saturating_add(text.chars().count()));
    }

    fn insert_atom_at_cursor(&mut self, atom: Segment) -> Result<(), ComposerEditError> {
        self.segments
            .try_reserve(2)
            .map_err(|_| ComposerEditError::AllocationFailed)?;
        let mut offset = 0usize;
        for index in 0..self.segments.len() {
            match &self.segments[index] {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 <= end {
                        let local = self.cursor.0.saturating_sub(offset);
                        let before = Rope::from_str(&text.slice(..local).to_string());
                        let after = Rope::from_str(&text.slice(local..).to_string());
                        self.segments.splice(
                            index..=index,
                            [Segment::Text(before), atom, Segment::Text(after)],
                        );
                        self.cursor = CharIndex(self.cursor.0.saturating_add(1));
                        return Ok(());
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    if self.cursor.0 == offset {
                        self.segments.insert(index, atom);
                        self.cursor = CharIndex(self.cursor.0.saturating_add(1));
                        return Ok(());
                    }
                    offset = offset.saturating_add(1);
                }
            }
        }
        self.segments.push(atom);
        self.cursor = CharIndex(self.cursor.0.saturating_add(1));
        Ok(())
    }

    fn replace_range_with_atom(
        &mut self,
        range: CharRange,
        atom: Segment,
    ) -> Result<(), ComposerEditError> {
        self.segments
            .try_reserve(2)
            .map_err(|_| ComposerEditError::AllocationFailed)?;
        let mut offset = 0usize;
        for index in 0..self.segments.len() {
            let Segment::Text(text) = &self.segments[index] else {
                offset = offset.saturating_add(1);
                continue;
            };
            let end = offset.saturating_add(text.len_chars());
            if range.start.0 >= offset && range.end.0 <= end {
                let local_start = range.start.0 - offset;
                let local_end = range.end.0 - offset;
                let before = Rope::from_str(&text.slice(..local_start).to_string());
                let after = Rope::from_str(&text.slice(local_end..).to_string());
                self.segments.splice(
                    index..=index,
                    [Segment::Text(before), atom, Segment::Text(after)],
                );
                self.cursor = CharIndex(range.start.0.saturating_add(1));
                return Ok(());
            }
            offset = end;
        }
        Err(ComposerEditError::StaleQuery)
    }

    fn detect_active_query(&self) -> Option<QueryView> {
        let (segment_index, local_cursor, global_start) = self.text_location_at_cursor()?;
        let Segment::Text(text) = &self.segments[segment_index] else {
            return None;
        };
        let prefix = text.slice(..local_cursor).to_string();
        let token_start_byte = prefix
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                character
                    .is_whitespace()
                    .then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        let token = &prefix[token_start_byte..];
        let (kind, query_text) = if let Some(query) = token.strip_prefix('@') {
            (QueryKind::FileReference, query)
        } else if let Some(query) = token.strip_prefix('/') {
            (QueryKind::Command, query)
        } else {
            return None;
        };
        let local_start = prefix[..token_start_byte].chars().count();
        let range = CharRange {
            start: CharIndex(global_start.saturating_add(local_start)),
            end: self.cursor,
        };
        let standalone =
            kind == QueryKind::Command && range.start.0 == 0 && range.end.0 == self.semantic_len();
        Some(QueryView {
            id: QueryId {
                revision: self.revision,
                cursor_epoch: self.cursor_epoch,
                range,
                kind,
            },
            text: query_text.to_owned(),
            standalone,
        })
    }

    fn text_location_at_cursor(&self) -> Option<(usize, usize, usize)> {
        let mut offset = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Text(text) => {
                    let end = offset.saturating_add(text.len_chars());
                    if self.cursor.0 >= offset && self.cursor.0 <= end {
                        return Some((index, self.cursor.0 - offset, offset));
                    }
                    offset = end;
                }
                Segment::FileReference(_) | Segment::PastedBlock(_) => {
                    offset = offset.saturating_add(1)
                }
            }
        }
        None
    }

    fn move_vertical(&mut self, width: usize, direction: i8) {
        let layout = self.layout(width);
        let Some(current) = layout.geometry(self.cursor) else {
            return;
        };
        let target_row = if direction < 0 {
            current.row.checked_sub(1)
        } else {
            current.row.checked_add(1)
        };
        let Some(target_row) = target_row.filter(|row| *row < layout.total_rows()) else {
            return;
        };
        let preferred = self.preferred_column.unwrap_or(current.column);
        let Some(cursor) = layout.closest_cursor(target_row, preferred) else {
            return;
        };
        let old = self.cursor;
        self.cursor = cursor;
        if self.cursor != old {
            self.cursor_epoch = self.cursor_epoch.wrapping_add(1);
            *self.query_cache.borrow_mut() = None;
        }
        self.preferred_column = Some(preferred);
    }

    fn finish_cursor_move(&mut self, old: CharIndex) {
        if self.cursor != old {
            self.cursor_epoch = self.cursor_epoch.wrapping_add(1);
            *self.query_cache.borrow_mut() = None;
        }
        self.preferred_column = None;
    }

    fn finish_edit(&mut self) {
        self.normalize_segments();
        self.revision.0 = self.revision.0.wrapping_add(1);
        self.cursor_epoch = self.cursor_epoch.wrapping_add(1);
        self.preferred_column = None;
        *self.query_cache.borrow_mut() = None;
        debug_assert!(self.cursor.0 <= self.semantic_len());
        debug_assert!(
            self.segments
                .windows(2)
                .all(|pair| !matches!((&pair[0], &pair[1]), (Segment::Text(_), Segment::Text(_))))
        );
    }

    fn normalize_segments(&mut self) {
        let mut normalized: Vec<Segment> = Vec::with_capacity(self.segments.len());
        for segment in std::mem::take(&mut self.segments) {
            match segment {
                Segment::Text(text) if text.len_chars() == 0 => {}
                Segment::Text(text) => {
                    if let Some(Segment::Text(previous)) = normalized.last_mut() {
                        for chunk in text.chunks() {
                            previous.insert(previous.len_chars(), chunk);
                        }
                    } else {
                        normalized.push(Segment::Text(text));
                    }
                }
                atom => normalized.push(atom),
            }
        }
        if normalized.is_empty() {
            normalized.push(Segment::Text(Rope::new()));
        }
        self.segments = normalized;
    }

    #[cfg(test)]
    fn layout_build_count(&self) -> usize {
        self.layout_builds.get()
    }
}

fn build_layout(segments: &[Segment], width: usize) -> ComposerLayout {
    let sources = split_logical_lines(segments);
    let lines = sources
        .iter()
        .map(|source| Arc::new(build_logical_line(source, width)))
        .collect();
    ComposerLayout::from_lines(width, lines)
}

fn rebuild_layout(
    width: usize,
    sources: &[LogicalLineSource],
    previous: Option<&LayoutCacheEntry>,
) -> (Arc<ComposerLayout>, usize) {
    let Some(previous) = previous else {
        let lines = sources
            .iter()
            .map(|source| Arc::new(build_logical_line(source, width)))
            .collect::<Vec<_>>();
        let builds = lines.len();
        return (Arc::new(ComposerLayout::from_lines(width, lines)), builds);
    };

    let prefix = previous
        .sources
        .iter()
        .zip(sources)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix_limit = previous
        .sources
        .len()
        .saturating_sub(prefix)
        .min(sources.len().saturating_sub(prefix));
    let suffix = previous
        .sources
        .iter()
        .rev()
        .zip(sources.iter().rev())
        .take(suffix_limit)
        .take_while(|(left, right)| left == right)
        .count();

    let changed_end = sources.len().saturating_sub(suffix);
    let mut lines = Vec::with_capacity(sources.len());
    lines.extend(previous.layout.lines[..prefix].iter().cloned());
    lines.extend(
        sources[prefix..changed_end]
            .iter()
            .map(|source| Arc::new(build_logical_line(source, width))),
    );
    if suffix > 0 {
        let old_suffix = previous.layout.lines.len().saturating_sub(suffix);
        lines.extend(previous.layout.lines[old_suffix..].iter().cloned());
    }
    (
        Arc::new(ComposerLayout::from_lines(width, lines)),
        changed_end.saturating_sub(prefix),
    )
}

fn split_logical_lines(segments: &[Segment]) -> Vec<LogicalLineSource> {
    let mut lines = vec![LogicalLineSource::default()];
    for segment in segments {
        match segment {
            Segment::Text(text) => {
                for line_index in 0..text.len_lines() {
                    let start = text.line_to_char(line_index);
                    let end = if line_index + 1 < text.len_lines() {
                        text.line_to_char(line_index + 1)
                    } else {
                        text.len_chars()
                    };
                    if start < end {
                        lines
                            .last_mut()
                            .expect("logical line table always has one line")
                            .segments
                            .push(LogicalLineSegment::Text {
                                rope: text.clone(),
                                range: start..end,
                            });
                    }
                    if start < end && text.char(end - 1) == '\n' {
                        lines.push(LogicalLineSource::default());
                    }
                }
            }
            Segment::FileReference(path) => lines
                .last_mut()
                .expect("logical line table always has one line")
                .segments
                .push(LogicalLineSegment::FileReference(path.clone())),
            Segment::PastedBlock(block) => lines
                .last_mut()
                .expect("logical line table always has one line")
                .segments
                .push(LogicalLineSegment::PastedBlock(block.clone())),
        }
    }
    lines
}

fn build_logical_line(source: &LogicalLineSource, width: usize) -> LogicalLineLayout {
    let mut builder = LayoutBuilder::new(width);
    let mut semantic_cursor = CharIndex(0);
    builder.add_anchor(semantic_cursor);
    for segment in &source.segments {
        match segment {
            LogicalLineSegment::Text { rope, range } => {
                layout_rope_text(&mut builder, rope.slice(range.clone()), semantic_cursor);
                semantic_cursor.0 = semantic_cursor.0.saturating_add(range.len());
            }
            LogicalLineSegment::FileReference(path) => {
                let after = CharIndex(semantic_cursor.0.saturating_add(1));
                builder.push_atom(
                    &format!("@{}", path.display()),
                    DisplayRunKind::FileReference,
                    semantic_cursor,
                    after,
                );
                semantic_cursor = after;
            }
            LogicalLineSegment::PastedBlock(block) => {
                let after = CharIndex(semantic_cursor.0.saturating_add(1));
                builder.push_atom(
                    &block.summary(),
                    DisplayRunKind::PastedBlock,
                    semantic_cursor,
                    after,
                );
                semantic_cursor = after;
            }
        }
    }
    builder.add_anchor(semantic_cursor);
    builder.finish(semantic_cursor.0)
}

fn layout_rope_text(builder: &mut LayoutBuilder, text: RopeSlice<'_>, base: CharIndex) {
    if text
        .chunks()
        .all(|chunk| chunk.bytes().all(is_fast_ascii_display_byte))
    {
        layout_ascii_text(builder, text, base);
        return;
    }

    let mut graphemes = RopeGraphemes::new(text).peekable();
    let mut buffered = Vec::new();
    while let Some(next) = graphemes.peek() {
        if next.text == "\n" {
            let grapheme = graphemes.next().unwrap();
            push_layout_grapheme(builder, base, &grapheme);
            continue;
        }

        let mut seen_non_whitespace = false;
        let mut direct = builder.column == 0;
        let mut projected_column = builder.column;
        buffered.clear();
        while let Some(next) = graphemes.peek() {
            if next.text == "\n" {
                break;
            }
            let whitespace = next.text.chars().all(char::is_whitespace);
            if seen_non_whitespace && whitespace {
                break;
            }
            if !whitespace {
                seen_non_whitespace = true;
            }
            let grapheme = graphemes.next().unwrap();
            if direct {
                push_layout_grapheme(builder, base, &grapheme);
                continue;
            }

            projected_column = projected_column
                .saturating_add(rendered_width(grapheme.text.as_ref(), projected_column));
            buffered.push(grapheme);
            if projected_column > builder.width {
                builder.start_wrapped_group();
                for grapheme in buffered.drain(..) {
                    push_layout_grapheme(builder, base, &grapheme);
                }
                direct = true;
            }
        }
        for grapheme in buffered.drain(..) {
            push_layout_grapheme(builder, base, &grapheme);
        }
    }
}

fn is_fast_ascii_display_byte(byte: u8) -> bool {
    byte == b'\n' || byte == b'\t' || (b' '..=b'~').contains(&byte)
}

fn layout_ascii_text(builder: &mut LayoutBuilder, text: RopeSlice<'_>, base: CharIndex) {
    let mut bytes = text.bytes().enumerate().peekable();
    while let Some((_, byte)) = bytes.peek().copied() {
        if byte == b'\n' {
            let (index, byte) = bytes.next().unwrap();
            builder.push_ascii_text_byte(byte, base, index);
            continue;
        }

        let mut seen_non_whitespace = false;
        let mut direct = builder.column == 0;
        let mut projected_column = builder.column;
        let mut buffered = Vec::new();
        while let Some((_, byte)) = bytes.peek().copied() {
            if byte == b'\n' {
                break;
            }
            let whitespace = byte == b' ' || byte == b'\t';
            if seen_non_whitespace && whitespace {
                break;
            }
            if !whitespace {
                seen_non_whitespace = true;
            }
            let (index, byte) = bytes.next().unwrap();
            if direct {
                builder.push_ascii_text_byte(byte, base, index);
                continue;
            }

            projected_column = projected_column.saturating_add(if byte == b'\t' {
                TAB_WIDTH - projected_column % TAB_WIDTH
            } else {
                1
            });
            buffered.push((index, byte));
            if projected_column > builder.width {
                builder.start_wrapped_group();
                for (index, byte) in buffered.drain(..) {
                    builder.push_ascii_text_byte(byte, base, index);
                }
                direct = true;
            }
        }
        for (index, byte) in buffered {
            builder.push_ascii_text_byte(byte, base, index);
        }
    }
}

fn push_layout_grapheme(builder: &mut LayoutBuilder, base: CharIndex, grapheme: &RopeGrapheme<'_>) {
    builder.push_text_grapheme(
        grapheme.text.as_ref(),
        DisplayRunKind::Text,
        CharIndex(base.0.saturating_add(grapheme.char_start)),
        CharIndex(base.0.saturating_add(grapheme.char_end)),
    );
}

fn rendered_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        TAB_WIDTH - column % TAB_WIDTH
    } else {
        let safe = safe_grapheme(grapheme);
        UnicodeWidthStr::width(safe.as_ref()).max(1)
    }
}

struct LayoutBuilder {
    width: usize,
    display: String,
    spans: Vec<DisplaySpan>,
    row_starts: Vec<usize>,
    anchors: Vec<CursorAnchor>,
    row: usize,
    column: usize,
    soft_boundary: bool,
}

impl LayoutBuilder {
    fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            display: String::new(),
            spans: Vec::new(),
            row_starts: vec![0],
            anchors: Vec::new(),
            row: 0,
            column: 0,
            soft_boundary: false,
        }
    }

    fn push_text_grapheme(
        &mut self,
        grapheme: &str,
        kind: DisplayRunKind,
        before: CharIndex,
        after: CharIndex,
    ) {
        if grapheme == "\n" {
            self.add_anchor(before);
            if self.soft_boundary {
                self.soft_boundary = false;
            } else {
                self.row = self.row.saturating_add(1);
                self.column = 0;
            }
            self.ensure_row();
            self.add_anchor(after);
            return;
        }
        if grapheme == "\t" {
            let spaces = TAB_WIDTH - self.column % TAB_WIDTH;
            self.push_rendered_sequence(&" ".repeat(spaces), kind, before, after);
            return;
        }
        let safe = safe_grapheme(grapheme);
        self.push_rendered_sequence(safe.as_ref(), kind, before, after);
    }

    fn push_ascii_text_byte(&mut self, byte: u8, base: CharIndex, local_index: usize) {
        let before = CharIndex(base.0.saturating_add(local_index));
        let after = CharIndex(before.0.saturating_add(1));
        if byte == b'\n' {
            self.add_anchor(before);
            if self.soft_boundary {
                self.soft_boundary = false;
            } else {
                self.row = self.row.saturating_add(1);
                self.column = 0;
            }
            self.ensure_row();
            self.add_anchor(after);
            return;
        }
        if byte == b'\t' {
            let spaces = TAB_WIDTH - self.column % TAB_WIDTH;
            self.add_anchor(before);
            for _ in 0..spaces {
                self.push_ascii_piece(b' ', DisplayRunKind::Text);
            }
            self.add_anchor(after);
            return;
        }
        self.add_anchor(before);
        self.push_ascii_piece(byte, DisplayRunKind::Text);
        self.add_anchor(after);
    }

    fn push_ascii_piece(&mut self, byte: u8, kind: DisplayRunKind) {
        if self.soft_boundary {
            self.ensure_row();
            self.soft_boundary = false;
        }
        let start = self.display.len();
        self.display.push(char::from(byte));
        self.extend_span(kind, start);
        let next = self.column.saturating_add(1);
        if next >= self.width {
            self.row = self.row.saturating_add(next / self.width);
            self.column = next % self.width;
            self.soft_boundary = self.column == 0;
            self.ensure_row();
        } else {
            self.column = next;
        }
    }

    fn push_atom(
        &mut self,
        display: &str,
        kind: DisplayRunKind,
        before: CharIndex,
        after: CharIndex,
    ) {
        self.prepare_text_group(display.graphemes(true));
        self.push_rendered_sequence(display, kind, before, after);
    }

    fn start_wrapped_group(&mut self) {
        self.row = self.row.saturating_add(1);
        self.column = 0;
        self.soft_boundary = false;
        self.ensure_row();
    }

    fn prepare_text_group<'a>(&mut self, graphemes: impl IntoIterator<Item = &'a str>) {
        if self.column == 0 {
            return;
        }
        let mut column = self.column;
        for grapheme in graphemes {
            if grapheme == "\t" {
                column = column.saturating_add(TAB_WIDTH - column % TAB_WIDTH);
            } else {
                let safe = safe_grapheme(grapheme);
                column = column.saturating_add(UnicodeWidthStr::width(safe.as_ref()).max(1));
            }
        }
        if column > self.width {
            self.row = self.row.saturating_add(1);
            self.column = 0;
            self.soft_boundary = false;
            self.ensure_row();
        }
    }

    fn push_rendered_sequence(
        &mut self,
        display: &str,
        kind: DisplayRunKind,
        before: CharIndex,
        after: CharIndex,
    ) {
        if display.is_empty() {
            self.add_anchor(before);
            self.add_anchor(after);
            return;
        }
        self.add_anchor(before);
        for grapheme in display.graphemes(true) {
            self.push_piece(grapheme, kind);
        }
        self.add_anchor(after);
    }

    fn push_piece(&mut self, piece: &str, kind: DisplayRunKind) {
        if self.soft_boundary {
            self.ensure_row();
            self.soft_boundary = false;
        }
        let piece_width = UnicodeWidthStr::width(piece).max(1);
        if self.column > 0 && self.column.saturating_add(piece_width) > self.width {
            self.row = self.row.saturating_add(1);
            self.column = 0;
            self.ensure_row();
        }
        let start = self.display.len();
        self.display.push_str(piece);
        self.extend_span(kind, start);
        let next = self.column.saturating_add(piece_width);
        if next >= self.width {
            self.row = self.row.saturating_add(next / self.width);
            self.column = next % self.width;
            self.soft_boundary = self.column == 0;
            self.ensure_row();
        } else {
            self.column = next;
        }
    }

    fn add_anchor(&mut self, cursor: CharIndex) {
        self.ensure_row();
        let anchor = CursorAnchor {
            cursor,
            geometry: CursorGeometry {
                row: self.row,
                column: self.column,
            },
        };
        if let Some(last) = self.anchors.last_mut()
            && last.cursor == cursor
        {
            *last = anchor;
        } else {
            self.anchors.push(anchor);
        }
    }

    fn ensure_row(&mut self) {
        while self.row_starts.len() <= self.row {
            self.row_starts.push(self.display.len());
        }
    }

    fn extend_span(&mut self, kind: DisplayRunKind, start: usize) {
        let end = self.display.len();
        if let Some(last) = self.spans.last_mut()
            && last.kind == kind
            && last.end == start
        {
            last.end = end;
        } else {
            self.spans.push(DisplaySpan { kind, start, end });
        }
    }

    fn finish(mut self, semantic_len: usize) -> LogicalLineLayout {
        if self.row_starts.is_empty() {
            self.row_starts.push(0);
        }
        let row_ranges = self
            .row_starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = self
                    .row_starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(self.display.len());
                *start..end
            })
            .collect::<Vec<_>>();
        let mut ranges = vec![0..0; row_ranges.len()];
        let mut start = 0usize;
        for (row, range) in ranges.iter_mut().enumerate() {
            while start < self.anchors.len() && self.anchors[start].geometry.row < row {
                start += 1;
            }
            let mut end = start;
            while end < self.anchors.len() && self.anchors[end].geometry.row == row {
                end += 1;
            }
            *range = start..end;
            start = end;
        }
        LogicalLineLayout {
            semantic_len,
            display: self.display,
            spans: self.spans,
            row_ranges,
            anchors: self.anchors,
            row_anchor_ranges: ranges,
        }
    }
}

fn safe_grapheme(grapheme: &str) -> Cow<'_, str> {
    if !grapheme
        .chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        return Cow::Borrowed(grapheme);
    }
    let mut safe = String::new();
    for character in grapheme.chars() {
        match character {
            '\n' | '\t' => safe.push(character),
            '\u{0}'..='\u{1f}' => safe.push(char::from_u32(0x2400 + character as u32).unwrap()),
            '\u{7f}' => safe.push('\u{2421}'),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(safe, "\\u{{{:X}}}", character as u32);
            }
            character => safe.push(character),
        }
    }
    Cow::Owned(safe)
}

fn push_safe_text(output: &mut String, text: &str, column: &mut usize) {
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            output.push('\n');
            *column = 0;
        } else if grapheme == "\t" {
            let spaces = TAB_WIDTH - *column % TAB_WIDTH;
            output.push_str(&" ".repeat(spaces));
            *column = column.saturating_add(spaces);
        } else {
            let safe = safe_grapheme(grapheme);
            output.push_str(safe.as_ref());
            *column = column.saturating_add(UnicodeWidthStr::width(safe.as_ref()));
        }
    }
}

pub(crate) fn safe_single_line(text: &str, initial_column: usize) -> String {
    let mut output = String::new();
    let mut column = initial_column;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            output.push_str("\\n");
            column = column.saturating_add(2);
        } else if grapheme == "\t" {
            let spaces = TAB_WIDTH - column % TAB_WIDTH;
            output.push_str(&" ".repeat(spaces));
            column = column.saturating_add(spaces);
        } else {
            let safe = safe_grapheme(grapheme);
            output.push_str(safe.as_ref());
            column = column.saturating_add(UnicodeWidthStr::width(safe.as_ref()));
        }
    }
    output
}

fn normalized_len(raw: &str) -> Result<usize, ComposerEditError> {
    let mut bytes = 0usize;
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            bytes = bytes
                .checked_add(1)
                .ok_or(ComposerEditError::ProjectionOverflow)?;
        } else {
            bytes = bytes
                .checked_add(character.len_utf8())
                .ok_or(ComposerEditError::ProjectionOverflow)?;
        }
    }
    Ok(bytes)
}

fn normalize_paste(raw: &str) -> Result<Arc<str>, ComposerEditError> {
    if !raw.contains('\r') {
        return Ok(Arc::from(raw));
    }
    let capacity = normalized_len(raw)?;
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(capacity)
        .map_err(|_| ComposerEditError::AllocationFailed)?;
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(character);
        }
    }
    Ok(Arc::from(normalized))
}

struct RopeGrapheme<'a> {
    char_start: usize,
    char_end: usize,
    text: Cow<'a, str>,
}

struct RopeGraphemes<'a> {
    rope: RopeSlice<'a>,
    chunks: Vec<(&'a str, usize)>,
    cursor: GraphemeCursor,
    chunk_index: usize,
    text_chunk_index: usize,
    byte_start: usize,
    char_start: usize,
    finished: bool,
}

impl<'a> RopeGraphemes<'a> {
    fn new(rope: RopeSlice<'a>) -> Self {
        Self {
            chunks: rope_chunks(rope),
            cursor: GraphemeCursor::new(0, rope.len_bytes(), true),
            rope,
            chunk_index: 0,
            text_chunk_index: 0,
            byte_start: 0,
            char_start: 0,
            finished: rope.len_bytes() == 0,
        }
    }
}

impl<'a> Iterator for RopeGraphemes<'a> {
    type Item = RopeGrapheme<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            let (chunk, chunk_start) = self.chunks[self.chunk_index];
            match self.cursor.next_boundary(chunk, chunk_start) {
                Ok(Some(byte_end)) => {
                    if byte_end == self.byte_start {
                        continue;
                    }
                    let byte_start = self.byte_start;
                    self.byte_start = byte_end;
                    while self.text_chunk_index + 1 < self.chunks.len() {
                        let (chunk, start) = self.chunks[self.text_chunk_index];
                        if byte_start < start.saturating_add(chunk.len()) {
                            break;
                        }
                        self.text_chunk_index += 1;
                    }
                    let (text_chunk, text_chunk_start) = self.chunks[self.text_chunk_index];
                    let text_chunk_end = text_chunk_start.saturating_add(text_chunk.len());
                    let text = if byte_start >= text_chunk_start && byte_end <= text_chunk_end {
                        Cow::Borrowed(
                            &text_chunk[byte_start.saturating_sub(text_chunk_start)
                                ..byte_end.saturating_sub(text_chunk_start)],
                        )
                    } else {
                        Cow::Owned(self.rope.byte_slice(byte_start..byte_end).to_string())
                    };
                    let char_start = self.char_start;
                    let char_end = char_start.saturating_add(text.chars().count());
                    self.char_start = char_end;
                    return Some(RopeGrapheme {
                        char_start,
                        char_end,
                        text,
                    });
                }
                Ok(None) => self.finished = true,
                Err(GraphemeIncomplete::NextChunk) => {
                    self.chunk_index = self
                        .chunk_index
                        .saturating_add(1)
                        .min(self.chunks.len() - 1)
                }
                Err(GraphemeIncomplete::PrevChunk) => {
                    self.chunk_index = self.chunk_index.saturating_sub(1)
                }
                Err(GraphemeIncomplete::PreContext(offset)) => {
                    let context = self
                        .chunks
                        .iter()
                        .rev()
                        .find(|(chunk, start)| start.saturating_add(chunk.len()) == offset)
                        .copied()
                        .expect("rope chunks cover requested grapheme context");
                    self.cursor.provide_context(context.0, context.1);
                }
                Err(GraphemeIncomplete::InvalidOffset) => {
                    unreachable!("rope chunk must contain the grapheme cursor")
                }
            }
        }
        None
    }
}

fn previous_grapheme_boundary(rope: &Rope, char_index: usize) -> usize {
    grapheme_boundary(rope, char_index, false)
}

fn next_grapheme_boundary(rope: &Rope, char_index: usize) -> usize {
    grapheme_boundary(rope, char_index, true)
}

fn grapheme_boundary(rope: &Rope, char_index: usize, forward: bool) -> usize {
    let byte_index = rope.char_to_byte(char_index);
    if (forward && byte_index == rope.len_bytes()) || (!forward && byte_index == 0) {
        return char_index;
    }
    let chunks = rope_chunks(rope.slice(..));
    let mut chunk_index = if forward {
        chunks
            .iter()
            .position(|(chunk, start)| byte_index < start.saturating_add(chunk.len()))
            .unwrap_or(chunks.len() - 1)
    } else {
        chunks
            .iter()
            .rposition(|(_, start)| *start < byte_index)
            .unwrap_or_default()
    };
    let mut cursor = GraphemeCursor::new(byte_index, rope.len_bytes(), true);
    loop {
        let (chunk, chunk_start) = chunks[chunk_index];
        let result = if forward {
            cursor.next_boundary(chunk, chunk_start)
        } else {
            cursor.prev_boundary(chunk, chunk_start)
        };
        match result {
            Ok(Some(boundary)) => return rope.byte_to_char(boundary),
            Ok(None) => return if forward { rope.len_chars() } else { 0 },
            Err(GraphemeIncomplete::NextChunk) => {
                chunk_index = chunk_index.saturating_add(1).min(chunks.len() - 1)
            }
            Err(GraphemeIncomplete::PrevChunk) => chunk_index = chunk_index.saturating_sub(1),
            Err(GraphemeIncomplete::PreContext(offset)) => {
                let context = chunks
                    .iter()
                    .rev()
                    .find(|(chunk, start)| start.saturating_add(chunk.len()) == offset)
                    .copied()
                    .expect("rope chunks cover requested grapheme context");
                cursor.provide_context(context.0, context.1);
            }
            Err(GraphemeIncomplete::InvalidOffset) => {
                unreachable!("rope chunk must contain the grapheme cursor")
            }
        }
    }
}

fn rope_chunks(rope: RopeSlice<'_>) -> Vec<(&str, usize)> {
    let mut start = 0usize;
    rope.chunks()
        .map(|chunk| {
            let current = start;
            start = start.saturating_add(chunk.len());
            (chunk, current)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ComposerDocument, ComposerEditError, DisplayRunKind, QueryKind, REQUEST_HARD_LIMIT_BYTES,
        SubmittedContent,
    };
    use crate::workspace::WorkspacePath;
    use proptest::prelude::*;

    fn complete_file(document: &mut ComposerDocument, path: &str) {
        let query = document.active_query().unwrap();
        assert_eq!(query.kind(), QueryKind::FileReference);
        document
            .complete_file_reference(query.id(), WorkspacePath::from_raw(path))
            .unwrap();
    }

    #[test]
    fn stale_query_id_with_identical_text_cannot_complete_a_new_query() {
        let mut document = ComposerDocument::default();
        document.insert_text("@src");
        let stale_query = document.active_query().unwrap().id();

        document.clear();
        document.insert_text("@src");
        let before = document.freeze();
        let before_revision = document.revision();

        assert_eq!(
            document.complete_file_reference(stale_query, WorkspacePath::from_raw("src/app.rs"),),
            Err(ComposerEditError::StaleQuery)
        );
        assert_eq!(document.freeze(), before);
        assert_eq!(document.revision(), before_revision);
        assert_eq!(document.active_query().unwrap().text(), "src");
    }

    #[test]
    fn file_references_stay_between_surrounding_text() {
        let mut document = ComposerDocument::default();
        document.insert_text("inspect @src/ here");
        for _ in 0..5 {
            document.move_left();
        }
        complete_file(&mut document, "src/app.rs");

        assert_eq!(document.visible_text(), "inspect @src/app.rs here");
        assert_eq!(document.attachments()[0].path().raw(), "src/app.rs");
    }

    #[test]
    fn file_reference_deletion_is_atomic() {
        let mut document = ComposerDocument::default();
        document.insert_text("@src/a");
        complete_file(&mut document, "src/app.rs");
        document.backspace();

        assert!(document.is_empty());
        assert!(document.attachments().is_empty());
    }

    #[test]
    fn backspace_removes_a_combining_grapheme_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("e\u{301}X");
        document.move_left();
        document.backspace();

        assert_eq!(document.visible_text(), "X");
    }

    #[test]
    fn backspace_removes_a_zwj_emoji_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("👨‍👩‍👧X");
        document.move_left();
        document.backspace();

        assert_eq!(document.visible_text(), "X");
    }

    #[test]
    fn delete_removes_a_combining_grapheme_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("e\u{301}X");
        document.move_home();
        document.delete();

        assert_eq!(document.visible_text(), "X");
    }

    #[test]
    fn right_skips_a_zwj_emoji_as_one_grapheme() {
        let mut document = ComposerDocument::default();
        document.insert_text("👨‍👩‍👧X");
        document.move_home();
        document.move_right();
        document.delete();

        assert_eq!(document.visible_text(), "👨‍👩‍👧");
    }

    #[test]
    fn delete_removes_a_file_reference_at_the_cursor() {
        let mut document = ComposerDocument::default();
        document.insert_text("before @src/a after");
        for _ in 0..6 {
            document.move_left();
        }
        complete_file(&mut document, "src/app.rs");
        document.move_left();
        document.delete();

        assert_eq!(document.visible_text(), "before  after");
    }

    #[test]
    fn duplicate_file_references_are_sent_once() {
        let mut document = ComposerDocument::default();
        document.insert_text("@a");
        complete_file(&mut document, "src/app.rs");
        document.insert_text(" @a");
        complete_file(&mut document, "src/app.rs");

        assert_eq!(document.attachments().len(), 1);
    }

    #[test]
    fn email_addresses_are_not_file_queries() {
        let mut document = ComposerDocument::default();
        document.insert_text("me@example.com");

        assert!(document.active_query().is_none());
    }

    #[test]
    fn crlf_multiline_paste_is_one_atom() {
        let mut document = ComposerDocument::default();
        let proposal = document.propose_paste("alpha\r\nbeta").unwrap();
        assert_eq!(proposal.line_count(), 2);
        document.commit_paste(proposal).unwrap();

        assert_eq!(document.visible_text(), "[2 lines pasted]");
        assert_eq!(document.submission_text(), "alpha\nbeta");
        assert_eq!(document.freeze().segment_kinds(), ["paste"]);

        document.move_left();
        document.delete();
        assert!(document.is_empty());
    }

    #[test]
    fn single_line_paste_is_compact_while_editing_and_expands_after_send() {
        let mut document = ComposerDocument::default();
        let proposal = document.propose_paste("a long pasted line").unwrap();
        document.commit_paste(proposal).unwrap();

        assert_eq!(document.visible_text(), "[1 line pasted]");
        let submitted = document.freeze();
        assert_eq!(submitted.visible_text(), "a long pasted line");
        assert_eq!(
            submitted.display_lines(0)[0].runs[0].text,
            "a long pasted line"
        );
        assert_eq!(submitted.layout(8).total_rows(), 3);
    }

    #[test]
    fn multiline_paste_expands_to_its_full_text_after_send() {
        let mut document = ComposerDocument::default();
        let proposal = document.propose_paste("alpha\nbeta").unwrap();
        document.commit_paste(proposal).unwrap();

        let submitted = document.freeze();
        assert_eq!(submitted.visible_text(), "alpha\nbeta");
        assert_eq!(submitted.display_lines(0).len(), 2);
        assert_eq!(submitted.layout(80).total_rows(), 2);
    }

    #[test]
    fn stale_paste_proposal_does_not_mutate() {
        let mut document = ComposerDocument::default();
        let proposal = document.propose_paste("alpha\nbeta").unwrap();
        document.insert_text("changed");
        let before = document.freeze();

        assert_eq!(
            document.commit_paste(proposal),
            Err(ComposerEditError::StalePaste)
        );
        assert_eq!(document.freeze(), before);
    }

    #[test]
    fn oversized_paste_is_rejected_without_mutating_the_document() {
        let document = ComposerDocument::default();
        let paste = "x".repeat(REQUEST_HARD_LIMIT_BYTES + 1);

        assert!(matches!(
            document.propose_paste(&paste),
            Err(ComposerEditError::RequestTooLarge { .. })
        ));
        assert!(document.is_empty());
    }

    #[test]
    fn controls_are_safe_and_tabs_use_four_column_stops() {
        let mut document = ComposerDocument::default();
        document.insert_text("a\t\u{7}界");
        let layout = document.layout(20);
        let visible = &layout.visible_rows(0, 1)[0].runs;

        assert_eq!(visible[0].kind, DisplayRunKind::Text);
        assert_eq!(visible[0].text, "a   ␇界");
        assert_eq!(document.submission_text(), "a\t\u{7}界");
        assert_eq!(document.cursor_geometry(&layout).column, 7);
    }

    #[test]
    fn layout_is_cached_by_revision_and_width() {
        let mut document = ComposerDocument::default();
        document.insert_text("hello");

        let _ = document.layout(10);
        let _ = document.layout(10);
        assert_eq!(document.layout_build_count(), 1);
        let _ = document.layout(20);
        assert_eq!(document.layout_build_count(), 2);

        document.insert_text("!");
        let _ = document.layout(10);
        assert_eq!(document.layout_build_count(), 3);
    }

    #[test]
    fn editing_one_logical_line_reuses_unchanged_wrapping() {
        let mut document = ComposerDocument::default();
        document.insert_text("first\nmiddle\nlast");
        let _ = document.layout(20);
        assert_eq!(document.layout_build_count(), 3);

        document.cursor = super::CharIndex(8);
        document.insert_text("X");
        let _ = document.layout(20);

        assert_eq!(document.layout_build_count(), 4);
        assert_eq!(document.submission_text(), "first\nmiXddle\nlast");
    }

    #[test]
    fn splitting_and_joining_a_line_only_rebuilds_the_replaced_lines() {
        let mut document = ComposerDocument::default();
        document.insert_text("first\nmiddle\nlast");
        let _ = document.layout(20);
        assert_eq!(document.layout_build_count(), 3);

        document.cursor = super::CharIndex(8);
        document.insert_text("\n");
        let _ = document.layout(20);
        assert_eq!(document.layout_build_count(), 5);

        document.backspace();
        let _ = document.layout(20);
        assert_eq!(document.layout_build_count(), 6);
    }

    #[test]
    fn viewport_materializes_only_the_requested_rows_and_resize_cache_is_bounded() {
        let mut document = ComposerDocument::default();
        document.insert_text(&"x".repeat(1024 * 1024));
        let layout = document.layout(80);
        assert_eq!(layout.visible_rows(layout.total_rows() - 24, 24).len(), 24);

        let mut resized = ComposerDocument::default();
        resized.insert_text("resize me");
        for width in 81..181 {
            let _ = resized.layout(width);
        }
        let cache = resized.layout_cache.borrow();
        assert_eq!(cache.as_ref().map(|entry| entry.width), Some(180));
    }

    #[test]
    fn long_short_long_vertical_navigation_restores_the_preferred_column() {
        let mut document = ComposerDocument::default();
        document.insert_text("abcdefghij\nab\nabcdefghij");

        document.move_up(20);
        document.move_up(20);
        document.insert_text("X");

        assert_eq!(document.submission_text(), "abcdefghijX\nab\nabcdefghij");
    }

    #[test]
    fn exact_width_newline_uses_the_post_newline_anchor() {
        let mut document = ComposerDocument::default();
        document.insert_text("abcd\nx");
        document.move_left();

        document.move_up(4);
        document.move_down(4);
        document.insert_text("X");

        assert_eq!(document.submission_text(), "abcd\nXx");
    }

    #[test]
    fn pointer_position_moves_the_cursor_to_the_closest_visual_column() {
        let mut document = ComposerDocument::default();
        document.insert_text("abcdef");

        document.move_to_visual_position(4, 2, 1, 1);
        document.insert_text("X");

        assert_eq!(document.submission_text(), "abcdeXf");
    }

    #[test]
    fn seventy_thousand_hard_rows_keep_the_tail_reachable() {
        let mut document = ComposerDocument::default();
        let text = format!("{}TAIL", "x\n".repeat(70_000));
        document.insert_text(&text);
        let layout = document.layout(80);
        let cursor = document.cursor_geometry(&layout);

        assert!(cursor.row >= 70_000);
        assert!(
            layout.visible_rows(cursor.row, 1)[0]
                .runs
                .iter()
                .any(|run| run.text.contains("TAIL"))
        );

        document.move_up(80);
        document.move_down(80);
        document.insert_text("!");
        assert!(document.submission_text().ends_with("TAIL!"));
    }

    #[test]
    fn more_than_u16_wrapped_rows_preserve_reversible_navigation() {
        let mut document = ComposerDocument::default();
        document.insert_text(&"x".repeat(70_000));
        let layout = document.layout(1);
        let cursor = document.cursor_geometry(&layout);

        assert!(cursor.row > u16::MAX as usize);
        document.move_up(1);
        document.move_down(1);
        document.insert_text("!");
        assert!(document.submission_text().ends_with('!'));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn unicode_control_edits_and_atoms_keep_projections_and_layout_valid(
            operations in prop::collection::vec((0u8..10, any::<char>()), 0..120),
            width in 1usize..40,
        ) {
            let mut document = ComposerDocument::default();
            for (operation, character) in operations {
                match operation {
                    0 | 1 => document.insert_text(&character.to_string()),
                    2 => document.move_left(),
                    3 => document.move_right(),
                    4 => document.backspace(),
                    5 => document.delete(),
                    6 => document.move_up(width),
                    7 => document.move_down(width),
                    8 => {
                        let raw = format!("p\n{character}");
                        if let Ok(proposal) = document.propose_paste(&raw) {
                            let _ = document.commit_paste(proposal);
                        }
                    }
                    _ => {
                        document.insert_text(" @q");
                        if let Some(query) = document.active_query()
                            && query.kind() == QueryKind::FileReference
                        {
                            let _ = document.complete_file_reference(
                                query.id(),
                                WorkspacePath::from_raw(format!("src/{:X}.rs", character as u32)),
                            );
                        }
                    }
                }

                let layout = document.layout(width);
                let cursor = document.cursor_geometry(&layout);
                prop_assert!(cursor.row < layout.total_rows());
                prop_assert!(cursor.column <= width);
                prop_assert_eq!(document.freeze().submission_text(), document.submission_text());
                let submitted_visible = document.freeze().visible_text();
                let plain_visible =
                    SubmittedContent::plain(document.submission_text()).visible_text();
                prop_assert_eq!(submitted_visible, plain_visible);
                let kinds = document.freeze().segment_kinds();
                prop_assert!(!kinds.windows(2).any(|pair| pair == ["text", "text"]));
            }
        }
    }
}
