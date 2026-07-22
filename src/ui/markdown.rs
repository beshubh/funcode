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
    bytes: usize,
}

impl MarkdownLayout {
    pub(super) fn new(source: &str, width: usize) -> Self {
        Self::build(source, width, true)
    }

    pub(super) fn unhighlighted(source: &str, width: usize) -> Self {
        Self::build(source, width, false)
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
        Self { rows, bytes }
    }

    pub(super) fn height(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn literal(source: &str, width: usize) -> Self {
        let mut layout = Self {
            rows: vec![SemanticLine::default()],
            bytes: std::mem::size_of::<SemanticLine>(),
        };
        layout.append_literal(source, width);
        layout
    }

    pub(super) fn append_literal(&mut self, suffix: &str, width: usize) {
        let width = width.max(1);
        let mut column = self
            .rows
            .last()
            .map(|line| spans_width(&line.spans))
            .unwrap_or_default();
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
        self.bytes = self.bytes.saturating_add(suffix.len()).saturating_add(
            self.rows
                .len()
                .saturating_mul(std::mem::size_of::<SemanticLine>()),
        );
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn visually_eq(&self, other: &Self) -> bool {
        self.rows.len() == other.rows.len()
            && self
                .rows
                .iter()
                .zip(&other.rows)
                .all(|(left, right)| left.spans == right.spans)
    }

    pub(super) fn anchor_for_row(&self, row: usize) -> usize {
        self.rows.get(row).map_or(0, |line| line.anchor)
    }

    pub(super) fn row_for_anchor(&self, anchor: usize) -> usize {
        self.rows
            .partition_point(|line| line.anchor <= anchor)
            .saturating_sub(1)
    }

    pub(super) fn line(&self, index: usize, theme: &Theme) -> Option<Line<'static>> {
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
                .map(|span| span.text.graphemes(true).count())
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
            consumed = consumed.saturating_add(1);
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
    let mut offset = 0usize;
    for line_with_ending in source.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        if let Some(candidate) =
            container_fence_candidate(line, open.map(|(_, _, _, container)| container))
        {
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
                None => closing.list_indent.is_none(),
            }
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
        if spaces > 3 && !continuation_indent {
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
    use super::MarkdownLayout;
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
