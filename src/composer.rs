use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::{collections::HashSet, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    Plan,
    #[default]
    Build,
}

impl SessionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Build => "Build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: String,
}

impl Attachment {
    pub fn workspace_file(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineTokenKind {
    FileReference { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineToken {
    pub range: Range<usize>,
    pub kind: InlineTokenKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerContent {
    text: String,
    tokens: Vec<InlineToken>,
}

impl ComposerContent {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tokens: Vec::new(),
        }
    }

    pub fn with_attachments(text: impl Into<String>, attachments: &[Attachment]) -> Self {
        let mut content = Self::plain(text);
        for attachment in attachments {
            if !content.text.is_empty() && !content.text.ends_with(char::is_whitespace) {
                content.text.push(' ');
            }
            let start = content.text.len();
            content.text.push('@');
            content.text.push_str(&attachment.path);
            content.tokens.push(InlineToken {
                range: start..content.text.len(),
                kind: InlineTokenKind::FileReference {
                    path: attachment.path.clone(),
                },
            });
        }
        content
    }
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tokens(&self) -> &[InlineToken] {
        &self.tokens
    }

    pub fn prompt_text(&self) -> String {
        self.text.clone()
    }

    pub fn attachments(&self) -> Vec<Attachment> {
        let mut paths = HashSet::new();
        self.tokens
            .iter()
            .filter_map(|token| match &token.kind {
                InlineTokenKind::FileReference { path } if paths.insert(path.clone()) => {
                    Some(Attachment::workspace_file(path))
                }
                InlineTokenKind::FileReference { .. } => None,
            })
            .collect()
    }

    pub fn lines(&self, text_style: Style, file_style: Style) -> Vec<Line<'static>> {
        let mut lines = vec![Vec::new()];
        let mut cursor = 0;
        for token in &self.tokens {
            push_text_lines(
                &mut lines,
                &self.text[cursor..token.range.start],
                text_style,
            );
            let style = match token.kind {
                InlineTokenKind::FileReference { .. } => file_style,
            };
            push_text_lines(&mut lines, &self.text[token.range.clone()], style);
            cursor = token.range.end;
        }
        push_text_lines(&mut lines, &self.text[cursor..], text_style);
        lines.into_iter().map(Line::from).collect()
    }
}

fn push_text_lines(lines: &mut Vec<Vec<Span<'static>>>, text: &str, style: Style) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(Vec::new());
        }
        if !part.is_empty() {
            lines
                .last_mut()
                .unwrap()
                .push(Span::styled(part.to_owned(), style));
        }
    }
}

pub(crate) struct ComposerLayout {
    pub lines: Vec<Line<'static>>,
    pub cursor: (u16, u16),
    stops: Vec<VisualStop>,
}

struct ComposerGrapheme {
    symbol: String,
    style: Style,
    byte_start: usize,
    width: usize,
    whitespace: bool,
}

#[derive(Debug, Clone, Copy)]
struct VisualStop {
    byte: usize,
    column: u16,
    row: u16,
}

