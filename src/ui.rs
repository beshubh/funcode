use crate::{
    app::{App, ResponseStatus, Screen},
    theme::Theme,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};

const CHAT_MIN_WIDTH: u16 = 60;
const CHAT_MIN_HEIGHT: u16 = 20;
const HOME_MIN_WIDTH: u16 = 40;
const HOME_MIN_HEIGHT: u16 = 16;

pub fn render(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let area = frame.area();
    match app.screen {
        Screen::Home if area.width < HOME_MIN_WIDTH || area.height < HOME_MIN_HEIGHT => {
            render_too_small(frame, area, HOME_MIN_WIDTH, HOME_MIN_HEIGHT, theme);
        }
        Screen::Chat if area.width < CHAT_MIN_WIDTH || area.height < CHAT_MIN_HEIGHT => {
            render_too_small(frame, area, CHAT_MIN_WIDTH, CHAT_MIN_HEIGHT, theme);
        }
        Screen::Home => render_home(frame, area, theme),
        Screen::Chat => render_chat(frame, area, app, theme),
    }
}

fn render_home(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let outer = Block::bordered()
        .border_set(theme.border_set)
        .border_style(theme.outer_border);
    let inner = outer.inner(area).inner(Margin::new(2, 1));
    frame.render_widget(outer, area);

    let rows = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).split(inner);
    frame.render_widget(
        Paragraph::new(Line::styled("funcode", theme.title))
            .alignment(Alignment::Center)
            .block(Block::new().padding(ratatui::widgets::Padding::top(1))),
        rows[0],
    );

    if area.width >= 90 {
        let panels = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(3)
            .split(rows[1]);
        render_home_help(frame, panels[0], theme);
        render_home_status(frame, panels[1], theme);
    } else {
        let panels = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(1)
            .split(rows[1]);
        render_home_help(frame, panels[0], theme);
        render_home_status(frame, panels[1], theme);
    }
}

fn render_home_help(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let help = Text::from(vec![
        Line::styled("Commands", theme.heading),
        Line::from(""),
        Line::from(vec![
            Span::styled("/sessions", theme.status),
            Span::raw("  list sessions"),
        ]),
        Line::from(vec![
            Span::styled("/models", theme.status),
            Span::raw("    list models"),
        ]),
        Line::from(""),
        Line::styled("Enter start  ·  Ctrl+C quit", theme.muted),
    ]);
    frame.render_widget(
        Paragraph::new(help)
            .wrap(Wrap { trim: false })
            .block(panel_block("Help", theme)),
        area,
    );
}

fn render_home_status(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let status = Text::from(vec![
        Line::styled("Phase 1", theme.heading),
        Line::from(""),
        Line::from(vec![Span::styled("Mode: ", theme.muted), Span::raw("demo")]),
        Line::from(vec![
            Span::styled("Model: ", theme.muted),
            Span::raw("not connected"),
        ]),
        Line::from(vec![
            Span::styled("Session: ", theme.muted),
            Span::raw("new"),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(status).block(panel_block("Status", theme)),
        area,
    );
}

fn render_chat(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let outer = Block::bordered()
        .border_set(theme.border_set)
        .border_style(theme.outer_border);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let rows = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Length(5),
    ])
    .split(inner);

    render_messages(frame, rows[0], app, theme);
    render_agent_status(frame, rows[1], app, theme);
    render_activity(frame, rows[2], app, theme);
    render_composer(frame, rows[3], app, theme);
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let title = if app.follow_output {
        "Agent messages".to_owned()
    } else {
        "Agent messages · End to follow".to_owned()
    };
    let block = panel_block(title, theme);
    let inner = block.inner(area);
    let text = conversation_text(app, theme);
    let line_count = wrapped_line_count(&text, inner.width.max(1));
    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    let viewport_height = inner.height as usize;
    let maximum_top = line_count.saturating_sub(viewport_height);
    let from_bottom = app.scroll_from_bottom.min(maximum_top);
    let top = maximum_top
        .saturating_sub(from_bottom)
        .min(u16::MAX as usize) as u16;

    frame.render_widget(paragraph.scroll((top, 0)), area);
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

fn render_agent_status(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let columns = Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).split(area);
    let boxes = Layout::vertical([Constraint::Length(3), Constraint::Length(3)])
        .spacing(1)
        .split(columns[0]);

    let thinking = match app.active_request.and_then(|id| {
        app.turns
            .iter()
            .find(|turn| turn.request_id == id)
            .map(|turn| &turn.response_status)
    }) {
        Some(ResponseStatus::Thinking) => {
            let frames = ["|", "/", "-", "\\"];
            format!(
                "Thinking {}",
                frames[(app.animation_frame / 2) % frames.len()]
            )
        }
        Some(ResponseStatus::Streaming) => "Thinking > done".to_owned(),
        _ => "Thinking > idle".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(thinking)
            .style(theme.status)
            .block(panel_block("Thinking", theme)),
        boxes[0],
    );
    frame.render_widget(
        Paragraph::new("no tools used")
            .style(theme.muted)
            .block(panel_block("Tools", theme)),
        boxes[1],
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
    let block = panel_block(
        "Input · Enter send · Shift+Enter newline · /exit quit",
        theme,
    );
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
    frame.set_cursor_position((
        inner.x.saturating_add(cursor_column),
        inner
            .y
            .saturating_add(cursor_row.saturating_sub(vertical_scroll)),
    ));
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
    use super::render;
    use crate::{
        agent::AgentEvent,
        app::{App, Screen, Turn},
        theme::Theme,
    };
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    fn render_to_string(app: &App, width: u16, height: u16) -> (String, bool) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, &Theme::default()))
            .unwrap();
        (
            terminal.backend().to_string(),
            terminal.backend().cursor_visible(),
        )
    }

    #[test]
    fn home_screen_renders_funcode_help_and_demo_status() {
        let (screen, cursor_visible) = render_to_string(&App::new(), 100, 30);

        assert!(screen.contains("funcode"));
        assert!(screen.contains("/sessions"));
        assert!(screen.contains("Model: not connected"));
        assert!(!cursor_visible);
    }

    #[test]
    fn chat_screen_renders_wireframe_sections_and_input_cursor() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(Turn::queued(1, "hello".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });

        let (screen, cursor_visible) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Agent messages"));
        assert!(screen.contains("Thinking"));
        assert!(screen.contains("Tools"));
        assert!(screen.contains("Type something"));
        assert!(screen.contains("Waiting"));
        assert!(cursor_visible);
    }

    #[test]
    fn a_small_chat_terminal_shows_a_resize_message() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        let (screen, _) = render_to_string(&app, 40, 10);

        assert!(screen.contains("Terminal too small"));
        assert!(screen.contains("60x20"));
    }

    #[test]
    fn default_rendering_does_not_force_a_white_background() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &App::new(), &Theme::default()))
            .unwrap();

        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.style().bg != Some(Color::White))
        );
    }
}
