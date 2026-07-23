use crate::theme::{Theme, ThemeRole};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use std::{ops::Range, str::FromStr, sync::OnceLock};
use syntect::{
    easy::ScopeRegionIterator,
    highlighting::ScopeSelectors,
    parsing::{ParseState, ScopeStack, SyntaxSet},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticSpan {
    text: String,
    role: ThemeRole,
    modifiers: Modifier,
}

impl SemanticSpan {
    fn new(text: impl Into<String>, role: ThemeRole) -> Self {
        Self {
            text: text.into(),
            role,
            modifiers: Modifier::empty(),
        }
    }

    fn with_modifiers(mut self, modifiers: Modifier) -> Self {
        self.modifiers = modifiers;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SemanticLine {
    spans: Vec<SemanticSpan>,
    anchor: usize,
}

impl SemanticLine {
    fn push(&mut self, span: SemanticSpan) {
        if span.text.is_empty() {
            return;
        }
        if let Some(previous) = self.spans.last_mut()
            && previous.role == span.role
            && previous.modifiers == span.modifiers
        {
            previous.text.push_str(&span.text);
        } else {
            self.spans.push(span);
        }
    }
}

#[derive(Debug, Clone)]
struct LogicalLine {
    first_prefix: Vec<SemanticSpan>,
    continuation_prefix: Vec<SemanticSpan>,
    content: Vec<SemanticSpan>,
}

#[derive(Debug, Clone)]
pub(super) struct MarkdownLayout {
    rows: Vec<SemanticLine>,
    literal: Option<LiteralProjection>,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarkdownAnchor {
    offset: usize,
    token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiteralProjection {
    source: String,
    width: usize,
    lines: Vec<LiteralLogicalLine>,
    height: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiteralLogicalLine {
    start: usize,
    end: usize,
    first_row: usize,
    rows: usize,
    last_column: usize,
    checkpoints: Vec<LiteralCheckpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiteralCheckpoint {
    row: usize,
    offset: usize,
}

const LITERAL_CHECKPOINT_ROWS: usize = 256;

impl LiteralProjection {
    fn new(source: &str, width: usize) -> Self {
        Self::from_owned(source.to_owned(), width)
    }

    fn from_owned(mut source: String, width: usize) -> Self {
        if source.contains('\t') {
            source = source.replace('\t', "    ");
        }
        let mut projection = Self {
            source,
            width: width.max(1),
            lines: Vec::new(),
            height: 0,
        };
        projection.reindex_from(0);
        projection
    }

    fn append(&mut self, suffix: &str) {
        let suffix = suffix.replace('\t', "    ");
        let source_start = self.source.len();
        self.source.push_str(&suffix);
        let mut consumed = 0usize;
        for segment in suffix.split_inclusive('\n') {
            let body = segment.strip_suffix('\n').unwrap_or(segment);
            let end = source_start
                .saturating_add(consumed)
                .saturating_add(body.len());
            if let Some(line) = self.lines.last_mut() {
                let previous_rows = line.rows;
                extend_literal_checkpoints(
                    &mut line.checkpoints,
                    previous_rows,
                    line.last_column,
                    body,
                    self.width,
                    line.end.saturating_sub(line.start),
                );
                (line.rows, line.last_column) =
                    advance_literal_metrics(line.rows, line.last_column, body, self.width);
                line.end = end;
                self.height = self
                    .height
                    .saturating_add(line.rows.saturating_sub(previous_rows));
            }
            consumed = consumed.saturating_add(segment.len());
            if segment.ends_with('\n') {
                self.lines.push(LiteralLogicalLine {
                    start: source_start.saturating_add(consumed),
                    end: source_start.saturating_add(consumed),
                    first_row: self.height,
                    rows: 1,
                    last_column: 0,
                    checkpoints: vec![LiteralCheckpoint { row: 0, offset: 0 }],
                });
                self.height = self.height.saturating_add(1);
            }
        }
    }

    fn reindex_from(&mut self, start: usize) {
        let mut line_start = start;
        while line_start <= self.source.len() {
            let newline = self.source[line_start..]
                .find('\n')
                .map(|relative| line_start.saturating_add(relative));
            let end = newline.unwrap_or(self.source.len());
            let (rows, last_column) =
                advance_literal_metrics(1, 0, &self.source[line_start..end], self.width);
            let checkpoints = literal_checkpoints(&self.source[line_start..end], self.width);
            self.lines.push(LiteralLogicalLine {
                start: line_start,
                end,
                first_row: self.height,
                rows,
                last_column,
                checkpoints,
            });
            self.height = self.height.saturating_add(rows);
            let Some(newline) = newline else {
                break;
            };
            line_start = newline.saturating_add(1);
            if line_start == self.source.len() {
                self.lines.push(LiteralLogicalLine {
                    start: line_start,
                    end: line_start,
                    first_row: self.height,
                    rows: 1,
                    last_column: 0,
                    checkpoints: vec![LiteralCheckpoint { row: 0, offset: 0 }],
                });
                self.height = self.height.saturating_add(1);
                break;
            }
        }
    }

    fn line(&self, row: usize) -> Option<String> {
        let line = self.lines.get(
            self.lines
                .partition_point(|line| line.first_row <= row)
                .saturating_sub(1),
        )?;
        if row >= line.first_row.saturating_add(line.rows) {
            return None;
        }
        let target = row.saturating_sub(line.first_row);
        let checkpoint = checkpoint_for_row(line, target);
        Some(
            literal_visual_row_from(
                &self.source[line.start..line.end],
                self.width,
                target,
                checkpoint,
            )
            .0,
        )
    }

    fn anchor_for_row(&self, row: usize) -> usize {
        let index = self
            .lines
            .partition_point(|line| line.first_row <= row)
            .saturating_sub(1);
        self.lines.get(index).map_or(0, |line| {
            let target = row.saturating_sub(line.first_row);
            let checkpoint = checkpoint_for_row(line, target);
            line.start.saturating_add(
                literal_visual_row_from(
                    &self.source[line.start..line.end],
                    self.width,
                    target,
                    checkpoint,
                )
                .1,
            )
        })
    }

    fn row_for_anchor(&self, anchor: usize) -> usize {
        let anchor = anchor.min(self.source.len());
        let anchor = (0..=anchor)
            .rev()
            .find(|offset| self.source.is_char_boundary(*offset))
            .unwrap_or(0);
        let index = self
            .lines
            .partition_point(|line| line.start <= anchor)
            .saturating_sub(1);
        self.lines.get(index).map_or(0, |line| {
            let relative = anchor
                .saturating_sub(line.start)
                .min(line.end.saturating_sub(line.start));
            let checkpoint = line
                .checkpoints
                .iter()
                .rev()
                .find(|checkpoint| checkpoint.offset <= relative)
                .copied()
                .unwrap_or(LiteralCheckpoint { row: 0, offset: 0 });
            let visual_width = self.source
                [line.start.saturating_add(checkpoint.offset)..line.start.saturating_add(relative)]
                .width();
            line.first_row
                .saturating_add(checkpoint.row)
                .saturating_add(visual_width / self.width)
                .min(line.first_row.saturating_add(line.rows.saturating_sub(1)))
        })
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.source.len())
            .saturating_add(
                self.lines
                    .len()
                    .saturating_mul(std::mem::size_of::<LiteralLogicalLine>()),
            )
            .saturating_add(
                self.lines
                    .iter()
                    .map(|line| {
                        line.checkpoints
                            .len()
                            .saturating_mul(std::mem::size_of::<LiteralCheckpoint>())
                    })
                    .sum::<usize>(),
            )
    }
}

fn advance_literal_metrics(
    mut rows: usize,
    mut column: usize,
    text: &str,
    width: usize,
) -> (usize, usize) {
    let width = width.max(1);
    if text.is_ascii() {
        let total = column.saturating_add(text.len());
        let wraps = if text.is_empty() {
            0
        } else {
            total.saturating_sub(1) / width
        };
        let remainder = total % width;
        return (
            rows.saturating_add(wraps),
            if total > 0 && remainder == 0 {
                width
            } else {
                remainder
            },
        );
    }
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1).min(width);
        if column > 0 && column.saturating_add(grapheme_width) > width {
            rows = rows.saturating_add(1);
            column = 0;
        }
        column = column.saturating_add(grapheme_width);
    }
    (rows.max(1), column)
}

fn checkpoint_for_row(line: &LiteralLogicalLine, target: usize) -> LiteralCheckpoint {
    line.checkpoints
        .get(
            line.checkpoints
                .partition_point(|checkpoint| checkpoint.row <= target)
                .saturating_sub(1),
        )
        .copied()
        .unwrap_or(LiteralCheckpoint { row: 0, offset: 0 })
}

fn literal_visual_row_from(
    text: &str,
    width: usize,
    target: usize,
    checkpoint: LiteralCheckpoint,
) -> (String, usize) {
    let width = width.max(1);
    if text.is_ascii() {
        let start = target.saturating_mul(width).min(text.len());
        let end = start.saturating_add(width).min(text.len());
        return (text[start..end].to_owned(), start);
    }
    let mut row = checkpoint.row;
    let mut column = 0usize;
    let mut output = String::new();
    let mut start = text.len();
    for (relative, grapheme) in text[checkpoint.offset..].grapheme_indices(true) {
        let offset = checkpoint.offset.saturating_add(relative);
        let source_width = UnicodeWidthStr::width(grapheme).max(1);
        let grapheme_width = source_width.min(width);
        if column > 0 && column.saturating_add(grapheme_width) > width {
            if row == target {
                return (output, start);
            }
            row = row.saturating_add(1);
            column = 0;
            output.clear();
            start = text.len();
        }
        if row == target {
            if start == text.len() {
                start = offset;
            }
            output.push_str(if source_width > width {
                "\u{fffd}"
            } else {
                grapheme
            });
        }
        column = column.saturating_add(grapheme_width);
    }
    (output, start.min(text.len()))
}

fn literal_checkpoints(text: &str, width: usize) -> Vec<LiteralCheckpoint> {
    let mut checkpoints = vec![LiteralCheckpoint { row: 0, offset: 0 }];
    extend_literal_checkpoints(&mut checkpoints, 1, 0, text, width, 0);
    checkpoints
}

fn extend_literal_checkpoints(
    checkpoints: &mut Vec<LiteralCheckpoint>,
    rows: usize,
    mut column: usize,
    text: &str,
    width: usize,
    base_offset: usize,
) {
    if text.is_ascii() {
        let width = width.max(1);
        let mut row = rows.saturating_sub(1);
        let mut consumed = 0usize;
        while consumed < text.len() {
            if column == width {
                row = row.saturating_add(1);
                column = 0;
                if row.is_multiple_of(LITERAL_CHECKPOINT_ROWS) {
                    checkpoints.push(LiteralCheckpoint {
                        row,
                        offset: base_offset.saturating_add(consumed),
                    });
                }
            }
            let take = (width - column).min(text.len().saturating_sub(consumed));
            column = column.saturating_add(take);
            consumed = consumed.saturating_add(take);
        }
        return;
    }
    let width = width.max(1);
    let mut row = rows.saturating_sub(1);
    for (relative, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1).min(width);
        if column > 0 && column.saturating_add(grapheme_width) > width {
            row = row.saturating_add(1);
            column = 0;
            if row.is_multiple_of(LITERAL_CHECKPOINT_ROWS) {
                checkpoints.push(LiteralCheckpoint {
                    row,
                    offset: base_offset.saturating_add(relative),
                });
            }
        }
        column = column.saturating_add(grapheme_width);
    }
}

impl MarkdownLayout {
    pub(super) fn new(source: &str, width: usize) -> Self {
        Self::build(source, width, true)
    }

    pub(super) fn foreground(source: &str, width: usize) -> Self {
        const SEMANTIC_FOREGROUND_LIMIT: usize = 4 * 1024;
        if source.len() <= SEMANTIC_FOREGROUND_LIMIT {
            Self::build(source, width, false)
        } else {
            Self::literal_owned(foreground_plain_text(source), width)
        }
    }

    fn build(source: &str, width: usize, syntax_highlighting: bool) -> Self {
        let width = width.max(1);
        let mut builder = MarkdownBuilder::new(width, syntax_highlighting);
        let protected = protect_unsupported(source);
        if let Some(unclosed) = unclosed_fence_start(&protected) {
            builder.parse(&protected[..unclosed]);
            builder.append_literal_block(&protected[unclosed..]);
        } else {
            builder.parse(&protected);
        }
        let mut rows = builder.finish();
        if rows.is_empty() {
            rows.push(SemanticLine::default());
        }
        let bytes = source
            .len()
            .saturating_add(
                rows.len()
                    .saturating_mul(std::mem::size_of::<SemanticLine>()),
            )
            .saturating_add(
                rows.iter()
                    .flat_map(|line| &line.spans)
                    .map(|span| std::mem::size_of::<SemanticSpan>().saturating_add(span.text.len()))
                    .sum::<usize>(),
            );
        Self {
            rows,
            literal: None,
            bytes,
        }
    }

    pub(super) fn height(&self) -> usize {
        self.literal
            .as_ref()
            .map_or(self.rows.len(), |literal| literal.height)
    }

    pub(super) fn literal(source: &str, width: usize) -> Self {
        let literal = LiteralProjection::new(source, width);
        let bytes = literal.bytes();
        Self {
            rows: Vec::new(),
            literal: Some(literal),
            bytes,
        }
    }

    fn literal_owned(source: String, width: usize) -> Self {
        let literal = LiteralProjection::from_owned(source, width);
        let bytes = literal.bytes();
        Self {
            rows: Vec::new(),
            literal: Some(literal),
            bytes,
        }
    }

    pub(super) fn append_literal(&mut self, suffix: &str, width: usize) {
        if let Some(literal) = &mut self.literal
            && literal.width == width.max(1)
        {
            literal.append(suffix);
            self.bytes = literal.bytes();
            return;
        }
        if let Some(literal) = self.literal.take() {
            let mut source = literal.source;
            source.push_str(suffix);
            *self = Self::literal(&source, width);
            return;
        }
        let width = width.max(1);
        let previous_rows = self.rows.len();
        let previous_spans = self.rows.iter().map(|row| row.spans.len()).sum::<usize>();
        let mut column = self
            .rows
            .last()
            .map(|line| spans_width(&line.spans))
            .unwrap_or_default();
        if suffix.is_ascii() {
            let mut pending = String::new();
            for byte in suffix.bytes() {
                if byte == b'\n' {
                    self.rows
                        .last_mut()
                        .expect("literal layout always has a row")
                        .push(SemanticSpan::new(
                            std::mem::take(&mut pending),
                            ThemeRole::Text,
                        ));
                    let anchor = self
                        .rows
                        .last()
                        .map_or(0, |line| line.anchor.saturating_add(1));
                    self.rows.push(SemanticLine {
                        spans: Vec::new(),
                        anchor,
                    });
                    column = 0;
                    continue;
                }
                let repetitions = if byte == b'\t' { 4 } else { 1 };
                let character = if byte == b'\t' { ' ' } else { char::from(byte) };
                for _ in 0..repetitions {
                    if column == width {
                        self.rows
                            .last_mut()
                            .expect("literal layout always has a row")
                            .push(SemanticSpan::new(
                                std::mem::take(&mut pending),
                                ThemeRole::Text,
                            ));
                        let anchor = self
                            .rows
                            .last()
                            .map_or(0, |line| line.anchor.saturating_add(1));
                        self.rows.push(SemanticLine {
                            spans: Vec::new(),
                            anchor,
                        });
                        column = 0;
                    }
                    pending.push(character);
                    column = column.saturating_add(1);
                }
            }
            self.rows
                .last_mut()
                .expect("literal layout always has a row")
                .push(SemanticSpan::new(pending, ThemeRole::Text));
            let added_rows = self.rows.len().saturating_sub(previous_rows);
            let added_spans = self
                .rows
                .iter()
                .map(|row| row.spans.len())
                .sum::<usize>()
                .saturating_sub(previous_spans);
            let expanded_bytes = suffix
                .len()
                .saturating_add(suffix.bytes().filter(|byte| *byte == b'\t').count() * 3);
            self.bytes = self
                .bytes
                .saturating_add(expanded_bytes)
                .saturating_add(added_rows * std::mem::size_of::<SemanticLine>())
                .saturating_add(added_spans * std::mem::size_of::<SemanticSpan>());
            return;
        }
        for grapheme in suffix.graphemes(true) {
            if grapheme == "\n" {
                let anchor = self
                    .rows
                    .last()
                    .map_or(0, |line| line.anchor.saturating_add(1));
                self.rows.push(SemanticLine {
                    spans: Vec::new(),
                    anchor,
                });
                column = 0;
                continue;
            }
            let text = if grapheme == "\t" { "    " } else { grapheme };
            for grapheme in text.graphemes(true) {
                let source_width = UnicodeWidthStr::width(grapheme).max(1);
                let (grapheme, grapheme_width) = if source_width > width {
                    ("\u{fffd}", 1)
                } else {
                    (grapheme, source_width)
                };
                if column.saturating_add(grapheme_width) > width {
                    let anchor = self
                        .rows
                        .last()
                        .map_or(0, |line| line.anchor.saturating_add(1));
                    self.rows.push(SemanticLine {
                        spans: Vec::new(),
                        anchor,
                    });
                    column = 0;
                }
                self.rows
                    .last_mut()
                    .expect("literal layout always has a row")
                    .push(SemanticSpan::new(grapheme, ThemeRole::Text));
                column = column.saturating_add(grapheme_width);
            }
        }
        let added_rows = self.rows.len().saturating_sub(previous_rows);
        let added_spans = self
            .rows
            .iter()
            .map(|row| row.spans.len())
            .sum::<usize>()
            .saturating_sub(previous_spans);
        self.bytes = self
            .bytes
            .saturating_add(suffix.len())
            .saturating_add(added_rows * std::mem::size_of::<SemanticLine>())
            .saturating_add(added_spans * std::mem::size_of::<SemanticSpan>());
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn is_literal_projection(&self) -> bool {
        self.literal.is_some()
    }

    pub(super) fn visually_eq(&self, other: &Self) -> bool {
        if self.literal.is_some() || other.literal.is_some() {
            return self.literal == other.literal;
        }
        self.rows.len() == other.rows.len()
            && self
                .rows
                .iter()
                .zip(&other.rows)
                .all(|(left, right)| left.spans == right.spans)
    }

    pub(super) fn anchor_for_row(&self, row: usize) -> MarkdownAnchor {
        MarkdownAnchor {
            offset: if let Some(literal) = &self.literal {
                literal.anchor_for_row(row)
            } else {
                self.rows.get(row).map_or(0, |line| line.anchor)
            },
            token: self.row_text(row).and_then(stable_anchor_token),
        }
    }

    pub(super) fn row_for_anchor(&self, anchor: &MarkdownAnchor) -> usize {
        let approximate = if let Some(literal) = &self.literal {
            literal.row_for_anchor(anchor.offset)
        } else {
            self.rows
                .partition_point(|line| line.anchor <= anchor.offset)
                .saturating_sub(1)
        };
        let Some(token) = anchor.token.as_deref() else {
            return approximate;
        };
        let height = self.height();
        for distance in 0..=height.min(512) {
            let before = approximate.checked_sub(distance);
            let after = approximate
                .checked_add(distance)
                .filter(|row| *row < height);
            for row in [before, after].into_iter().flatten() {
                if self
                    .row_text(row)
                    .is_some_and(|text| row_contains_token(&text, token))
                {
                    return row;
                }
            }
        }
        approximate
    }

    pub(super) fn line(&self, index: usize, theme: &Theme) -> Option<Line<'static>> {
        if let Some(literal) = &self.literal {
            return Some(Line::styled(
                literal.line(index)?,
                theme.style(ThemeRole::Text),
            ));
        }
        let row = self.rows.get(index)?;
        Some(Line::from(
            row.spans
                .iter()
                .map(|span| {
                    Span::styled(
                        span.text.clone(),
                        theme.style(span.role).add_modifier(span.modifiers),
                    )
                })
                .collect::<Vec<_>>(),
        ))
    }

    fn row_text(&self, row: usize) -> Option<String> {
        if let Some(literal) = &self.literal {
            return literal.line(row);
        }
        self.rows.get(row).map(|line| {
            line.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
    }
}

fn foreground_plain_text(source: &str) -> String {
    let unclosed = if source.contains("```") || source.contains("~~~") {
        unclosed_fence_start(source)
    } else {
        None
    };
    let (parsed, literal) = unclosed.map_or((source, ""), |start| source.split_at(start));
    if !foreground_needs_parser(parsed) {
        let mut output = String::with_capacity(source.len());
        for line in parsed.split_inclusive('\n') {
            let (body, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"));
            let trimmed = body.trim_start_matches(' ');
            let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
                let indentation = body.len().saturating_sub(trimmed.len());
                output.push_str(&body[..indentation]);
                output.push_str(&trimmed[hashes + 1..]);
            } else {
                output.push_str(body);
            }
            output.push_str(newline);
        }
        output.push_str(literal);
        return output;
    }
    let mut output = String::with_capacity(source.len());
    let mut fence: Option<(char, usize, String, String)> = None;
    for line in parsed.split_inclusive('\n') {
        let (body, newline) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        let trimmed = body.trim_start_matches(' ');
        let mut fence_content = trimmed;
        let mut fence_prefix = String::new();
        let mut fence_continuation = String::new();
        while let Some(quoted) = fence_content.strip_prefix('>') {
            fence_prefix.push_str("│ ");
            fence_continuation.push_str("│ ");
            fence_content = quoted.strip_prefix(' ').unwrap_or(quoted);
        }
        if let Some(item) = fence_content
            .strip_prefix("- ")
            .or_else(|| fence_content.strip_prefix("+ "))
            .or_else(|| fence_content.strip_prefix("* "))
        {
            fence_prefix.push_str("• ");
            fence_continuation.push_str("  ");
            fence_content = item;
        }
        let fence_character = fence_content.chars().next().filter(|character| {
            (*character == '`' || *character == '~')
                && fence_content
                    .chars()
                    .take_while(|candidate| candidate == character)
                    .count()
                    >= 3
        });
        let fence_length = fence_character.map(|character| {
            fence_content
                .chars()
                .take_while(|candidate| *candidate == character)
                .count()
        });
        if let Some(character) = fence_character
            && fence.as_ref().is_none_or(|(open, length, _, _)| {
                *open == character
                    && fence_length.is_some_and(|current| current >= *length)
                    && fence_content[fence_length.unwrap_or_default()..]
                        .trim()
                        .is_empty()
            })
        {
            if let Some((_, _, _, continuation)) = fence.take() {
                output.push_str(&continuation);
                output.push_str("└─");
                output.push_str(newline);
            } else {
                fence = Some((
                    character,
                    fence_length.unwrap_or(3),
                    fence_prefix.clone(),
                    fence_continuation.clone(),
                ));
                let info = fence_content
                    .trim_start_matches(character)
                    .split_whitespace()
                    .next()
                    .unwrap_or_default();
                output.push_str(&fence_prefix);
                output.push_str("┌─");
                if !info.is_empty() {
                    output.push(' ');
                    output.push_str(info);
                }
                output.push_str(newline);
            }
            continue;
        }
        if let Some((_, _, _, prefix)) = &fence {
            output.push_str(prefix);
            output.push_str("│ ");
            output.push_str(strip_foreground_container(body));
            output.push_str(newline);
            continue;
        }
        let mut content = trimmed;
        let indentation = body.len().saturating_sub(trimmed.len());
        output.push_str(&body[..indentation]);
        while let Some(quoted) = content.strip_prefix('>') {
            output.push_str("│ ");
            content = quoted.strip_prefix(' ').unwrap_or(quoted);
        }
        let hashes = content.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&hashes) && content.as_bytes().get(hashes) == Some(&b' ') {
            content = &content[hashes + 1..];
        }
        if let Some(item) = content
            .strip_prefix("- ")
            .or_else(|| content.strip_prefix("+ "))
            .or_else(|| content.strip_prefix("* "))
        {
            output.push_str("• ");
            content = item;
        }
        if let Some(task) = content
            .strip_prefix("[x] ")
            .or_else(|| content.strip_prefix("[X] "))
        {
            output.push_str("☑ ");
            content = task;
        } else if let Some(task) = content.strip_prefix("[ ] ") {
            output.push_str("☐ ");
            content = task;
        }
        if content.len() >= 3 && content.bytes().all(|byte| byte == b'-') {
            output.push_str("───");
            output.push_str(newline);
            continue;
        }
        output.push_str(&strip_inline_markers(content));
        output.push_str(newline);
    }
    while output.ends_with('\n') {
        output.pop();
    }
    if !literal.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(literal);
    }
    output
}

pub(super) fn foreground_suffix(source: &str) -> String {
    foreground_plain_text(source)
}

fn strip_foreground_container(body: &str) -> &str {
    let mut content = body.trim_start_matches(' ');
    let had_quote = content.starts_with('>');
    while let Some(quoted) = content.strip_prefix('>') {
        content = quoted.strip_prefix(' ').unwrap_or(quoted);
    }
    if had_quote {
        return content;
    }
    if let Some(item) = content
        .strip_prefix("- ")
        .or_else(|| content.strip_prefix("+ "))
        .or_else(|| content.strip_prefix("* "))
    {
        return item;
    }
    body
}

fn foreground_needs_parser(source: &str) -> bool {
    source
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'_' | b'~' | b'`' | b'[' | b']' | b'>' | b'!'))
        || source
            .lines()
            .any(|line| line.starts_with("- ") || line.starts_with("+ "))
}

fn strip_inline_markers(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while cursor < source.len() {
        let rest = &source[cursor..];
        if let Some(escaped) = rest.strip_prefix('\\') {
            if let Some(next) = escaped.chars().next() {
                output.push(next);
                cursor = cursor.saturating_add(1 + next.len_utf8());
            } else {
                output.push('\\');
                cursor = cursor.saturating_add(1);
            }
            continue;
        }
        if rest.starts_with('`')
            && let Some(close) = find_unescaped(rest, "`", 1)
        {
            output.push_str(&rest[1..close]);
            cursor = cursor.saturating_add(close + 1);
            continue;
        }
        if rest.starts_with("![")
            && let Some(close) = rest.find(')')
        {
            output.push_str(&rest[..close + 1]);
            cursor = cursor.saturating_add(close + 1);
            continue;
        }
        if rest.starts_with('[')
            && let Some(label_end) = rest.find("](")
            && let Some(destination_end) = rest[label_end + 2..].find(')')
        {
            let destination_end = label_end + 2 + destination_end;
            let label = &rest[1..label_end];
            let destination = &rest[label_end + 2..destination_end];
            output.push_str(label);
            if label != destination {
                output.push_str(" → ");
                output.push_str(destination);
            }
            cursor = cursor.saturating_add(destination_end + 1);
            continue;
        }
        let delimiter = ["**", "__", "~~", "*", "_", "~"]
            .into_iter()
            .find(|delimiter| rest.starts_with(delimiter));
        if let Some(delimiter) = delimiter
            && let Some(close) = find_unescaped(rest, delimiter, delimiter.len())
            && close > delimiter.len()
            && !(delimiter == "_"
                && source[..cursor]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                && rest[close + delimiter.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric))
        {
            output.push_str(&rest[delimiter.len()..close]);
            cursor = cursor.saturating_add(close + delimiter.len());
            continue;
        }
        let Some(character) = rest.chars().next() else {
            break;
        };
        output.push(character);
        cursor = cursor.saturating_add(character.len_utf8());
    }
    output
}

fn find_unescaped(source: &str, needle: &str, start: usize) -> Option<usize> {
    let mut cursor = start;
    while cursor < source.len() {
        let relative = source[cursor..].find(needle)?;
        let position = cursor.saturating_add(relative);
        let escaped = source[..position]
            .chars()
            .rev()
            .take_while(|c| *c == '\\')
            .count()
            % 2
            == 1;
        if !escaped {
            return Some(position);
        }
        cursor = position.saturating_add(needle.len());
    }
    None
}

fn stable_anchor_token(text: String) -> Option<String> {
    let mut tokens = text
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .filter(|token| token.chars().count() >= 4);
    let first = tokens.next()?;
    Some(tokens.next().unwrap_or(first).to_owned())
}

fn row_contains_token(text: &str, expected: &str) -> bool {
    text.split(|character: char| {
        !character.is_alphanumeric() && character != '_' && character != '-'
    })
    .any(|token| token == expected)
}

fn markdown_options() -> Options {
    Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
        | Options::ENABLE_DEFINITION_LIST
}

fn protect_unsupported(source: &str) -> String {
    let mut ranges = Vec::<Range<usize>>::new();
    let mut image_ids = Vec::new();
    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { id, .. }) => {
                if !id.is_empty() {
                    image_ids.push(id.to_ascii_lowercase());
                }
                ranges.push(range);
            }
            Event::Start(Tag::Table(_) | Tag::FootnoteDefinition(_) | Tag::DefinitionList) => {
                ranges.push(range)
            }
            Event::Start(Tag::HtmlBlock)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_) => ranges.push(range),
            _ => {}
        }
    }
    if !image_ids.is_empty() {
        let mut offset = 0usize;
        for line in source.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('[')
                && let Some((label, destination)) = rest.split_once("]:")
                && !destination.trim().is_empty()
                && image_ids
                    .iter()
                    .any(|image_id| image_id.eq_ignore_ascii_case(label.trim()))
            {
                ranges.push(offset..offset.saturating_add(line.len()));
            }
            offset = offset.saturating_add(line.len());
        }
    }
    if ranges.is_empty() {
        return source.to_owned();
    }
    ranges.sort_by_key(|range| range.start);
    let mut merged = Vec::<Range<usize>>::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    let mut protected = String::with_capacity(source.len());
    let mut offset = 0usize;
    for range in merged {
        protected.push_str(&source[offset..range.start]);
        escape_markdown_literal(&source[range.clone()], &mut protected);
        offset = range.end;
    }
    protected.push_str(&source[offset..]);
    protected
}

