use crate::{
    app::{App, AuthProvider, ResponseStatus, Screen, Suggestion, SuggestionKind},
    theme::Theme,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

const CHAT_MIN_WIDTH: u16 = 60;
const CHAT_MIN_HEIGHT: u16 = 20;
const HOME_MIN_WIDTH: u16 = 40;
const HOME_MIN_HEIGHT: u16 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiTarget {
    Thinking,
    Tools,
    AuthProvider,
    Suggestion(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiRegions {
    pub thinking: Option<Rect>,
    pub tools: Option<Rect>,
    pub auth_provider: Option<Rect>,
    pub suggestions: Vec<Rect>,
}

impl UiRegions {
    pub fn target_at(&self, column: u16, row: u16) -> Option<UiTarget> {
        let position = Position::new(column, row);
        if self
            .auth_provider
            .is_some_and(|area| area.contains(position))
        {
            Some(UiTarget::AuthProvider)
        } else if let Some((index, _)) = self
            .suggestions
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
        {
            Some(UiTarget::Suggestion(index))
        } else if self.thinking.is_some_and(|area| area.contains(position)) {
            Some(UiTarget::Thinking)
        } else if self.tools.is_some_and(|area| area.contains(position)) {
            Some(UiTarget::Tools)
        } else {
            None
        }
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App, theme: &Theme) -> UiRegions {
    let area = frame.area();
    let mut regions = match app.screen {
        Screen::Home if area.width < HOME_MIN_WIDTH || area.height < HOME_MIN_HEIGHT => {
            render_too_small(frame, area, HOME_MIN_WIDTH, HOME_MIN_HEIGHT, theme);
            UiRegions::default()
        }
        Screen::Chat if area.width < CHAT_MIN_WIDTH || area.height < CHAT_MIN_HEIGHT => {
            render_too_small(frame, area, CHAT_MIN_WIDTH, CHAT_MIN_HEIGHT, theme);
            UiRegions::default()
        }
        Screen::Home => {
            render_home(frame, area, app, theme);
            UiRegions::default()
        }
        Screen::Chat => render_chat(frame, area, app, theme),
    };

    if app.auth_dialog.is_some() && area.width >= HOME_MIN_WIDTH && area.height >= HOME_MIN_HEIGHT {
        regions = UiRegions {
            auth_provider: render_auth_dialog(frame, area, app, theme),
            ..UiRegions::default()
        };
    }
    regions
}

fn render_auth_dialog(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> Option<Rect> {
    let width = area.width.saturating_sub(4).min(64);
    let height = 12.min(area.height.saturating_sub(2));
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    frame.render_widget(panel_block(" Authenticate ", theme), dialog_area);

    let inner = dialog_area.inner(Margin::new(2, 1));
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(4),
        Constraint::Min(1),
    ])
    .split(inner);

    let dialog = app.auth_dialog.as_ref()?;
    match &dialog.phase {
        crate::app::AuthDialogPhase::Selecting => {
            frame.render_widget(
                Paragraph::new("Choose how to authenticate").style(theme.heading),
                rows[0],
            );
            let option = Paragraph::new(Text::from(vec![
                Line::styled(
                    format!(" {} ", AuthProvider::ChatGptSubscription.label()),
                    theme.status.add_modifier(Modifier::REVERSED),
                ),
                Line::styled(
                    format!(" {}", AuthProvider::ChatGptSubscription.description()),
                    theme.muted,
                ),
            ]))
            .block(panel_block(" OpenAI ", theme));
            frame.render_widget(option, rows[1]);
            frame.render_widget(
                Paragraph::new("↑/↓ select · Enter open · click · Esc close").style(theme.muted),
                rows[2],
            );
            Some(rows[1])
        }
        crate::app::AuthDialogPhase::Starting => {
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("Preparing browser sign-in…", theme.status),
                    Line::styled("Esc cancels", theme.muted),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            None
        }
        crate::app::AuthDialogPhase::WaitingForBrowser {
            authorization_url,
            browser_opened,
        } => {
            let heading = if *browser_opened {
                "Finish signing in through your browser"
            } else {
                "Open this URL in your browser"
            };
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled(heading, theme.status),
                    Line::styled(authorization_url.clone(), theme.muted),
                    Line::from(""),
                    Line::styled("Waiting for ChatGPT… · Esc cancel", theme.muted),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            None
        }
        crate::app::AuthDialogPhase::Succeeded { account_id } => {
            let detail = account_id.as_deref().map_or_else(
                || "Credentials saved".to_owned(),
                |id| format!("Connected to {id}"),
            );
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("✓ Authenticated with ChatGPT", theme.status),
                    Line::styled(detail, theme.muted),
                    Line::from(""),
                    Line::styled("Enter close", theme.muted),
                ])),
                inner,
            );
            None
        }
        crate::app::AuthDialogPhase::Failed { message } => {
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("ChatGPT sign-in failed", theme.warning),
                    Line::styled(message.clone(), theme.muted),
                    Line::from(""),
                    Line::styled("Enter choose provider · Esc close", theme.muted),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            None
        }
    }
}

