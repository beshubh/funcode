use ratatui::{
    style::{Color, Modifier, Style},
    symbols::border,
};

#[derive(Debug, Clone)]
pub struct Theme {
    pub outer_border: Style,
    pub panel_border: Style,
    pub title: Style,
    pub heading: Style,
    pub user: Style,
    pub agent: Style,
    pub status: Style,
    pub warning: Style,
    pub muted: Style,
    pub input: Style,
    pub border_set: border::Set<'static>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            outer_border: Style::default().fg(Color::Gray),
            panel_border: Style::default().fg(Color::DarkGray),
            title: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            heading: Style::default().add_modifier(Modifier::BOLD),
            user: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            agent: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            status: Style::default().fg(Color::Cyan),
            warning: Style::default().fg(Color::Yellow),
            muted: Style::default().fg(Color::DarkGray),
            input: Style::default(),
            border_set: border::ROUNDED,
        }
    }
}