fn escape_markdown_literal(source: &str, output: &mut String) {
    for character in source.chars() {
        if character.is_ascii_punctuation() {
            output.push('\\');
        }
        output.push(character);
    }
}

#[derive(Debug, Clone)]
struct ListState {
    next: Option<u64>,
    marker: Option<String>,
    marker_used: bool,
}

#[derive(Debug)]
struct LinkState {
    destination: String,
    visible: String,
    autolink: bool,
    image_title: Option<String>,
}

#[derive(Debug)]
struct CodeBlock {
    language: Option<String>,
    text: String,
}

struct MarkdownBuilder {
    width: usize,
    syntax_highlighting: bool,
    logical: Vec<LogicalLine>,
    current: Vec<SemanticSpan>,
    first_prefix: Vec<SemanticSpan>,
    continuation_prefix: Vec<SemanticSpan>,
    prefixes_ready: bool,
    text_block: bool,
    needs_gap: bool,
    heading: Option<ThemeRole>,
    quote_depth: usize,
    lists: Vec<ListState>,
    emphasis: usize,
    strong: usize,
    strikethrough: usize,
    links: Vec<LinkState>,
    code: Option<CodeBlock>,
}

impl MarkdownBuilder {
    fn new(width: usize, syntax_highlighting: bool) -> Self {
        Self {
            width,
            syntax_highlighting,
            logical: Vec::new(),
            current: Vec::new(),
            first_prefix: Vec::new(),
            continuation_prefix: Vec::new(),
            prefixes_ready: false,
            text_block: false,
            needs_gap: false,
            heading: None,
            quote_depth: 0,
            lists: Vec::new(),
            emphasis: 0,
            strong: 0,
            strikethrough: 0,
            links: Vec::new(),
            code: None,
        }
    }