fn render_home(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let inner = area.inner(Margin::new(2, 1));
    let rows = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Length(10),
        Constraint::Min(0),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(fun_logo(theme)).alignment(Alignment::Center),
        rows[0],
    );

    let columns = Layout::horizontal([Constraint::Length(44), Constraint::Min(0)]).split(rows[2]);
    render_home_help(frame, columns[0], app, theme);
}

fn fun_logo(theme: &Theme) -> Text<'static> {
    let accent = theme.logo_accent;
    let neutral = theme.logo_neutral;
    Text::from(vec![
        Line::from(vec![
            Span::styled("██████████", accent),
            Span::raw("                      "),
        ]),
        Line::from(vec![
            Span::styled("██████████", accent),
            Span::raw("                      "),
        ]),
        Line::from(vec![
            Span::styled("██████", accent),
            Span::raw("                          "),
        ]),
        Line::from(vec![
            Span::styled("██", neutral),
            Span::raw("         "),
            Span::styled("██", accent),
            Span::raw("    "),
            Span::styled("██", accent),
            Span::raw("   "),
            Span::styled("███████", neutral),
            Span::raw("   "),
        ]),
        Line::from(vec![
            Span::styled("██", neutral),
            Span::raw("         "),
            Span::styled("██", accent),
            Span::raw("    "),
            Span::styled("██", accent),
            Span::raw("   "),
            Span::styled("██", neutral),
            Span::raw("    "),
            Span::styled("██", neutral),
            Span::raw("  "),
        ]),
        Line::from(vec![
            Span::styled("██", neutral),
            Span::raw("         "),
            Span::styled("██", accent),
            Span::raw("    "),
            Span::styled("██", accent),
            Span::raw("   "),
            Span::styled("██", neutral),
            Span::raw("    "),
            Span::styled("██", neutral),
            Span::raw("  "),
        ]),
        Line::from(vec![
            Span::styled("██", neutral),
            Span::raw("          "),
            Span::styled("██████", accent),
            Span::raw("    "),
            Span::styled("██", neutral),
            Span::raw("    "),
            Span::styled("██", neutral),
            Span::raw("  "),
        ]),
    ])
}

fn render_home_help(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let mut lines = vec![Line::styled("Available commands", theme.heading)];
    lines.extend(app.available_commands().into_iter().map(|command| {
        Line::from(vec![
            Span::styled(format!("{:<10}", command.label), theme.status),
            Span::raw(command.description),
        ])
    }));
    lines.push(Line::from(""));
    lines.push(Line::styled("Enter start  ·  Ctrl+C quit", theme.muted));
    let help = Text::from(lines);
    frame.render_widget(
        Paragraph::new(help)
            .wrap(Wrap { trim: false })
            .block(panel_block("Help", theme)),
        area,
    );
}

