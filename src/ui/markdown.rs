use crate::theme::{Theme, ThemeRole};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use std::{str::FromStr, sync::OnceLock};
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
        let width = width.max(1);
        let mut builder = MarkdownBuilder::new(width);
        if let Some(unclosed) = unclosed_fence_start(source) {
            builder.parse(&source[..unclosed]);
            builder.append_literal_block(&source[unclosed..]);
        } else {
            builder.parse(source);
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

    pub(super) fn bytes(&self) -> usize {
        self.bytes
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
}

#[derive(Debug)]
struct CodeBlock {
    language: Option<String>,
    text: String,
}

struct MarkdownBuilder {
    width: usize,
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
    fn new(width: usize) -> Self {
        Self {
            width,
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
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS);
        for event in Parser::new_ext(source, options) {
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
            }
            | Tag::Image {
                link_type,
                dest_url,
                ..
            } => self.links.push(LinkState {
                destination: dest_url.into_string(),
                visible: String::new(),
                autolink: matches!(link_type, LinkType::Autolink | LinkType::Email),
            }),
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
        } else if !self.links.is_empty() {
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
        if !self.links.is_empty() {
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
        if !link.autolink && link.visible != link.destination {
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
        let (base, _) = self.block_prefixes();
        let label = code.language.as_deref().unwrap_or("code");
        self.logical.push(LogicalLine {
            first_prefix: base.clone(),
            continuation_prefix: base.clone(),
            content: vec![SemanticSpan::new(
                format!("┌─ {label}"),
                ThemeRole::MarkdownRule,
            )],
        });

        let code_prefix = [
            base.clone(),
            vec![SemanticSpan::new("│ ", ThemeRole::MarkdownRule)],
        ]
        .concat();
        let highlighted = highlight_code(&code.text, code.language.as_deref());
        for spans in highlighted {
            self.logical.push(LogicalLine {
                first_prefix: code_prefix.clone(),
                continuation_prefix: code_prefix.clone(),
                content: spans,
            });
        }
        self.logical.push(LogicalLine {
            first_prefix: base.clone(),
            continuation_prefix: base,
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
        self.logical
            .into_iter()
            .flat_map(|line| wrap_logical(line, self.width))
            .collect()
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

fn wrap_logical(line: LogicalLine, width: usize) -> Vec<SemanticLine> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = SemanticLine::default();
    let first_prefix = fit_prefix(line.first_prefix, width);
    let continuation_prefix = fit_prefix(line.continuation_prefix, width);
    let mut column = append_prefix(&mut row, &first_prefix);
    let mut row_has_content = false;

    for span in line.content {
        for grapheme in span.text.graphemes(true) {
            let source_width = UnicodeWidthStr::width(grapheme).max(1);
            let (grapheme, grapheme_width) = if source_width > width {
                ("\u{fffd}", 1)
            } else {
                (grapheme, source_width)
            };
            if !row_has_content && column > 0 && column.saturating_add(grapheme_width) > width {
                row = SemanticLine::default();
                column = 0;
            }
            if row_has_content && column.saturating_add(grapheme_width) > width {
                rows.push(row);
                row = SemanticLine::default();
                column = append_prefix(&mut row, &continuation_prefix);
            }
            row.push(SemanticSpan {
                text: grapheme.to_owned(),
                role: span.role,
                modifiers: span.modifiers,
            });
            column = column.saturating_add(grapheme_width);
            row_has_content = true;
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
    let mut open: Option<(usize, char, usize)> = None;
    let mut offset = 0usize;
    for line_with_ending in source.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if indent <= 3 {
            let candidate = &line[indent..];
            let character = candidate.chars().next();
            if matches!(character, Some('`' | '~')) {
                let character = character.expect("fence character");
                let length = candidate
                    .chars()
                    .take_while(|current| *current == character)
                    .count();
                if length >= 3 {
                    match open {
                        None => open = Some((offset, character, length)),
                        Some((_, open_character, open_length))
                            if open_character == character
                                && length >= open_length
                                && candidate[length..].trim().is_empty() =>
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
    open.map(|(start, _, _)| start)
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