    fn parse(&mut self, source: &str) {
        if source.is_empty() {
            return;
        }
        for event in Parser::new_ext(source, markdown_options()) {
            self.event(event);
        }
        self.end_text_block();
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(code) = &mut self.code {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().expect("code block exists");
                    self.emit_code_block(code);
                }
                Event::Text(text) | Event::Code(text) => code.text.push_str(&text),
                Event::SoftBreak | Event::HardBreak => code.text.push('\n'),
                Event::Html(text) | Event::InlineHtml(text) => code.text.push_str(&text),
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.append_text(&text, None),
            Event::Code(text) => self.append_text(&text, Some(ThemeRole::MarkdownInlineCode)),
            Event::Html(text) | Event::InlineHtml(text) => self.append_text(&text, None),
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            Event::Rule => self.emit_rule(),
            Event::TaskListMarker(checked) => {
                if let Some(list) = self.lists.last_mut() {
                    list.marker = Some(if checked { "☑" } else { "☐" }.to_owned());
                }
            }
            Event::FootnoteReference(label) => {
                self.append_text(&format!("[^{label}]"), None);
            }
            Event::InlineMath(math) => self.append_text(&format!("${math}$"), None),
            Event::DisplayMath(math) => self.append_text(&format!("$${math}$$"), None),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_text_block(None),
            Tag::Heading { level, .. } => {
                self.begin_text_block(Some(heading_role(level)));
            }
            Tag::BlockQuote(_) => {
                self.end_text_block();
                self.begin_spaced_block();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.end_text_block();
                self.begin_spaced_block();
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|value| !value.is_empty())
                        .map(normalize_language),
                };
                self.code = Some(CodeBlock {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                self.end_text_block();
                if self.lists.is_empty() {
                    self.begin_spaced_block();
                }
                self.lists.push(ListState {
                    next: start,
                    marker: None,
                    marker_used: false,
                });
            }
            Tag::Item => {
                self.end_text_block();
                if let Some(list) = self.lists.last_mut() {
                    list.marker = Some(match list.next {
                        Some(number) => {
                            list.next = Some(number.saturating_add(1));
                            format!("{number}.")
                        }
                        None => "•".to_owned(),
                    });
                    list.marker_used = false;
                }
            }
            Tag::Emphasis => self.emphasis = self.emphasis.saturating_add(1),
            Tag::Strong => self.strong = self.strong.saturating_add(1),
            Tag::Strikethrough => self.strikethrough = self.strikethrough.saturating_add(1),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => self.links.push(LinkState {
                destination: dest_url.into_string(),
                visible: String::new(),
                autolink: matches!(link_type, LinkType::Autolink | LinkType::Email),
                image_title: None,
            }),
            Tag::Image {
                dest_url, title, ..
            } => {
                self.append_text("![", None);
                self.links.push(LinkState {
                    destination: dest_url.into_string(),
                    visible: String::new(),
                    autolink: false,
                    image_title: Some(title.into_string()),
                });
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_text_block(),
            TagEnd::Heading(_) => {
                self.end_text_block();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.end_text_block();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    self.needs_gap = true;
                }
            }
            TagEnd::List(_) => {
                self.end_text_block();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.needs_gap = true;
                }
            }
            TagEnd::Item => self.end_text_block(),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => {
                self.strikethrough = self.strikethrough.saturating_sub(1);
            }
            TagEnd::Link | TagEnd::Image => self.end_link(),
            TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn begin_text_block(&mut self, heading: Option<ThemeRole>) {
        self.end_text_block();
        if self.lists.is_empty() && self.quote_depth == 0 {
            self.begin_spaced_block();
        }
        self.heading = heading;
        self.text_block = true;
        self.prefixes_ready = false;
    }