fn render_chat(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> UiRegions {
    let inner = area.inner(Margin::new(1, 1));
    let contextual_height = contextual_widgets_height(app);
    let suggestions = app.suggestions();

    let rows = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(contextual_height),
        Constraint::Length(1),
        Constraint::Length(5),
    ])
    .split(inner);

    render_messages(frame, rows[0], app, theme);
    let mut regions = render_contextual_widgets(frame, rows[1], app, theme);
    render_activity(frame, rows[2], app, theme);
    let composer_area = rows[3];
    let suggestion_height =
        (suggestions.len() as u16 + 2).min(composer_area.y.saturating_sub(inner.y));
    let suggestion_area = Rect::new(
        composer_area.x,
        composer_area.y.saturating_sub(suggestion_height),
        composer_area.width,
        suggestion_height,
    );
    regions.suggestions = render_suggestions(frame, suggestion_area, app, &suggestions, theme);
    render_composer(frame, composer_area, app, theme);
    regions
}

fn render_suggestions(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    suggestions: &[Suggestion],
    theme: &Theme,
) -> Vec<Rect> {
    if suggestions.is_empty() || area.is_empty() {
        return Vec::new();
    }

    let title = match suggestions[0].kind {
        SuggestionKind::Command => " Commands ",
        SuggestionKind::File => " Files ",
    };
    let block = panel_block(title, theme);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut regions = Vec::with_capacity(suggestions.len());
    for (index, suggestion) in suggestions.iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        let line = Line::from(vec![
            Span::styled(format!(" {}", suggestion.label), theme.status),
            Span::styled(format!("  {}", suggestion.description), theme.muted),
        ]);
        let style = if index == app.selected_suggestion() {
            ratatui::style::Style::default().add_modifier(Modifier::REVERSED)
        } else {
            ratatui::style::Style::default()
        };
        frame.render_widget(Paragraph::new(line).style(style), row);
        regions.push(row);
    }
    regions
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let content_area = area.inner(Margin::new(2, 0));
    let mut text = conversation_text(app, theme);
    if !app.follow_output {
        text.lines
            .insert(0, Line::styled("↑ End to follow", theme.muted));
    }
    let line_count = wrapped_line_count(&text, content_area.width.max(1));
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let viewport_height = content_area.height as usize;
    let maximum_top = line_count.saturating_sub(viewport_height);
    let from_bottom = app.scroll_from_bottom.min(maximum_top);
    let top = maximum_top
        .saturating_sub(from_bottom)
        .min(u16::MAX as usize) as u16;

    frame.render_widget(paragraph.scroll((top, 0)), content_area);
}

fn conversation_text(app: &App, theme: &Theme) -> Text<'static> {
    if app.turns.is_empty() {
        return Text::from(Line::styled(
            "No messages yet. Type something below to begin.",
            theme.muted,
        ));
    }

    let mut lines = Vec::new();
    for turn in &app.turns {
        lines.push(Line::styled("you", theme.user));
        lines.extend(
            turn.prompt
                .split('\n')
                .map(|line| Line::from(line.to_owned())),
        );
        lines.push(Line::styled("funcode", theme.agent));

        match &turn.response_status {
            ResponseStatus::Queued => lines.push(Line::styled("queued…", theme.muted)),
            ResponseStatus::Thinking => lines.push(Line::styled("thinking…", theme.status)),
            ResponseStatus::Streaming | ResponseStatus::Completed => {
                lines.extend(response_lines(&turn.response));
            }
            ResponseStatus::Interrupted => {
                lines.extend(response_lines(&turn.response));
                lines.push(Line::styled("[interrupted]", theme.warning));
            }
            ResponseStatus::Failed(message) => {
                lines.push(Line::styled(format!("[failed: {message}]"), theme.warning));
            }
        }
        lines.push(Line::from(""));
    }
    Text::from(lines)
}

fn response_lines(response: &str) -> Vec<Line<'static>> {
    if response.is_empty() {
        vec![Line::from("")]
    } else {
        response
            .split('\n')
            .map(|line| Line::from(line.to_owned()))
            .collect()
    }
}