pub(crate) fn layout_composer(
    text: &str,
    cursor: usize,
    lines: Vec<Line<'static>>,
    width: u16,
) -> ComposerLayout {
    let width = width.max(1) as usize;
    let mut wrapped: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut cursor_position = None;
    let mut stops = Vec::new();
    let mut byte_offset = 0;
    let line_count = lines.len();

    for (line_index, line) in lines.into_iter().enumerate() {
        if line_index > 0 {
            wrapped.push(Vec::new());
        }
        let mut column: usize = 0;
        let mut graphemes = Vec::new();
        for span in line.spans {
            for grapheme in span.styled_graphemes(Style::default()) {
                let symbol = grapheme.symbol.to_owned();
                let symbol_len = symbol.len();
                graphemes.push(ComposerGrapheme {
                    width: Line::from(symbol.as_str()).width(),
                    whitespace: symbol.chars().all(char::is_whitespace),
                    symbol,
                    style: grapheme.style,
                    byte_start: byte_offset,
                });
                byte_offset = byte_offset.saturating_add(symbol_len);
            }
        }

        let mut segment_start = 0;
        while segment_start < graphemes.len() {
            let mut segment_end = segment_start;
            while segment_end < graphemes.len() && graphemes[segment_end].whitespace {
                segment_end += 1;
            }
            while segment_end < graphemes.len() && !graphemes[segment_end].whitespace {
                segment_end += 1;
            }
            let segment_width = graphemes[segment_start..segment_end]
                .iter()
                .map(|grapheme| grapheme.width)
                .sum::<usize>();
            if column > 0 && column.saturating_add(segment_width) > width {
                wrapped.push(Vec::new());
                column = 0;
            }

            for grapheme in &graphemes[segment_start..segment_end] {
                if column > 0 && column.saturating_add(grapheme.width) > width {
                    wrapped.push(Vec::new());
                    column = 0;
                }
                let position = normalize_cursor(column, wrapped.len() - 1, width);
                stops.push(VisualStop {
                    byte: grapheme.byte_start,
                    column: position.0,
                    row: position.1,
                });
                let grapheme_end = grapheme.byte_start.saturating_add(grapheme.symbol.len());
                if cursor_position.is_none()
                    && cursor >= grapheme.byte_start
                    && cursor < grapheme_end
                {
                    cursor_position = Some(position);
                }
                wrapped
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(grapheme.symbol.clone(), grapheme.style));
                column = column.saturating_add(grapheme.width);
            }
            segment_start = segment_end;
        }

        let line_end = normalize_cursor(column, wrapped.len() - 1, width);
        stops.push(VisualStop {
            byte: byte_offset,
            column: line_end.0,
            row: line_end.1,
        });
        if cursor_position.is_none() && cursor == byte_offset {
            cursor_position = Some(line_end);
        }
        if line_index + 1 < line_count {
            byte_offset = byte_offset.saturating_add(1);
        }
    }

    let cursor = cursor_position.unwrap_or((0, 0));
    while wrapped.len() <= cursor.1 as usize {
        wrapped.push(Vec::new());
    }
    let lines = wrapped.into_iter().map(Line::from).collect();
    debug_assert_eq!(byte_offset, text.len());
    ComposerLayout {
        lines,
        cursor,
        stops,
    }
}

fn normalize_cursor(column: usize, row: usize, width: usize) -> (u16, u16) {
    (
        (column % width).min(u16::MAX as usize) as u16,
        row.saturating_add(column / width).min(u16::MAX as usize) as u16,
    )
}

#[derive(Debug, Clone, Default)]
pub struct ComposerDocument {
    content: ComposerContent,
    cursor: usize,
}

impl ComposerDocument {
    pub fn text(&self) -> &str {
        self.content.text()
    }

    pub fn content(&self) -> &ComposerContent {
        &self.content
    }