    fn end_text_block(&mut self) {
        if !self.text_block && self.current.is_empty() {
            return;
        }
        self.flush_line();
        self.text_block = false;
        self.prefixes_ready = false;
        if self.lists.is_empty() && self.quote_depth == 0 {
            self.needs_gap = true;
        }
    }

    fn begin_spaced_block(&mut self) {
        if self.needs_gap
            && self
                .logical
                .last()
                .is_some_and(|line| !line.content.is_empty() || !line.first_prefix.is_empty())
        {
            self.logical.push(LogicalLine {
                first_prefix: Vec::new(),
                continuation_prefix: Vec::new(),
                content: Vec::new(),
            });
        }
        self.needs_gap = false;
    }

    fn ensure_text_block(&mut self) {
        if !self.text_block {
            self.text_block = true;
        }
        if !self.prefixes_ready {
            let (first, continuation) = self.block_prefixes();
            self.first_prefix = first;
            self.continuation_prefix = continuation;
            self.prefixes_ready = true;
        }
    }

    fn block_prefixes(&mut self) -> (Vec<SemanticSpan>, Vec<SemanticSpan>) {
        let mut first = Vec::new();
        for _ in 0..self.quote_depth {
            first.push(SemanticSpan::new("│ ", ThemeRole::MarkdownQuote));
        }
        let mut continuation = first.clone();
        if !self.lists.is_empty() {
            let indent = "  ".repeat(self.lists.len().saturating_sub(1));
            first.push(SemanticSpan::new(indent.clone(), ThemeRole::Text));
            continuation.push(SemanticSpan::new(indent, ThemeRole::Text));
            let list = self.lists.last_mut().expect("list exists");
            let marker = list.marker.clone().unwrap_or_else(|| "•".to_owned());
            if list.marker_used {
                first.push(SemanticSpan::new(
                    " ".repeat(marker.width().saturating_add(1)),
                    ThemeRole::Text,
                ));
            } else {
                first.push(SemanticSpan::new(
                    format!("{marker} "),
                    ThemeRole::MarkdownQuote,
                ));
                list.marker_used = true;
            }
            continuation.push(SemanticSpan::new(
                " ".repeat(marker.width().saturating_add(1)),
                ThemeRole::Text,
            ));
        }
        (first, continuation)
    }

