mod config;

pub use config::{ThemeConfig, ThemeConfigLoad, ThemeConfigStore};

use ratatui::{
    style::{Color, Modifier, Style},
    symbols::border,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeId {
    #[default]
    Terminal,
    FunDark,
    Midnight,
    Paper,
}

impl ThemeId {
    pub const ALL: [Self; 4] = [Self::Terminal, Self::FunDark, Self::Midnight, Self::Paper];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::FunDark => "fun-dark",
            Self::Midnight => "midnight",
            Self::Paper => "paper",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::FunDark => "Fun Dark",
            Self::Midnight => "Midnight",
            Self::Paper => "Paper",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeAppearance {
    Terminal,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeRole {
    Surface,
    Text,
    MutedText,
    Border,
    Accent,
    Warning,
    User,
    Agent,
    PlanMode,
    BuildMode,
}

#[derive(Debug, Clone, Copy)]
struct ThemeDefinition {
    id: ThemeId,
    appearance: ThemeAppearance,
    background: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    accent: Color,
    warning: Color,
    user: Color,
    agent: Color,
    plan: Color,
    build: Color,
}

const TERMINAL: ThemeDefinition = ThemeDefinition {
    id: ThemeId::Terminal,
    appearance: ThemeAppearance::Terminal,
    background: Color::Reset,
    foreground: Color::Reset,
    muted: Color::DarkGray,
    border: Color::DarkGray,
    accent: Color::Cyan,
    warning: Color::Yellow,
    user: Color::Yellow,
    agent: Color::Cyan,
    plan: Color::Rgb(240, 136, 62),
    build: Color::Rgb(63, 185, 80),
};

const FUN_DARK: ThemeDefinition = ThemeDefinition {
    id: ThemeId::FunDark,
    appearance: ThemeAppearance::Dark,
    background: Color::Rgb(13, 17, 23),
    foreground: Color::Rgb(230, 237, 243),
    muted: Color::Rgb(139, 148, 158),
    border: Color::Rgb(48, 54, 61),
    accent: Color::Rgb(181, 255, 0),
    warning: Color::Rgb(210, 153, 34),
    user: Color::Rgb(255, 166, 87),
    agent: Color::Rgb(181, 255, 0),
    plan: Color::Rgb(240, 136, 62),
    build: Color::Rgb(63, 185, 80),
};

const MIDNIGHT: ThemeDefinition = ThemeDefinition {
    id: ThemeId::Midnight,
    appearance: ThemeAppearance::Dark,
    background: Color::Rgb(11, 16, 32),
    foreground: Color::Rgb(229, 231, 235),
    muted: Color::Rgb(148, 163, 184),
    border: Color::Rgb(51, 65, 85),
    accent: Color::Rgb(96, 165, 250),
    warning: Color::Rgb(251, 191, 36),
    user: Color::Rgb(244, 114, 182),
    agent: Color::Rgb(96, 165, 250),
    plan: Color::Rgb(240, 136, 62),
    build: Color::Rgb(63, 185, 80),
};

const PAPER: ThemeDefinition = ThemeDefinition {
    id: ThemeId::Paper,
    appearance: ThemeAppearance::Light,
    background: Color::Rgb(250, 250, 249),
    foreground: Color::Rgb(28, 25, 23),
    muted: Color::Rgb(120, 113, 108),
    border: Color::Rgb(214, 211, 209),
    accent: Color::Rgb(124, 58, 237),
    warning: Color::Rgb(161, 98, 7),
    user: Color::Rgb(190, 24, 93),
    agent: Color::Rgb(109, 40, 217),
    plan: Color::Rgb(188, 76, 0),
    build: Color::Rgb(26, 127, 55),
};

#[derive(Debug, Clone)]
pub struct Theme {
    id: ThemeId,
    appearance: ThemeAppearance,
    styles: [Style; 10],
    border_set: border::Set<'static>,
}

impl Theme {
    pub fn resolve(id: ThemeId) -> Self {
        let definition = match id {
            ThemeId::Terminal => TERMINAL,
            ThemeId::FunDark => FUN_DARK,
            ThemeId::Midnight => MIDNIGHT,
            ThemeId::Paper => PAPER,
        };
        debug_assert_eq!(definition.id, id);
        let foreground = |color| Style::default().fg(color);
        let styles = [
            Style::default()
                .fg(definition.foreground)
                .bg(definition.background),
            foreground(definition.foreground),
            foreground(definition.muted),
            foreground(definition.border),
            foreground(definition.accent),
            foreground(definition.warning),
            foreground(definition.user),
            foreground(definition.agent),
            foreground(definition.plan),
            foreground(definition.build),
        ];
        Self {
            id,
            appearance: definition.appearance,
            styles,
            border_set: border::ROUNDED,
        }
    }

    pub const fn id(&self) -> ThemeId {
        self.id
    }

    pub const fn appearance(&self) -> ThemeAppearance {
        self.appearance
    }

    pub fn style(&self, role: ThemeRole) -> Style {
        self.styles[role as usize]
    }

    pub const fn border_set(&self) -> border::Set<'static> {
        self.border_set
    }

    pub fn accent_badge(&self) -> Style {
        self.style(ThemeRole::Accent)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::resolve(ThemeId::Terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::{Theme, ThemeAppearance, ThemeId, ThemeRole};
    use ratatui::style::Color;

    #[test]
    fn bundled_themes_resolve_stable_principal_colors() {
        assert_eq!(ThemeId::ALL.len(), 4);
        assert_eq!(ThemeId::Terminal.as_str(), "terminal");
        assert_eq!(
            Theme::resolve(ThemeId::Terminal).appearance(),
            ThemeAppearance::Terminal
        );
        assert_eq!(
            Theme::resolve(ThemeId::Terminal)
                .style(ThemeRole::Surface)
                .bg,
            Some(Color::Reset)
        );
        assert_eq!(
            Theme::resolve(ThemeId::Terminal)
                .style(ThemeRole::Accent)
                .fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            Theme::resolve(ThemeId::FunDark)
                .style(ThemeRole::Surface)
                .bg,
            Some(Color::Rgb(13, 17, 23))
        );
        assert_eq!(
            Theme::resolve(ThemeId::Midnight)
                .style(ThemeRole::Accent)
                .fg,
            Some(Color::Rgb(96, 165, 250))
        );
        assert_eq!(
            Theme::resolve(ThemeId::Paper)
                .style(ThemeRole::BuildMode)
                .fg,
            Some(Color::Rgb(26, 127, 55))
        );
    }
}
