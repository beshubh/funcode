pub mod config;

pub(crate) use config::{
    ThemeConfigEvent, ThemeConfigLoad, ThemeConfigStore, ThemeConfigTaskRunner,
};

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
    DiffAdded,
    DiffRemoved,
    MarkdownHeading1,
    MarkdownHeading2,
    MarkdownHeading3,
    MarkdownHeading4,
    MarkdownHeading5,
    MarkdownHeading6,
    MarkdownStrong,
    MarkdownEmphasis,
    MarkdownStrikethrough,
    MarkdownInlineCode,
    MarkdownLink,
    MarkdownQuote,
    MarkdownRule,
    CodeText,
    CodeKeyword,
    CodeString,
    CodeComment,
    CodeConstant,
    CodeType,
    CodeFunction,
    CodeOperator,
}

const THEME_ROLE_COUNT: usize = ThemeRole::CodeOperator as usize + 1;

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
    diff_added: Color,
    diff_removed: Color,
    markdown_accents: [Color; 6],
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
    diff_added: Color::Green,
    diff_removed: Color::Red,
    markdown_accents: [
        Color::Cyan,
        Color::Blue,
        Color::Magenta,
        Color::Yellow,
        Color::Green,
        Color::DarkGray,
    ],
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
    diff_added: Color::Rgb(63, 185, 80),
    diff_removed: Color::Rgb(248, 81, 73),
    markdown_accents: [
        Color::Rgb(181, 255, 0),
        Color::Rgb(88, 166, 255),
        Color::Rgb(210, 168, 255),
        Color::Rgb(255, 166, 87),
        Color::Rgb(126, 231, 135),
        Color::Rgb(255, 122, 178),
    ],
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
    diff_added: Color::Rgb(74, 222, 128),
    diff_removed: Color::Rgb(248, 113, 113),
    markdown_accents: [
        Color::Rgb(96, 165, 250),
        Color::Rgb(167, 139, 250),
        Color::Rgb(244, 114, 182),
        Color::Rgb(251, 191, 36),
        Color::Rgb(45, 212, 191),
        Color::Rgb(148, 163, 184),
    ],
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
    diff_added: Color::Rgb(26, 127, 55),
    diff_removed: Color::Rgb(185, 28, 28),
    markdown_accents: [
        Color::Rgb(109, 40, 217),
        Color::Rgb(29, 78, 216),
        Color::Rgb(190, 24, 93),
        Color::Rgb(180, 83, 9),
        Color::Rgb(4, 120, 87),
        Color::Rgb(87, 83, 78),
    ],
};

#[derive(Debug, Clone)]
pub struct Theme {
    id: ThemeId,
    appearance: ThemeAppearance,
    styles: [Style; THEME_ROLE_COUNT],
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
        let [a1, a2, a3, a4, a5, a6] = definition.markdown_accents;
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
            foreground(definition.diff_added),
            foreground(definition.diff_removed),
            foreground(a1).add_modifier(Modifier::BOLD),
            foreground(a2).add_modifier(Modifier::BOLD),
            foreground(a3),
            foreground(a4),
            foreground(a5),
            foreground(a6),
            foreground(a4).add_modifier(Modifier::BOLD),
            foreground(a6).add_modifier(Modifier::ITALIC),
            foreground(definition.muted).add_modifier(Modifier::CROSSED_OUT),
            foreground(a2),
            foreground(a2).add_modifier(Modifier::UNDERLINED),
            foreground(a3),
            foreground(definition.muted),
            foreground(definition.foreground),
            foreground(a6).add_modifier(Modifier::BOLD),
            foreground(a5),
            foreground(definition.muted).add_modifier(Modifier::ITALIC),
            foreground(a4),
            foreground(a1),
            foreground(a2),
            foreground(a3),
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
    use std::collections::HashSet;