    fn append_text(&mut self, text: &str, forced_role: Option<ThemeRole>) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.ensure_text_block();
                for link in &mut self.links {
                    link.visible.push_str(part);
                }
                let (role, modifiers) = self.inline_style(forced_role);
                push_span(
                    &mut self.current,
                    SemanticSpan::new(part, role).with_modifiers(modifiers),
                );
            }
            if parts.peek().is_some() {
                self.break_line();
            }
        }
    }

    fn inline_style(&self, forced_role: Option<ThemeRole>) -> (ThemeRole, Modifier) {
        let role = if let Some(role) = forced_role {
            role
        } else if self.links.iter().any(|link| link.image_title.is_none()) {
            ThemeRole::MarkdownLink
        } else if self.strong > 0 {
            ThemeRole::MarkdownStrong
        } else if self.emphasis > 0 {
            ThemeRole::MarkdownEmphasis
        } else if self.strikethrough > 0 {
            ThemeRole::MarkdownStrikethrough
        } else {
            self.heading.unwrap_or(ThemeRole::Text)
        };
        let mut modifiers = Modifier::empty();
        if self.strong > 0 {
            modifiers.insert(Modifier::BOLD);
        }
        if self.emphasis > 0 {
            modifiers.insert(Modifier::ITALIC);
        }
        if self.strikethrough > 0 {
            modifiers.insert(Modifier::CROSSED_OUT);
        }
        if self.links.iter().any(|link| link.image_title.is_none()) {
            modifiers.insert(Modifier::UNDERLINED);
        }
        (role, modifiers)
    }

    fn break_line(&mut self) {
        self.ensure_text_block();
        self.flush_line();
        self.first_prefix = self.continuation_prefix.clone();
        self.prefixes_ready = true;
    }

    fn flush_line(&mut self) {
        if !self.prefixes_ready && self.current.is_empty() {
            return;
        }
        self.logical.push(LogicalLine {
            first_prefix: std::mem::take(&mut self.first_prefix),
            continuation_prefix: std::mem::take(&mut self.continuation_prefix),
            content: std::mem::take(&mut self.current),
        });
        self.prefixes_ready = false;
    }

    fn end_link(&mut self) {
        let Some(link) = self.links.pop() else {
            return;
        };
        if let Some(title) = link.image_title {
            let title = (!title.is_empty()).then(|| format!(" \"{title}\""));
            self.append_text(
                &format!(
                    "]({}{})",
                    link.destination,
                    title.as_deref().unwrap_or_default()
                ),
                None,
            );
        } else if !link.autolink && link.visible != link.destination {
            self.ensure_text_block();
            push_span(
                &mut self.current,
                SemanticSpan::new(format!(" → {}", link.destination), ThemeRole::MutedText),
            );
        }
    }

    fn emit_rule(&mut self) {
        self.end_text_block();
        self.begin_spaced_block();
        let (prefix, _) = self.block_prefixes();
        let prefix_width = spans_width(&prefix);
        self.logical.push(LogicalLine {
            first_prefix: prefix.clone(),
            continuation_prefix: prefix,
            content: vec![SemanticSpan::new(
                "─".repeat(self.width.saturating_sub(prefix_width).max(1)),
                ThemeRole::MarkdownRule,
            )],
        });
        self.needs_gap = true;
    }

    fn emit_code_block(&mut self, code: CodeBlock) {
        let (first, continuation) = self.block_prefixes();
        let label = code.language.as_deref().unwrap_or("code");
        self.logical.push(LogicalLine {
            first_prefix: first,
            continuation_prefix: continuation.clone(),
            content: vec![SemanticSpan::new(
                format!("┌─ {label}"),
                ThemeRole::MarkdownRule,
            )],
        });

        let code_prefix = [
            continuation.clone(),
            vec![SemanticSpan::new("│ ", ThemeRole::MarkdownRule)],
        ]
        .concat();
        let highlighted = if self.syntax_highlighting {
            highlight_code(&code.text, code.language.as_deref())
        } else {
            plain_code_lines(&code.text.replace('\t', "    "))
        };
        for spans in highlighted {
            self.logical.push(LogicalLine {
                first_prefix: code_prefix.clone(),
                continuation_prefix: code_prefix.clone(),
                content: spans,
            });
        }
        self.logical.push(LogicalLine {
            first_prefix: continuation.clone(),
            continuation_prefix: continuation,
            content: vec![SemanticSpan::new("└─", ThemeRole::MarkdownRule)],
        });
        self.needs_gap = true;
    }

    fn append_literal_block(&mut self, source: &str) {
        self.end_text_block();
        self.begin_spaced_block();
        for line in source.split('\n') {
            self.logical.push(LogicalLine {
                first_prefix: Vec::new(),
                continuation_prefix: Vec::new(),
                content: vec![SemanticSpan::new(line, ThemeRole::Text)],
            });
        }
    }

    fn finish(self) -> Vec<SemanticLine> {
        let mut rows = Vec::new();
        let mut anchor = 0usize;
        for line in self.logical {
            let content_length = line
                .content
                .iter()
                .map(|span| span.text.len())
                .sum::<usize>();
            rows.extend(wrap_logical(line, self.width, anchor));
            anchor = anchor
                .saturating_add(content_length.max(1))
                .saturating_add(1);
        }
        rows
    }
}

fn heading_role(level: HeadingLevel) -> ThemeRole {
    match level {
        HeadingLevel::H1 => ThemeRole::MarkdownHeading1,
        HeadingLevel::H2 => ThemeRole::MarkdownHeading2,
        HeadingLevel::H3 => ThemeRole::MarkdownHeading3,
        HeadingLevel::H4 => ThemeRole::MarkdownHeading4,
        HeadingLevel::H5 => ThemeRole::MarkdownHeading5,
        HeadingLevel::H6 => ThemeRole::MarkdownHeading6,
    }
}

fn normalize_language(language: &str) -> String {
    language
        .trim_matches(|character| matches!(character, '{' | '}' | '.'))
        .to_ascii_lowercase()
}

fn push_span(spans: &mut Vec<SemanticSpan>, span: SemanticSpan) {
    if span.text.is_empty() {
        return;
    }
    if let Some(previous) = spans.last_mut()
        && previous.role == span.role
        && previous.modifiers == span.modifiers
    {
        previous.text.push_str(&span.text);
    } else {
        spans.push(span);
    }
}

fn spans_width(spans: &[SemanticSpan]) -> usize {
    spans.iter().map(|span| span.text.width()).sum()
}