fn contextual_widgets_height(app: &App) -> u16 {
    let thinking = app
        .is_thinking()
        .then_some(if app.thinking_expanded { 5 } else { 3 });
    let tools = app
        .active_tool
        .as_ref()
        .map(|_| if app.tools_expanded { 5 } else { 3 });

    match (thinking, tools) {
        (Some(thinking), Some(tools)) => thinking + tools + 1,
        (Some(thinking), None) => thinking,
        (None, Some(tools)) => tools,
        (None, None) => 0,
    }
}

fn render_contextual_widgets(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
) -> UiRegions {
    let mut regions = UiRegions::default();
    if area.is_empty() {
        return regions;
    }

    let width = area.width.min(40);
    let mut row = area.y;

    if app.is_thinking() {
        let height = if app.thinking_expanded { 5 } else { 3 };
        let thinking_area = Rect::new(area.x, row, width, height);
        render_thinking(frame, thinking_area, app, theme);
        regions.thinking = Some(thinking_area);
        row = row.saturating_add(height + 1);
    }

    if let Some(tool) = &app.active_tool {
        let height = if app.tools_expanded { 5 } else { 3 };
        let tools_area = Rect::new(area.x, row, width, height);
        render_tools(frame, tools_area, app, tool, theme);
        regions.tools = Some(tools_area);
    }

    regions
}

fn render_thinking(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let frames = ["|", "/", "-", "\\"];
    let spinner = frames[(app.animation_frame / 2) % frames.len()];
    let title = if app.thinking_expanded {
        "Thinking · click to collapse"
    } else {
        "Thinking · click to expand"
    };
    let content = if app.thinking_expanded {
        Text::from(vec![
            Line::styled(
                format!("Working through the request {spinner}"),
                theme.status,
            ),
            Line::styled("Preparing a response summary…", theme.muted),
        ])
    } else {
        Text::from(Line::styled(format!("working {spinner}"), theme.status))
    };

    frame.render_widget(
        Paragraph::new(content).block(panel_block(title, theme)),
        area,
    );
}

fn render_tools(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    tool: &crate::app::ToolActivity,
    theme: &Theme,
) {
    let title = if app.tools_expanded {
        "Tools · click to collapse"
    } else {
        "Tools · click to expand"
    };
    let content = if app.tools_expanded {
        Text::from(vec![
            Line::styled(tool.name.clone(), theme.status),
            Line::styled(tool.summary.clone(), theme.muted),
        ])
    } else {
        Text::from(Line::styled(tool.name.clone(), theme.status))
    };

    frame.render_widget(
        Paragraph::new(content).block(panel_block(title, theme)),
        area,
    );
}

fn render_activity(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let text = if app.active_request.is_some() {
        let dots = ".".repeat((app.animation_frame / 5) % 4);
        format!(" Waiting{dots}")
    } else if app
        .turns
        .iter()
        .any(|turn| turn.response_status == ResponseStatus::Queued)
    {
        " Queued…".to_owned()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(text).style(theme.status), area);
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let block = panel_block("Enter send · Shift+Enter new line", theme);
    let inner = block.inner(area);
    let (cursor_column, cursor_row) = composer_cursor(
        app.composer.text(),
        app.composer.cursor(),
        inner.width.max(1),
    );
    let vertical_scroll = cursor_row.saturating_sub(inner.height.saturating_sub(1));
    let content = if app.composer.text().is_empty() {
        Text::from(Line::styled("Type something…", theme.muted))
    } else {
        Text::styled(app.composer.text().to_owned(), theme.input)
    };

    frame.render_widget(
        Paragraph::new(content)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((vertical_scroll, 0)),
        area,
    );
    if app.auth_dialog.is_none() {
        frame.set_cursor_position((
            inner.x.saturating_add(cursor_column),
            inner
                .y
                .saturating_add(cursor_row.saturating_sub(vertical_scroll)),
        ));
    }
}