    #[test]
    fn bundled_themes_resolve_stable_principal_colors() {
        let ids: HashSet<_> = ThemeId::ALL.iter().map(|id| id.as_str()).collect();
        assert_eq!(ids.len(), ThemeId::ALL.len());

        let cases = [
            (
                ThemeId::Terminal,
                ThemeAppearance::Terminal,
                Color::Reset,
                Color::Reset,
                Color::Cyan,
                Color::Rgb(240, 136, 62),
                Color::Rgb(63, 185, 80),
            ),
            (
                ThemeId::FunDark,
                ThemeAppearance::Dark,
                Color::Rgb(13, 17, 23),
                Color::Rgb(230, 237, 243),
                Color::Rgb(181, 255, 0),
                Color::Rgb(240, 136, 62),
                Color::Rgb(63, 185, 80),
            ),
            (
                ThemeId::Midnight,
                ThemeAppearance::Dark,
                Color::Rgb(11, 16, 32),
                Color::Rgb(229, 231, 235),
                Color::Rgb(96, 165, 250),
                Color::Rgb(240, 136, 62),
                Color::Rgb(63, 185, 80),
            ),
            (
                ThemeId::Paper,
                ThemeAppearance::Light,
                Color::Rgb(250, 250, 249),
                Color::Rgb(28, 25, 23),
                Color::Rgb(124, 58, 237),
                Color::Rgb(188, 76, 0),
                Color::Rgb(26, 127, 55),
            ),
        ];
        for (id, appearance, background, foreground, accent, plan, build) in cases {
            let theme = Theme::resolve(id);
            assert_eq!(theme.id(), id);
            assert_eq!(theme.appearance(), appearance);
            assert_eq!(theme.style(ThemeRole::Surface).bg, Some(background));
            assert_eq!(theme.style(ThemeRole::Text).fg, Some(foreground));
            assert_eq!(theme.style(ThemeRole::Accent).fg, Some(accent));
            assert_eq!(theme.style(ThemeRole::PlanMode).fg, Some(plan));
            assert_eq!(theme.style(ThemeRole::BuildMode).fg, Some(build));
        }
    }

    #[test]
    fn bundled_markdown_roles_follow_the_exact_six_accent_palettes() {
        let cases = [
            (
                ThemeId::Terminal,
                [
                    Color::Cyan,
                    Color::Blue,
                    Color::Magenta,
                    Color::Yellow,
                    Color::Green,
                    Color::DarkGray,
                ],
            ),
            (
                ThemeId::FunDark,
                [
                    Color::Rgb(181, 255, 0),
                    Color::Rgb(88, 166, 255),
                    Color::Rgb(210, 168, 255),
                    Color::Rgb(255, 166, 87),
                    Color::Rgb(126, 231, 135),
                    Color::Rgb(255, 122, 178),
                ],
            ),
            (
                ThemeId::Midnight,
                [
                    Color::Rgb(96, 165, 250),
                    Color::Rgb(167, 139, 250),
                    Color::Rgb(244, 114, 182),
                    Color::Rgb(251, 191, 36),
                    Color::Rgb(45, 212, 191),
                    Color::Rgb(148, 163, 184),
                ],
            ),
            (
                ThemeId::Paper,
                [
                    Color::Rgb(109, 40, 217),
                    Color::Rgb(29, 78, 216),
                    Color::Rgb(190, 24, 93),
                    Color::Rgb(180, 83, 9),
                    Color::Rgb(4, 120, 87),
                    Color::Rgb(87, 83, 78),
                ],
            ),
        ];
        let heading_roles = [
            ThemeRole::MarkdownHeading1,
            ThemeRole::MarkdownHeading2,
            ThemeRole::MarkdownHeading3,
            ThemeRole::MarkdownHeading4,
            ThemeRole::MarkdownHeading5,
            ThemeRole::MarkdownHeading6,
        ];

        for (id, accents) in cases {
            let theme = Theme::resolve(id);
            for (role, accent) in heading_roles.into_iter().zip(accents) {
                assert_eq!(theme.style(role).fg, Some(accent), "{id:?} {role:?}");
            }
            assert_eq!(theme.style(ThemeRole::MarkdownStrong).fg, Some(accents[3]));
            assert_eq!(
                theme.style(ThemeRole::MarkdownEmphasis).fg,
                Some(accents[5])
            );
            assert_eq!(theme.style(ThemeRole::MarkdownLink).fg, Some(accents[1]));
            assert_eq!(theme.style(ThemeRole::MarkdownQuote).fg, Some(accents[2]));
            assert_eq!(theme.style(ThemeRole::CodeString).fg, Some(accents[4]));
            assert_eq!(theme.style(ThemeRole::CodeType).fg, Some(accents[0]));
        }
    }
}