fn wrap_logical(line: LogicalLine, width: usize, anchor: usize) -> Vec<SemanticLine> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = SemanticLine {
        spans: Vec::new(),
        anchor,
    };
    let first_prefix = fit_prefix(line.first_prefix, width);
    let continuation_prefix = fit_prefix(line.continuation_prefix, width);
    let mut column = append_prefix(&mut row, &first_prefix);
    let mut row_has_content = false;
    let mut consumed = 0usize;

    for span in line.content {
        for grapheme in span.text.graphemes(true) {
            let source_width = UnicodeWidthStr::width(grapheme).max(1);
            let (grapheme, grapheme_width) = if source_width > width {
                ("\u{fffd}", 1)
            } else {
                (grapheme, source_width)
            };
            if row_has_content && column.saturating_add(grapheme_width) > width {
                rows.push(row);
                row = SemanticLine {
                    spans: Vec::new(),
                    anchor: anchor.saturating_add(consumed),
                };
                column = append_prefix(&mut row, &continuation_prefix);
                row_has_content = false;
            }
            if !row_has_content && column > 0 && column.saturating_add(grapheme_width) > width {
                row = SemanticLine {
                    spans: Vec::new(),
                    anchor: anchor.saturating_add(consumed),
                };
                column = 0;
            }
            row.push(SemanticSpan {
                text: grapheme.to_owned(),
                role: span.role,
                modifiers: span.modifiers,
            });
            column = column.saturating_add(grapheme_width);
            row_has_content = true;
            consumed = consumed.saturating_add(grapheme.len());
        }
    }
    rows.push(row);
    rows
}

fn append_prefix(line: &mut SemanticLine, prefix: &[SemanticSpan]) -> usize {
    for span in prefix {
        line.push(span.clone());
    }
    spans_width(prefix)
}

fn fit_prefix(prefix: Vec<SemanticSpan>, width: usize) -> Vec<SemanticSpan> {
    let maximum = width.saturating_sub(1);
    if spans_width(&prefix) <= maximum {
        return prefix;
    }
    let mut fitted = Vec::new();
    let mut used = 0usize;
    for span in prefix {
        for grapheme in span.text.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if used.saturating_add(grapheme_width) > maximum {
                return fitted;
            }
            push_span(
                &mut fitted,
                SemanticSpan {
                    text: grapheme.to_owned(),
                    role: span.role,
                    modifiers: span.modifiers,
                },
            );
            used = used.saturating_add(grapheme_width);
        }
    }
    fitted
}