fn composer_cursor(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = width.max(1) as usize;
    let prefix = &text[..cursor];
    let parts: Vec<_> = prefix.split('\n').collect();
    let mut row = 0usize;

    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        let display_width = Line::from((*part).to_owned()).width();
        row += display_width.div_ceil(width).max(1);
    }

    let final_width = Line::from(parts.last().copied().unwrap_or_default().to_owned()).width();
    row += final_width / width;
    (
        (final_width % width) as u16,
        row.min(u16::MAX as usize) as u16,
    )
}

fn wrapped_line_count(text: &Text<'_>, width: u16) -> usize {
    let width = width.max(1) as usize;
    text.lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}

fn panel_block<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    Block::bordered()
        .title(title)
        .border_set(theme.border_set)
        .border_style(theme.panel_border)
}

fn render_too_small(
    frame: &mut Frame<'_>,
    area: Rect,
    minimum_width: u16,
    minimum_height: u16,
    theme: &Theme,
) {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(2),
        Constraint::Fill(1),
    ])
    .split(area);
    let message = format!(
        "Terminal too small\nResize to at least {minimum_width}x{minimum_height} (now {}x{})",
        area.width, area.height
    );
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(theme.warning),
        vertical[1],
    );
}

#[cfg(test)]
mod tests {
    use super::{UiRegions, UiTarget, render};
    use crate::{
        agent::AgentEvent,
        app::{App, Screen, Turn},
        theme::Theme,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Position, style::Color};

