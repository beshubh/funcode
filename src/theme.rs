use ratatui::{
    style::{Color, Modifier, Style},
    symbols::border,
};

#[derive(Debug, Clone)]
pub struct Theme {
    pub panel_border: Style,
    pub logo_accent: Style,
    pub logo_neutral: Style,
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
            panel_border: Style::default().fg(Color::DarkGray),
            logo_accent: Style::default()
                .fg(Color::Rgb(181, 255, 0))
                .add_modifier(Modifier::BOLD),
            logo_neutral: Style::default()
                .fg(Color::Rgb(172, 180, 188))
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