    pub fn attachments(&self) -> Vec<Attachment> {
        self.content.attachments()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_text(&mut self, text: &str) {
        self.content.text.insert_str(self.cursor, text);
        self.shift_tokens_at_or_after(self.cursor, text.len() as isize);
        self.cursor += text.len();
    }

    pub fn move_left(&mut self) {
        if let Some(token) = self.token_ending_at(self.cursor) {
            self.cursor = token.range.start;
        } else if let Some((index, _)) = self.content.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
        {
            self.cursor = index;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(token) = self.token_starting_at(self.cursor) {
            self.cursor = token.range.end;
        } else if let Some(grapheme) = self.content.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = self.content.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    pub fn move_end(&mut self) {
        self.cursor = self.content.text[self.cursor..]
            .find('\n')
            .map_or(self.content.text.len(), |index| self.cursor + index);
    }

    pub fn move_up(&mut self, width: u16) {
        self.move_vertical(width, -1);
    }

    pub fn move_down(&mut self, width: u16) {
        self.move_vertical(width, 1);
    }

    pub fn backspace(&mut self) {
        if let Some(token) = self.token_ending_at(self.cursor).cloned() {
            self.remove_token(token);
            return;
        }
        let old_cursor = self.cursor;
        self.move_left();
        if self.cursor != old_cursor {
            self.content.text.drain(self.cursor..old_cursor);
            self.shift_tokens_at_or_after(old_cursor, -((old_cursor - self.cursor) as isize));
        }
    }

    pub fn delete(&mut self) {
        if let Some(token) = self.token_starting_at(self.cursor).cloned() {
            self.remove_token(token);
            return;
        }
        if let Some(grapheme) = self.content.text[self.cursor..].graphemes(true).next() {
            let end = self.cursor + grapheme.len();
            self.content.text.drain(self.cursor..end);
            self.shift_tokens_at_or_after(end, -((end - self.cursor) as isize));
        }
    }

    pub fn active_file_query(&self) -> Option<(Range<usize>, &str)> {
        let start = self.content.text[..self.cursor].rfind('@')?;
        if self
            .content
            .tokens
            .iter()
            .any(|token| token.range.contains(&start))
        {
            return None;
        }
        let is_token_start = start == 0
            || self.content.text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let query = &self.content.text[start + 1..self.cursor];
        (is_token_start && !query.chars().any(char::is_whitespace))
            .then_some((start..self.cursor, query))
    }

    pub fn active_command_query(&self) -> Option<(Range<usize>, &str)> {
        let start = self.content.text[..self.cursor].rfind('/')?;
        if self
            .content
            .tokens
            .iter()
            .any(|token| token.range.contains(&start))
        {
            return None;
        }
        let is_token_start = start == 0
            || self.content.text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let query = &self.content.text[start + 1..self.cursor];
        (is_token_start && !query.chars().any(char::is_whitespace))
            .then_some((start..self.cursor, query))
    }

    pub fn insert_file_reference(&mut self, range: Range<usize>, path: String) {
        let replacement = format!("@{path}");
        self.replace_range(range.clone(), &replacement);
        self.content.tokens.push(InlineToken {
            range: range.start..range.start + replacement.len(),
            kind: InlineTokenKind::FileReference { path },
        });
        self.sort_tokens();
    }

    pub fn take_submission(&mut self) -> ComposerContent {
        self.cursor = 0;
        std::mem::take(&mut self.content)
    }

    pub fn clear(&mut self) {
        self.cursor = 0;
        self.content = ComposerContent::default();
    }

    fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        self.content.text.replace_range(range.clone(), replacement);
        let delta = replacement.len() as isize - (range.end - range.start) as isize;
        self.shift_tokens_at_or_after(range.end, delta);
        self.cursor = range.start + replacement.len();
    }

    fn remove_token(&mut self, token: InlineToken) {
        let range = token.range.clone();
        self.content.tokens.retain(|candidate| candidate != &token);
        self.content.text.replace_range(range.clone(), "");
        self.shift_tokens_at_or_after(range.end, -((range.end - range.start) as isize));
        self.cursor = range.start;
    }

    fn shift_tokens_at_or_after(&mut self, position: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        for token in &mut self.content.tokens {
            if token.range.start >= position {
                token.range.start = token.range.start.checked_add_signed(delta).unwrap();
                token.range.end = token.range.end.checked_add_signed(delta).unwrap();
            }
        }
    }

    fn token_starting_at(&self, cursor: usize) -> Option<&InlineToken> {
        self.content
            .tokens
            .iter()
            .find(|token| token.range.start == cursor)
    }

    fn token_ending_at(&self, cursor: usize) -> Option<&InlineToken> {
        self.content
            .tokens
            .iter()
            .find(|token| token.range.end == cursor)
    }

    fn snap_cursor_left(&mut self) {
        if let Some(token) = self
            .content
            .tokens
            .iter()
            .find(|token| token.range.contains(&self.cursor))
        {
            self.cursor = token.range.start;
        }
    }

    fn sort_tokens(&mut self) {
        self.content.tokens.sort_by_key(|token| token.range.start);
    }

    fn move_vertical(&mut self, width: u16, direction: i8) {
        let lines = self.content.lines(Style::default(), Style::default());
        let layout = layout_composer(&self.content.text, self.cursor, lines, width);
        let target_row = if direction < 0 {
            layout.cursor.1.checked_sub(1)
        } else {
            layout.cursor.1.checked_add(1)
        };
        let Some(target_row) = target_row else {
            return;
        };
        let target_column = layout.cursor.0;
        if let Some(stop) = layout
            .stops
            .iter()
            .filter(|stop| stop.row == target_row)
            .min_by_key(|stop| stop.column.abs_diff(target_column))
        {
            self.cursor = stop.byte;
            self.snap_cursor_left();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ComposerDocument;

    #[test]
    fn file_references_stay_between_surrounding_text() {
        let mut document = ComposerDocument::default();
        document.insert_text("inspect @src/ here");
        document.move_left();
        document.move_left();
        document.move_left();
        document.move_left();
        document.move_left();
        let (range, _) = document.active_file_query().unwrap();
        document.insert_file_reference(range, "src/app.rs".into());

        assert_eq!(document.text(), "inspect @src/app.rs here");
        assert_eq!(document.content().attachments()[0].path, "src/app.rs");
    }

    #[test]
    fn token_deletion_is_atomic() {
        let mut document = ComposerDocument::default();
        document.insert_text("@src/a");
        let (range, _) = document.active_file_query().unwrap();
        document.insert_file_reference(range, "src/app.rs".into());
        document.backspace();

        assert!(document.text().is_empty());
        assert!(document.content().attachments().is_empty());
    }

    #[test]
    fn backspace_removes_a_combining_grapheme_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("e\u{301}X");
        document.move_left();
        document.backspace();

        assert_eq!(document.text(), "X");
        assert_eq!(document.cursor(), 0);
    }

    #[test]
    fn backspace_removes_a_zwj_emoji_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("👨‍👩‍👧X");
        document.move_left();
        document.backspace();

        assert_eq!(document.text(), "X");
        assert_eq!(document.cursor(), 0);
    }

    #[test]
    fn delete_removes_a_combining_grapheme_atomically() {
        let mut document = ComposerDocument::default();
        document.insert_text("e\u{301}X");
        document.move_home();
        document.delete();

        assert_eq!(document.text(), "X");
        assert_eq!(document.cursor(), 0);
    }

    #[test]
    fn right_skips_a_zwj_emoji_as_one_grapheme() {
        let mut document = ComposerDocument::default();
        let emoji = "👨‍👩‍👧";
        document.insert_text(&format!("{emoji}X"));
        document.move_home();
        document.move_right();

        assert_eq!(document.cursor(), emoji.len());
    }

    #[test]
    fn delete_removes_a_token_at_the_cursor() {
        let mut document = ComposerDocument::default();
        document.insert_text("before @src/a after");
        document.move_left();
        document.move_left();
        document.move_left();
        document.move_left();
        document.move_left();
        document.move_left();
        let (range, _) = document.active_file_query().unwrap();
        document.insert_file_reference(range, "src/app.rs".into());
        document.move_left();
        document.delete();

        assert_eq!(document.text(), "before  after");
    }

    #[test]
    fn duplicate_file_references_are_sent_once() {
        let mut document = ComposerDocument::default();
        document.insert_text("@a");
        let (range, _) = document.active_file_query().unwrap();
        document.insert_file_reference(range, "src/app.rs".into());
        document.insert_text(" @a");
        let (range, _) = document.active_file_query().unwrap();
        document.insert_file_reference(range, "src/app.rs".into());

        assert_eq!(document.content().attachments().len(), 1);
    }

    #[test]
    fn email_addresses_are_not_file_queries() {
        let mut document = ComposerDocument::default();
        document.insert_text("me@example.com");

        assert!(document.active_file_query().is_none());
    }
}