fn unclosed_fence_start(source: &str) -> Option<usize> {
    let mut open: Option<(usize, char, usize, FenceContainer)> = None;
    let mut list_continuation: Option<FenceContainer> = None;
    let mut offset = 0usize;
    for line_with_ending in source.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let expected_container = open
            .map(|(_, _, _, container)| container)
            .or(list_continuation);
        if let Some(candidate) = container_fence_candidate(line, expected_container) {
            let character = candidate.text.chars().next();
            if matches!(character, Some('`' | '~')) {
                let character = character.expect("fence character");
                let length = candidate
                    .text
                    .chars()
                    .take_while(|current| *current == character)
                    .count();
                if length >= 3 {
                    match open {
                        None => open = Some((offset, character, length, candidate.container)),
                        Some((_, open_character, open_length, open_container))
                            if open_character == character
                                && length >= open_length
                                && candidate.text[length..].trim().is_empty()
                                && open_container.matches(candidate.container) =>
                        {
                            open = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        if open.is_none() {
            if let Some(container) = list_container(line) {
                list_continuation = Some(container);
            } else if let Some(container) = list_continuation
                && !line.trim().is_empty()
                && line
                    .chars()
                    .take_while(|character| *character == ' ')
                    .count()
                    < container.list_indent.unwrap_or_default()
            {
                list_continuation = None;
            }
        }
        offset = offset.saturating_add(line_with_ending.len());
    }
    open.map(|(start, _, _, _)| start)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceContainer {
    quote_depth: usize,
    indent: usize,
    list_indent: Option<usize>,
}

impl FenceContainer {
    fn matches(self, closing: Self) -> bool {
        self.quote_depth == closing.quote_depth
            && match self.list_indent {
                Some(required) => {
                    closing.list_indent == Some(required) || closing.indent >= required
                }
                None if self.indent > 3 => {
                    closing.list_indent.is_none() && closing.indent >= self.indent
                }
                None => closing.list_indent.is_none() && closing.indent <= 3,
            }
    }
}

fn list_container(mut line: &str) -> Option<FenceContainer> {
    let mut quote_depth = 0usize;
    loop {
        let spaces = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        line = &line[spaces..];
        let Some(quoted) = line.strip_prefix('>') else {
            let (_, marker_width) = strip_list_marker(line)?;
            return Some(FenceContainer {
                quote_depth,
                indent: spaces,
                list_indent: Some(spaces.saturating_add(marker_width)),
            });
        };
        quote_depth = quote_depth.saturating_add(1);
        line = quoted.strip_prefix(' ').unwrap_or(quoted);
    }
}

struct FenceCandidate<'a> {
    text: &'a str,
    container: FenceContainer,
}

fn container_fence_candidate(
    mut line: &str,
    open_container: Option<FenceContainer>,
) -> Option<FenceCandidate<'_>> {
    let mut quote_depth = 0usize;
    let mut indent = 0usize;
    loop {
        let spaces = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        let continuation_indent = open_container
            .and_then(|container| container.list_indent)
            .is_some_and(|required| indent.saturating_add(spaces) >= required);
        let indented_list = strip_list_marker(&line[spaces..]).is_some();
        if spaces > 3 && !continuation_indent && !indented_list {
            return None;
        }
        indent = indent.saturating_add(spaces);
        line = &line[spaces..];
        let Some(quoted) = line.strip_prefix('>') else {
            break;
        };
        quote_depth = quote_depth.saturating_add(1);
        line = quoted.strip_prefix(' ').unwrap_or(quoted);
        indent = 0;
    }
    let list = strip_list_marker(line);
    let (text, list_indent) = match list {
        Some((item, marker_width)) => (item, Some(indent.saturating_add(marker_width))),
        None => (line, None),
    };
    Some(FenceCandidate {
        text,
        container: FenceContainer {
            quote_depth,
            indent,
            list_indent,
        },
    })
}

fn strip_list_marker(line: &str) -> Option<(&str, usize)> {
    let marker_end = if line.starts_with(['-', '+', '*']) {
        1
    } else {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count().min(9);
        (digits > 0 && matches!(line.as_bytes().get(digits), Some(b'.' | b')')))
            .then_some(digits + 1)?
    };
    let rest = &line[marker_end..];
    let spaces = rest
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .count();
    (spaces > 0).then(|| (&rest[spaces..], marker_end.saturating_add(spaces)))
}

struct SyntaxSelectors {
    comment: ScopeSelectors,
    string: ScopeSelectors,
    keyword: ScopeSelectors,
    constant: ScopeSelectors,
    kind: ScopeSelectors,
    function: ScopeSelectors,
    operator: ScopeSelectors,
}

impl SyntaxSelectors {
    fn new() -> Self {
        let parse = |selector| ScopeSelectors::from_str(selector).expect("valid scope selector");
        Self {
            comment: parse("comment"),
            string: parse("string"),
            keyword: parse("keyword, storage"),
            constant: parse("constant.numeric, constant.language, constant.character"),
            kind: parse(
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, support.type, storage.type",
            ),
            function: parse("entity.name.function, support.function, entity.name.function.macro"),
            operator: parse("keyword.operator, punctuation"),
        }
    }

    fn role(&self, stack: &ScopeStack) -> ThemeRole {
        let scopes = stack.as_slice();
        if self.comment.does_match(scopes).is_some() {
            ThemeRole::CodeComment
        } else if self.string.does_match(scopes).is_some() {
            ThemeRole::CodeString
        } else if self.kind.does_match(scopes).is_some() {
            ThemeRole::CodeType
        } else if self.function.does_match(scopes).is_some() {
            ThemeRole::CodeFunction
        } else if self.constant.does_match(scopes).is_some() {
            ThemeRole::CodeConstant
        } else if self.keyword.does_match(scopes).is_some() {
            ThemeRole::CodeKeyword
        } else if self.operator.does_match(scopes).is_some() {
            ThemeRole::CodeOperator
        } else {
            ThemeRole::CodeText
        }
    }
}

fn highlight_code(source: &str, language: Option<&str>) -> Vec<Vec<SemanticSpan>> {
    let expanded = source.replace('\t', "    ");
    let syntax_set = syntax_set();
    let syntax = language.and_then(|language| syntax_set.find_syntax_by_token(language));
    let Some(syntax) = syntax else {
        return plain_code_lines(&expanded);
    };
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let selectors = syntax_selectors();
    let mut lines = Vec::new();
    for line in code_lines_with_endings(&expanded) {
        let Ok(operations) = parser.parse_line(line, syntax_set) else {
            return plain_code_lines(&expanded);
        };
        let mut spans = Vec::new();
        for (region, operation) in ScopeRegionIterator::new(&operations, line) {
            if stack.apply(operation).is_err() {
                return plain_code_lines(&expanded);
            }
            let region = region.trim_end_matches(['\r', '\n']);
            if !region.is_empty() {
                push_span(
                    &mut spans,
                    SemanticSpan::new(region, selectors.role(&stack)),
                );
            }
        }
        lines.push(spans);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn code_lines_with_endings(source: &str) -> Vec<&str> {
    if source.is_empty() {
        return vec![""];
    }
    let mut lines = source.split_inclusive('\n').collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn plain_code_lines(source: &str) -> Vec<Vec<SemanticSpan>> {
    let lines = if source.is_empty() {
        vec![""]
    } else {
        source.lines().collect::<Vec<_>>()
    };
    lines
        .into_iter()
        .map(|line| vec![SemanticSpan::new(line, ThemeRole::CodeText)])
        .collect()
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntax_selectors() -> &'static SyntaxSelectors {
    static SELECTORS: OnceLock<SyntaxSelectors> = OnceLock::new();
    SELECTORS.get_or_init(SyntaxSelectors::new)
}

#[cfg(test)]
mod tests {
    use super::{LITERAL_CHECKPOINT_ROWS, MarkdownLayout, checkpoint_for_row};
    use crate::theme::{Theme, ThemeId, ThemeRole};
    use unicode_width::UnicodeWidthStr;

    fn symbols(layout: &MarkdownLayout) -> Vec<String> {
        let theme = Theme::default();
        (0..layout.height())
            .map(|index| layout.line(index, &theme).unwrap().to_string())
            .collect()
    }

    #[test]
    fn valid_inline_markdown_hides_delimiters_and_styles_text() {
        let layout = MarkdownLayout::new("plain **strong** and *emphasis* with `code`", 80);

        assert_eq!(symbols(&layout), ["plain strong and emphasis with code"]);
        let line = layout.line(0, &Theme::default()).unwrap();
        assert_eq!(
            line.spans[1].style.fg,
            Theme::default().style(ThemeRole::MarkdownStrong).fg
        );
        assert_eq!(
            line.spans[3].style.fg,
            Theme::default().style(ThemeRole::MarkdownEmphasis).fg
        );
        assert_eq!(
            line.spans[5].style.fg,
            Theme::default().style(ThemeRole::MarkdownInlineCode).fg
        );
    }

    #[test]
    fn headings_lists_quotes_and_rules_use_terminal_structure() {
        let layout = MarkdownLayout::new(
            "# First\n\n## Second\n\n> quoted\n\n3. three\n4. four\n\n- [x] done\n- [ ] todo\n\n---",
            40,
        );
        let rendered = symbols(&layout).join("\n");

        assert!(!rendered.contains("# First"));
        assert!(rendered.contains("First\n\nSecond"));
        assert!(rendered.contains("│ quoted"));
        assert!(rendered.contains("3. three\n4. four"));
        assert!(rendered.contains("☑ done\n☐ todo"));
        assert!(rendered.contains("────────────────"));
    }

    #[test]
    fn fenced_code_is_framed_and_language_aware() {
        let layout = MarkdownLayout::new(
            "```rust\nfn main() {\n    let message = \"hello\";\n}\n```",
            40,
        );
        let rendered = symbols(&layout).join("\n");

        assert!(rendered.contains("┌─ rust"));
        assert!(rendered.contains("│ fn main() {"));
        assert!(rendered.contains("└─"));
        assert!(!rendered.contains("```"));

        let theme = Theme::default();
        let roles = (0..layout.height())
            .flat_map(|index| layout.line(index, &theme).unwrap().spans)
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(
            roles.len() >= 3,
            "expected several syntax colors: {roles:?}"
        );
    }

    #[test]
    fn incomplete_streaming_syntax_remains_literal_until_closed() {
        let inline = MarkdownLayout::new("before **unfinished", 40);
        assert_eq!(symbols(&inline), ["before **unfinished"]);

        let open_fence = MarkdownLayout::new("```rust\nfn main()", 40);
        assert_eq!(symbols(&open_fence), ["```rust", "fn main()"]);

        let closed_fence = MarkdownLayout::new("```rust\nfn main()\n```", 40);
        let rendered = symbols(&closed_fence).join("\n");
        assert!(rendered.contains("┌─ rust"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn unclosed_container_fences_remain_literal_until_their_matching_fence_arrives() {
        for source in [
            "> ```rust\n> fn main()",
            "- ```rust\n  fn main()",
            "> ~~~rust\n> fn main()",
        ] {
            assert_eq!(symbols(&MarkdownLayout::new(source, 80)).join("\n"), source);
        }
        let mismatched = "> ```rust\n> fn main()\n```";
        assert_eq!(
            symbols(&MarkdownLayout::new(mismatched, 80)).join("\n"),
            mismatched
        );

        let nested_closed = "> - ~~~rust\n>   fn main()\n>   ~~~";
        let rendered = symbols(&MarkdownLayout::new(nested_closed, 80)).join("\n");
        assert!(rendered.contains("┌─ rust"), "{rendered:?}");
        assert!(!rendered.contains("~~~"), "{rendered:?}");

        let nested_list = "- outer\n  - ```rust\n    code\n    ```";
        let rendered = symbols(&MarkdownLayout::new(nested_list, 80)).join("\n");
        assert!(rendered.contains("┌─ rust"), "{rendered:?}");
        assert!(rendered.contains("│ code"), "{rendered:?}");
        assert!(!rendered.contains("```"), "{rendered:?}");

        let deeply_nested_open = "- a\n  - b\n    - ```rust\n      code";
        let rendered = symbols(&MarkdownLayout::new(deeply_nested_open, 80)).join("\n");
        assert!(
            rendered.contains("    - ```rust\n      code"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("┌─ rust"), "{rendered:?}");

        let continuation_open = "- a\n  - b\n    ```rust\n    code";
        let rendered = symbols(&MarkdownLayout::new(continuation_open, 80)).join("\n");
        assert!(rendered.contains("    ```rust\n    code"), "{rendered:?}");
        assert!(!rendered.contains("┌─ rust"), "{rendered:?}");

        let wrong_container_close = "- a\n  - b\n    ```rust\n    code\n```";
        let rendered = symbols(&MarkdownLayout::new(wrong_container_close, 80)).join("\n");
        assert!(rendered.contains("```rust"), "{rendered:?}");
        assert!(!rendered.contains("┌─ rust"), "{rendered:?}");
    }

    #[test]
    fn list_code_frames_use_the_item_marker_once_and_hang_following_rows() {
        let rows = symbols(&MarkdownLayout::new(
            "- ```rust\n  let value = 1;\n  ```",
            80,
        ));

        assert!(rows[0].starts_with("• ┌─ rust"));
        assert!(rows[1].starts_with("  │ "));
        assert!(rows.last().is_some_and(|row| row.starts_with("  └─")));
        assert_eq!(rows.iter().filter(|row| row.starts_with("• ")).count(), 1);
    }

    #[test]
    fn links_unknown_code_and_unsupported_constructs_preserve_content() {
        let source = "[docs](https://example.com) and <https://rust-lang.org>\n\n| a | b |\n| - | - |\n\n<div>literal html</div>\n\n```not-a-language\nvalue = 42\n```";
        let rendered = symbols(&MarkdownLayout::new(source, 80)).join("\n");

        assert!(rendered.contains("docs → https://example.com"));
        assert_eq!(rendered.matches("https://rust-lang.org").count(), 1);
        assert!(rendered.contains("| a | b |"));
        assert!(rendered.contains("<div>literal html</div>"));
        assert!(rendered.contains("┌─ not-a-language"));
        assert!(rendered.contains("│ value = 42"));
    }

    #[test]
    fn unsupported_footnotes_math_and_definition_lists_preserve_their_text() {
        let source = "**styled**\n\nterm\n: *definition*\n\nFootnote[^note] and $*x* + y$.\n\n[^note]: *details*";
        let rendered = symbols(&MarkdownLayout::new(source, 120)).join("\n");

        for content in [
            "styled",
            "term",
            ": *definition*",
            "Footnote[^note]",
            "$*x* + y$",
            "[^note]: *details*",
        ] {
            assert!(
                rendered.contains(content),
                "missing {content:?}: {rendered:?}"
            );
        }
        assert!(!rendered.contains("**styled**"));
    }

    #[test]
    fn unsupported_images_preserve_alt_destination_and_title_as_markdown_text() {
        for source in [
            "Before ![diagram](img.png \"architecture\") after",
            "![escaped](img\\(1\\).png 'single title')",
            "![reference][diagram]\n\n[diagram]: img.png (parenthesized)",
        ] {
            assert_eq!(
                symbols(&MarkdownLayout::new(source, 120)).join("\n"),
                source
            );
        }

        let mixed = "**styled** ![**raw alt**](img.png) and `![code](not-an-image)`";
        assert_eq!(
            symbols(&MarkdownLayout::new(mixed, 120)).join("\n"),
            "styled ![**raw alt**](img.png) and ![code](not-an-image)"
        );

        let fenced = "```text\n![code](not-an-image)\n```";
        let rendered = symbols(&MarkdownLayout::new(fenced, 120)).join("\n");
        assert!(rendered.contains("┌─ text"), "{rendered:?}");
        assert!(rendered.contains("│ ![code](not-an-image)"), "{rendered:?}");
    }

    #[test]
    fn narrow_nested_content_wraps_within_width_and_repeats_structure() {
        let layout = MarkdownLayout::new(
            "> - a deliberately long nested list item with 🦀 unicode\n\n```text\n123456789012345\n```",
            12,
        );
        let rows = symbols(&layout);

        assert!(rows.iter().all(|row| row.width() <= 12), "{rows:?}");
        assert!(rows.iter().filter(|row| row.starts_with("│   ")).count() >= 2);
        assert!(rows.iter().filter(|row| row.starts_with("│ ")).count() >= 3);
    }

    #[test]
    fn heading_levels_are_distinct_in_every_bundled_theme() {
        let layout = MarkdownLayout::new(
            "# one\n\n## two\n\n### three\n\n#### four\n\n##### five\n\n###### six",
            40,
        );

        for id in ThemeId::ALL {
            let theme = Theme::resolve(id);
            let colors = (0..layout.height())
                .filter_map(|index| layout.line(index, &theme))
                .filter(|line| !line.to_string().is_empty())
                .filter_map(|line| line.spans.first().and_then(|span| span.style.fg))
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(colors.len(), 6, "heading palette for {id:?}: {colors:?}");
        }
    }

    #[test]
    fn inline_code_keeps_enclosing_markdown_modifiers() {
        let layout = MarkdownLayout::new("**_`code`_** and ~~`old`~~", 40);
        let line = layout.line(0, &Theme::default()).unwrap();
        let code = line
            .spans
            .iter()
            .find(|span| span.content == "code")
            .expect("nested inline code");
        assert!(
            code.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            code.style
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC)
        );
        let old = line
            .spans
            .iter()
            .find(|span| span.content == "old")
            .expect("struck inline code");
        assert!(
            old.style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn inline_precedence_keeps_the_highest_role_and_accumulates_all_modifiers() {
        let layout = MarkdownLayout::new("***~~[label](https://example.com)~~***", 80);
        let span = layout.rows[0]
            .spans
            .iter()
            .find(|span| span.text == "label")
            .expect("styled link label");

        assert_eq!(span.role, ThemeRole::MarkdownLink);
        for modifier in [
            ratatui::style::Modifier::BOLD,
            ratatui::style::Modifier::ITALIC,
            ratatui::style::Modifier::CROSSED_OUT,
            ratatui::style::Modifier::UNDERLINED,
        ] {
            assert!(span.modifiers.contains(modifier), "missing {modifier:?}");
        }
    }

    #[test]
    fn recognized_rust_code_maps_every_practical_syntax_role() {
        let layout = MarkdownLayout::new(
            "```rust\nstruct Widget;\nfn calculate(value: i32) -> usize {\n    // note\n    let message = \"hello\";\n    value + 42\n}\n```",
            100,
        );
        let roles = layout
            .rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.role))
            .collect::<Vec<_>>();

        for role in [
            ThemeRole::CodeText,
            ThemeRole::CodeKeyword,
            ThemeRole::CodeString,
            ThemeRole::CodeComment,
            ThemeRole::CodeConstant,
            ThemeRole::CodeType,
            ThemeRole::CodeFunction,
            ThemeRole::CodeOperator,
        ] {
            assert!(
                roles.contains(&role),
                "missing syntax role {role:?}: {roles:?}"
            );
        }
    }

    #[test]
    fn true_nested_lists_indent_each_level_and_hang_wrapped_rows() {
        let layout = MarkdownLayout::new(
            "- parent\n  - child with enough text to wrap\n    - grandchild",
            18,
        );
        let rows = symbols(&layout);

        assert!(
            rows.iter().any(|row| row.starts_with("• parent")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("  • child")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("    • grandchild")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("    ugh")),
            "{rows:?}"
        );
    }

    #[test]
    fn fenced_code_preserves_intentional_trailing_blank_lines_and_tabs() {
        let layout = MarkdownLayout::new("```rust\n\tlet value = 1;\n\n```", 40);
        let rows = symbols(&layout);

        assert!(rows.iter().any(|row| row == "│     let value = 1;"));
        let footer = rows.iter().position(|row| row == "└─").unwrap();
        assert_eq!(rows[footer - 1], "│ ");
    }

    #[test]
    fn large_foreground_projection_keeps_code_and_plain_underscore_content() {
        let source = format!(
            "{}\n`_λ[]_` and foo_bar another_value\n> ~~~rust\n> let value = 1;\n> ~~~\n- ```text\n- body\n- ```",
            "x".repeat(5 * 1024)
        );
        let rendered = symbols(&MarkdownLayout::foreground(&source, 120)).join("\n");

        assert!(rendered.contains("_λ[]_"), "{rendered:?}");
        assert!(rendered.contains("foo_bar another_value"), "{rendered:?}");
        assert!(rendered.contains("│ ┌─ rust"), "{rendered:?}");
        assert!(rendered.contains("│ │ let value = 1;"), "{rendered:?}");
        assert!(rendered.contains("• ┌─ text"), "{rendered:?}");
        assert!(rendered.contains("  │ body"), "{rendered:?}");
    }

    #[test]
    fn narrow_prefixes_never_clip_a_wide_grapheme() {
        let rows = symbols(&MarkdownLayout::new("> 🦀", 3));

        assert!(rows.iter().all(|row| row.width() <= 3), "{rows:?}");
        assert!(rows.join("").contains('🦀'));
    }

    #[test]
    fn continuation_prefixes_never_push_wide_graphemes_past_the_layout_width() {
        let layout = MarkdownLayout::new("> a🦀", 3);

        for row in symbols(&layout) {
            assert!(row.width() <= 3, "row {row:?} exceeded width 3");
        }
    }

    #[test]
    fn one_cell_layouts_use_a_safe_placeholder_for_wide_graphemes() {
        assert_eq!(symbols(&MarkdownLayout::new("🦀", 1)), ["�"]);
    }

    #[test]
    fn long_unicode_tail_rows_use_bounded_random_access_checkpoints() {
        let layout = MarkdownLayout::literal(&"λ".repeat(200_000), 1);
        let literal = layout.literal.as_ref().expect("compact literal projection");
        let line = literal.lines.first().expect("one logical line");
        assert!(line.checkpoints.len() > 700);

        for _ in 0..4 {
            for row in 199_980..200_000 {
                assert_eq!(
                    layout.line(row, &Theme::default()).unwrap().to_string(),
                    "λ"
                );
                let checkpoint = checkpoint_for_row(line, row);
                assert!(row.saturating_sub(checkpoint.row) < LITERAL_CHECKPOINT_ROWS);
            }
        }
    }

    #[test]
    fn unicode_anchor_crossing_semantic_and_literal_layouts_stays_on_a_character_boundary() {
        let semantic = MarkdownLayout::new("**λambda** and content", 12);
        let anchor = semantic.anchor_for_row(1);
        let literal = MarkdownLayout::literal("λambda and content", 6);
        let row = literal.row_for_anchor(&anchor);

        assert!(row < literal.height());
        assert!(
            literal
                .line(row, &Theme::default())
                .is_some_and(|line| !line.to_string().is_empty())
        );
    }

    #[test]
    fn ascii_appended_to_a_unicode_line_extends_random_access_checkpoints() {
        let mut layout = MarkdownLayout::literal("λ", 1);
        layout.append_literal(&"x".repeat(200_000), 1);
        let literal = layout.literal.as_ref().expect("compact literal projection");
        let line = literal.lines.first().expect("one logical line");
        assert!(line.checkpoints.len() > 700);

        for _ in 0..4 {
            for row in 199_981..200_001 {
                assert_eq!(
                    layout.line(row, &Theme::default()).unwrap().to_string(),
                    "x"
                );
                let checkpoint = checkpoint_for_row(line, row);
                assert!(row.saturating_sub(checkpoint.row) < LITERAL_CHECKPOINT_ROWS);
            }
        }
    }

    #[test]
    fn empty_escaped_entity_and_incomplete_inline_input_remain_readable() {
        assert_eq!(symbols(&MarkdownLayout::new("", 20)), [""]);

        let source = r"\*literal\* &amp; [unfinished](https://example.com and `open";
        assert_eq!(
            symbols(&MarkdownLayout::new(source, 80)),
            ["*literal* & [unfinished](https://example.com and `open"]
        );
    }

    #[test]
    fn every_layout_row_matches_measurement_at_common_terminal_widths() {
        let source = "## Heading\n\n> Wrapped **content** with 🦀 unicode.\n\n```rust\nfn main() { println!(\"hello\"); }\n```";
        for width in [20, 40, 80] {
            let layout = MarkdownLayout::new(source, width);
            let rows = symbols(&layout);
            assert_eq!(layout.height(), rows.len());
            assert!(
                rows.iter().all(|row| row.width() <= width),
                "width {width}: {rows:?}"
            );
        }
    }
}