    fn render_to_string(app: &App, width: u16, height: u16) -> (String, bool, UiRegions, String) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();
        terminal
            .draw(|frame| regions = render(frame, app, &Theme::default()))
            .unwrap();
        (
            terminal.backend().to_string(),
            terminal.backend().cursor_visible(),
            regions,
            terminal
                .backend()
                .buffer()
                .cell(Position::new(0, 0))
                .unwrap()
                .symbol()
                .to_owned(),
        )
    }

    #[test]
    fn home_screen_has_no_app_border_and_one_compact_help_widget() {
        let (screen, cursor_visible, _, top_left) = render_to_string(&App::new(), 100, 30);

        assert!(screen.contains("██████████"));
        assert!(screen.contains("/auth"));
        assert!(screen.contains("/exit"));
        assert!(!screen.contains("/sessions"));
        assert!(!screen.contains("/models"));
        assert!(!screen.contains("Model: not connected"));
        assert_eq!(top_left, " ");
        assert!(!cursor_visible);
    }

    #[test]
    fn idle_chat_hides_thinking_and_tools_and_has_no_app_border() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        let (screen, cursor_visible, regions, top_left) = render_to_string(&app, 100, 30);

        assert!(!screen.contains("Agent messages"));
        assert!(screen.contains("No messages yet"));
        assert!(!screen.contains("Thinking"));
        assert!(!screen.contains("Tools"));
        assert!(screen.contains("Type something"));
        assert!(regions.thinking.is_none());
        assert!(regions.tools.is_none());
        assert_eq!(top_left, " ");
        assert!(cursor_visible);
    }

    #[test]
    fn file_suggestions_render_above_the_composer_with_clickable_rows() {
        let mut app = App::with_files(["src/app.rs", "src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("inspect @src/");

        let (screen, _, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Files"));
        assert!(screen.contains("src/app.rs"));
        assert!(screen.contains("src/main.rs"));
        assert_eq!(regions.suggestions.len(), 2);
        let second = regions.suggestions[1];
        assert_eq!(
            regions.target_at(second.x, second.y),
            Some(UiTarget::Suggestion(1))
        );
    }

    #[test]
    fn selected_suggestion_has_visible_reverse_highlighting() {
        let mut app = App::with_files(["src/app.rs", "src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src/");
        app.set_suggestion_selection(1);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();

        terminal
            .draw(|frame| regions = render(frame, &app, &Theme::default()))
            .unwrap();

        let selected = regions.suggestions[1];
        let style = terminal
            .backend()
            .buffer()
            .cell(Position::new(selected.x, selected.y))
            .unwrap()
            .style();
        assert!(
            style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn suggestion_popup_keeps_the_composer_visible_in_a_busy_small_terminal() {
        let mut app = App::with_files(["src/app.rs"]);
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(5, "prompt".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 5 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 5,
            name: "read_file".into(),
            summary: "Reading".into(),
        });
        app.toggle_thinking();
        app.toggle_tools();
        app.composer.insert_text("@src/");

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 60, 20);

        assert!(screen.contains("src/app.rs"));
        assert!(screen.contains("@src/"));
        assert!(cursor_visible);
        assert_eq!(regions.suggestions.len(), 1);
    }

    #[test]
    fn auth_dialog_lists_chatgpt_subscription_as_a_clickable_provider() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(1, "hello".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.open_auth_dialog();

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Authenticate"));
        assert!(screen.contains("ChatGPT subscription"));
        assert!(screen.contains("browser"));
        assert!(regions.auth_provider.is_some());
        assert!(regions.thinking.is_none());
        assert!(regions.tools.is_none());
        assert!(!cursor_visible);
    }

    #[test]
    fn transcript_content_has_balanced_horizontal_padding() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(1, "x".repeat(54)));
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &Theme::default());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer.cell(Position::new(1, 1)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell(Position::new(2, 1)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell(Position::new(3, 1)).unwrap().symbol(), "y");
        assert_eq!(buffer.cell(Position::new(56, 2)).unwrap().symbol(), "x");
        assert_eq!(buffer.cell(Position::new(57, 2)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell(Position::new(58, 2)).unwrap().symbol(), " ");
    }

    #[test]
    fn thinking_is_a_clickable_collapsed_widget_only_while_thinking() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(1, "hello".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });

        let (screen, _, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Thinking"));
        assert!(!screen.contains("Working through the request"));
        assert!(!screen.contains("Tools"));
        assert!(screen.contains("Waiting"));
        let thinking = regions.thinking.unwrap();
        assert_eq!(
            regions.target_at(thinking.x, thinking.y),
            Some(UiTarget::Thinking)
        );

        app.toggle_thinking();
        let (expanded, _, _, _) = render_to_string(&app, 100, 30);
        assert!(expanded.contains("Working through the request"));

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 1,
            text: "reply".into(),
        });
        let (streaming, _, regions, _) = render_to_string(&app, 100, 30);
        assert!(!streaming.contains("Thinking"));
        assert!(regions.thinking.is_none());
    }

    #[test]
    fn tools_widget_only_appears_for_an_active_tool_and_expands_on_click() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(3, "inspect".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 3 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            name: "read_file".into(),
            summary: "Reading src/main.rs".into(),
        });

        let (collapsed, _, regions, _) = render_to_string(&app, 100, 30);
        assert!(collapsed.contains("Tools"));
        assert!(collapsed.contains("read_file"));
        assert!(!collapsed.contains("Reading src/main.rs"));
        let tools = regions.tools.unwrap();
        assert_eq!(regions.target_at(tools.x, tools.y), Some(UiTarget::Tools));

        app.toggle_tools();
        let (expanded, _, _, _) = render_to_string(&app, 100, 30);
        assert!(expanded.contains("Reading src/main.rs"));
    }

    #[test]
    fn a_small_chat_terminal_shows_a_resize_message() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        let (screen, _, _, _) = render_to_string(&app, 40, 10);

        assert!(screen.contains("Terminal too small"));
        assert!(screen.contains("60x20"));
    }

    #[test]
    fn default_rendering_does_not_force_a_white_background() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &App::new(), &theme);
            })
            .unwrap();

        let cells = terminal.backend().buffer().content();
        assert!(
            cells
                .iter()
                .all(|cell| cell.style().bg == Some(Color::Reset))
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.style().fg == theme.logo_accent.fg)
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.style().fg == theme.logo_neutral.fg)
        );
        assert!(
            cells
                .iter()
                .all(|cell| cell.style().fg != Some(Color::Black))
        );
    }
}
