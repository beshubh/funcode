pub mod transcript;

use crate::{
    app::{App, AuthProvider, ModelsDialogPhase, Screen, Suggestion, SuggestionKind},
    composer::SessionMode,
    theme::{Theme, ThemeId, ThemeRole},
    transcript::EntryId,
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
    TranscriptEntry(EntryId),
    AuthProvider(usize),
    Suggestion(usize),
    MessageCopy,
    Mode(SessionMode),
    Theme(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiRegions {
    pub transcript_entries: Vec<transcript::EntryRegion>,
    pub auth_providers: Vec<Rect>,
    pub suggestions: Vec<Rect>,
    pub message_copy: Option<Rect>,
    pub mode_tabs: Vec<Rect>,
    pub theme_options: Vec<Rect>,
}

impl UiRegions {
    pub fn target_at(&self, column: u16, row: u16) -> Option<UiTarget> {
        let position = Position::new(column, row);
        if self
            .message_copy
            .is_some_and(|area| area.contains(position))
        {
            Some(UiTarget::MessageCopy)
        } else if let Some((index, _)) = self
            .auth_providers
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
        {
            Some(UiTarget::AuthProvider(index))
        } else if let Some((index, _)) = self
            .suggestions
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
        {
            Some(UiTarget::Suggestion(index))
        } else if let Some((index, _)) = self
            .theme_options
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
        {
            Some(UiTarget::Theme(index))
        } else if let Some((index, _)) = self
            .mode_tabs
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
        {
            Some(UiTarget::Mode(if index == 0 {
                SessionMode::Plan
            } else {
                SessionMode::Build
            }))
        } else {
            self.transcript_entries
                .iter()
                .find(|region| region.area.contains(position))
                .map(|region| UiTarget::TranscriptEntry(region.id))
        }
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App, theme: &Theme) -> UiRegions {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(theme.style(ThemeRole::Surface)),
        area,
    );
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
        regions.auth_providers = render_auth_dialog(frame, area, app, theme);
    } else if app.message_dialog.is_some()
        && area.width >= CHAT_MIN_WIDTH
        && area.height >= CHAT_MIN_HEIGHT
    {
        regions.message_copy = render_message_dialog(frame, area, app, theme);
    } else if app.theme_dialog.is_some()
        && area.width >= CHAT_MIN_WIDTH
        && area.height >= CHAT_MIN_HEIGHT
    {
        regions.theme_options = render_theme_dialog(frame, area, app, theme);
    } else if app.models_dialog.is_some()
        && area.width >= CHAT_MIN_WIDTH
        && area.height >= CHAT_MIN_HEIGHT
    {
        render_models_dialog(frame, area, app, theme);
    }
    regions
}

fn render_theme_dialog(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> Vec<Rect> {
    let Some(dialog) = app.theme_dialog else {
        return Vec::new();
    };
    let width = area.width.saturating_sub(6).min(54);
    let height = 10.min(area.height.saturating_sub(4));
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    let block = panel_block(" Choose theme ", theme).style(theme.style(ThemeRole::Surface));
    let inner = block.inner(dialog_area).inner(Margin::new(1, 1));
    frame.render_widget(block, dialog_area);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    for (index, theme_id) in ThemeId::ALL.iter().enumerate() {
        let candidate = Theme::resolve(*theme_id);
        let selected = dialog.selected == index;
        let style = if selected {
            candidate
                .style(ThemeRole::Accent)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme.style(ThemeRole::Text)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {} ", theme_id.display_name()), style),
                Span::styled(
                    format!("  {}", theme_id.as_str()),
                    theme.style(ThemeRole::MutedText),
                ),
            ])),
            rows[index],
        );
    }
    frame.render_widget(
        Paragraph::new("↑/↓ preview · Enter save · mouse preview · Esc cancel")
            .style(theme.style(ThemeRole::MutedText)),
        rows[4],
    );
    rows[..ThemeId::ALL.len()].to_vec()
}

fn render_models_dialog(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let Some(phase) = app.models_dialog.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(6).min(78);
    let height = area.height.saturating_sub(4).min(24);
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    let block = panel_block(" Available models ", theme);
    let inner = block.inner(dialog_area).inner(Margin::new(1, 1));
    frame.render_widget(block, dialog_area);

    let mut lines = match phase {
        ModelsDialogPhase::Loading => vec![
            Line::styled("Loading provider catalogs…", theme.style(ThemeRole::Accent)),
            Line::from(""),
            Line::styled("Esc close", theme.style(ThemeRole::MutedText)),
        ],
        ModelsDialogPhase::Failed(message) => vec![
            Line::styled("Could not load models", theme.style(ThemeRole::Warning)),
            Line::styled(message.clone(), theme.style(ThemeRole::MutedText)),
            Line::from(""),
            Line::styled(
                "Run /auth if sign-in is required · Esc close",
                theme.style(ThemeRole::MutedText),
            ),
        ],
        ModelsDialogPhase::Loaded(catalogs) => {
            let mut lines = Vec::new();
            for (provider_index, catalog) in catalogs.iter().enumerate() {
                if provider_index > 0 {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        catalog.provider.clone(),
                        theme.style(ThemeRole::Text).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", catalog.source),
                        theme.style(ThemeRole::MutedText),
                    ),
                ]));
                if catalog.models.is_empty() {
                    lines.push(Line::styled(
                        "  No user-visible models returned",
                        theme.style(ThemeRole::MutedText),
                    ));
                }
                for model in &catalog.models {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  • {}", model.display_name),
                            theme.style(ThemeRole::Accent),
                        ),
                        Span::styled(format!("  {}", model.id), theme.style(ThemeRole::MutedText)),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Enter or Esc close",
                theme.style(ThemeRole::MutedText),
            ));
            lines
        }
    };
    if lines.is_empty() {
        lines.push(Line::styled(
            "No providers configured",
            theme.style(ThemeRole::MutedText),
        ));
    }
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    let scroll = app.models_scroll().min(max_scroll).min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), inner);
}

fn render_auth_dialog(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> Vec<Rect> {
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

    let Some(dialog) = app.auth_dialog.as_ref() else {
        return Vec::new();
    };
    match &dialog.phase {
        crate::app::AuthDialogPhase::Selecting => {
            frame.render_widget(
                Paragraph::new("Choose how to authenticate")
                    .style(theme.style(ThemeRole::Text).add_modifier(Modifier::BOLD)),
                rows[0],
            );
            let provider_rows = Layout::vertical(
                AuthProvider::ALL
                    .iter()
                    .map(|_| Constraint::Length(4))
                    .collect::<Vec<_>>(),
            )
            .split(rows[1]);
            for (index, provider) in AuthProvider::ALL.iter().enumerate() {
                let selected = dialog.selected == index;
                let label_style = if selected {
                    theme
                        .style(ThemeRole::Accent)
                        .add_modifier(Modifier::REVERSED)
                } else {
                    theme.style(ThemeRole::Accent)
                };
                let option = Paragraph::new(Text::from(vec![
                    Line::styled(format!(" {} ", provider.label()), label_style),
                    Line::styled(
                        format!(" {}", provider.description()),
                        theme.style(ThemeRole::MutedText),
                    ),
                ]))
                .block(panel_block(" OpenAI ", theme));
                frame.render_widget(option, provider_rows[index]);
            }
            frame.render_widget(
                Paragraph::new("↑/↓ select · Enter open · click · Esc close")
                    .style(theme.style(ThemeRole::MutedText)),
                rows[2],
            );
            provider_rows.to_vec()
        }
        crate::app::AuthDialogPhase::Starting => {
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("Preparing browser sign-in…", theme.style(ThemeRole::Accent)),
                    Line::styled("Esc cancels", theme.style(ThemeRole::MutedText)),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            Vec::new()
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
                    Line::styled(heading, theme.style(ThemeRole::Accent)),
                    Line::styled(authorization_url.clone(), theme.style(ThemeRole::MutedText)),
                    Line::from(""),
                    Line::styled(
                        "Waiting for ChatGPT… · Esc cancel",
                        theme.style(ThemeRole::MutedText),
                    ),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            Vec::new()
        }
        crate::app::AuthDialogPhase::Succeeded { account_id } => {
            let detail = account_id.as_deref().map_or_else(
                || "Credentials saved".to_owned(),
                |id| format!("Connected to {id}"),
            );
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled(
                        "✓ Authenticated with ChatGPT",
                        theme.style(ThemeRole::Accent),
                    ),
                    Line::styled(detail, theme.style(ThemeRole::MutedText)),
                    Line::from(""),
                    Line::styled("Enter close", theme.style(ThemeRole::MutedText)),
                ])),
                inner,
            );
            Vec::new()
        }
        crate::app::AuthDialogPhase::Failed { message } => {
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("ChatGPT sign-in failed", theme.style(ThemeRole::Warning)),
                    Line::styled(message.clone(), theme.style(ThemeRole::MutedText)),
                    Line::from(""),
                    Line::styled(
                        "Enter choose provider · Esc close",
                        theme.style(ThemeRole::MutedText),
                    ),
                ]))
                .wrap(Wrap { trim: false }),
                inner,
            );
            Vec::new()
        }
    }
}

fn render_message_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
) -> Option<Rect> {
    let entry_id = app.message_dialog?;
    let message = app.transcript.user_message(entry_id)?;
    let width = area.width.saturating_sub(8).min(72);
    let height = area.height.saturating_sub(6).min(18);
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    let block = panel_block(" Message ", theme);
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let rows = Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).split(inner);
    let lines = message.content.lines(
        theme.style(ThemeRole::Text),
        theme.accent_badge(),
        theme.style(ThemeRole::Accent),
    );
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        rows[0],
    );
    let action_line = Line::from(vec![
        Span::styled(
            " Copy ",
            theme
                .style(ThemeRole::Accent)
                .add_modifier(Modifier::REVERSED),
        ),
        Span::styled(
            "  Revert (coming soon)  ·  Esc close",
            theme.style(ThemeRole::MutedText),
        ),
    ]);
    let copy_width = 6.min(rows[1].width);
    frame.render_widget(Paragraph::new(action_line), rows[1]);
    Some(Rect::new(rows[1].x, rows[1].y, copy_width, 1))
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
    let accent = theme.style(ThemeRole::Accent).add_modifier(Modifier::BOLD);
    let neutral = theme.style(ThemeRole::Text).add_modifier(Modifier::BOLD);
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
    let mut lines = vec![Line::styled(
        "Available commands",
        theme.style(ThemeRole::Text).add_modifier(Modifier::BOLD),
    )];
    lines.extend(app.available_commands().into_iter().map(|command| {
        Line::from(vec![
            Span::styled(
                format!("{:<10}", command.label),
                theme.style(ThemeRole::Accent),
            ),
            Span::raw(command.description),
        ])
    }));
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Enter start  ·  Ctrl+C quit",
        theme.style(ThemeRole::MutedText),
    ));
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
    let suggestions = app.suggestions();
    let composer_height = composer_height(app, inner.width, theme);

    let rows = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(composer_height),
    ])
    .split(inner);

    let mut regions = UiRegions {
        transcript_entries: transcript::render(frame, rows[0], app, theme),
        ..UiRegions::default()
    };
    render_activity(frame, rows[1], app, theme);
    let composer_area = rows[2];
    let suggestion_height =
        (suggestions.len() as u16 + 2).min(composer_area.y.saturating_sub(inner.y));
    let suggestion_area = Rect::new(
        composer_area.x,
        composer_area.y.saturating_sub(suggestion_height),
        composer_area.width,
        suggestion_height,
    );
    regions.suggestions = render_suggestions(frame, suggestion_area, app, &suggestions, theme);
    regions.mode_tabs = render_composer(frame, composer_area, app, theme);
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
            Span::styled(
                format!(" {}", suggestion.label),
                theme.style(ThemeRole::Accent),
            ),
            Span::styled(
                format!("  {}", suggestion.description),
                theme.style(ThemeRole::MutedText),
            ),
        ]);
        let style = if index == app.selected_suggestion() {
            theme
                .style(ThemeRole::Text)
                .add_modifier(Modifier::REVERSED)
        } else {
            theme.style(ThemeRole::Text)
        };
        frame.render_widget(Paragraph::new(line).style(style), row);
        regions.push(row);
    }
    regions
}

fn render_activity(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let text = if app.active_request.is_some() {
        let dots = ".".repeat((app.animation_frame / 5) % 4);
        format!(" Waiting{dots}")
    } else if app.transcript.entries().iter().any(|entry| {
        matches!(
            entry.kind,
            crate::transcript::EntryKind::Assistant(crate::transcript::AssistantMessage {
                status: crate::transcript::AssistantStatus::Queued,
                ..
            })
        )
    }) {
        " Queued…".to_owned()
    } else if let Some(notice) = &app.notice {
        format!(" {notice}")
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(text).style(theme.style(ThemeRole::Accent)),
        area,
    );
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> Vec<Rect> {
    let active_mode = app.effective_mode();
    let mode_role = match active_mode {
        SessionMode::Plan => ThemeRole::PlanMode,
        SessionMode::Build => ThemeRole::BuildMode,
    };
    let mode_style = theme.style(mode_role).add_modifier(Modifier::BOLD);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "[ Plan ]",
            if active_mode == SessionMode::Plan {
                mode_style
            } else {
                theme.style(ThemeRole::MutedText)
            },
        ),
        Span::raw(" "),
        Span::styled(
            "[ Build ]",
            if active_mode == SessionMode::Build {
                mode_style
            } else {
                theme.style(ThemeRole::MutedText)
            },
        ),
        Span::raw(" "),
    ]);
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::styled(
            " Enter send · Shift+Enter new line ",
            theme.style(ThemeRole::MutedText),
        ))
        .border_set(theme.border_set())
        .border_style(theme.style(mode_role));
    let inner = block.inner(area);
    let input_area = inner;
    let (cursor_column, cursor_row) = composer_cursor(
        app.composer.text(),
        app.composer.cursor(),
        input_area.width.max(1),
    );
    let vertical_scroll = cursor_row.saturating_sub(input_area.height.saturating_sub(1));
    let content = if app.composer.text().is_empty() {
        Text::from(Line::styled(
            "Type something…",
            theme.style(ThemeRole::MutedText),
        ))
    } else {
        Text::from(app.composer.content().lines(
            theme.style(ThemeRole::Text),
            theme.accent_badge(),
            theme.style(mode_role).add_modifier(Modifier::REVERSED),
        ))
    };

    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .scroll((vertical_scroll, 0)),
        input_area,
    );
    if app.auth_dialog.is_none()
        && app.message_dialog.is_none()
        && app.theme_dialog.is_none()
        && app.models_dialog.is_none()
    {
        frame.set_cursor_position((
            input_area.x.saturating_add(cursor_column),
            input_area
                .y
                .saturating_add(cursor_row.saturating_sub(vertical_scroll)),
        ));
    }
    vec![
        Rect::new(area.x.saturating_add(2), area.y, 8, 1),
        Rect::new(area.x.saturating_add(11), area.y, 9, 1),
    ]
}

fn composer_height(app: &App, width: u16, theme: &Theme) -> u16 {
    let _ = (app, width, theme);
    5
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

fn panel_block<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    Block::bordered()
        .title(title)
        .border_set(theme.border_set())
        .border_style(theme.style(ThemeRole::Border))
        .style(theme.style(ThemeRole::Surface))
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
            .style(theme.style(ThemeRole::Warning)),
        vertical[1],
    );
}

#[cfg(test)]
mod tests {
    use super::{UiRegions, UiTarget, render};
    use crate::{
        agent::AgentEvent,
        app::{App, ModelsDialogPhase, Screen},
        llm::{ModelInfo, ProviderModels},
        theme::Theme,
        transcript::ToolArtifact,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Position,
        style::{Color, Modifier},
    };

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
        assert!(!screen.contains("/help"));
        assert!(screen.contains("/models"));
        assert!(!screen.contains("Model: not connected"));
        assert_eq!(top_left, " ");
        assert!(!cursor_visible);
    }

    #[test]
    fn idle_chat_has_no_transcript_entry_regions_and_no_app_border() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        let (screen, cursor_visible, regions, top_left) = render_to_string(&app, 100, 30);

        assert!(!screen.contains("Agent messages"));
        assert!(screen.contains("No messages yet"));
        assert!(screen.contains("Type something"));
        assert!(regions.transcript_entries.is_empty());
        assert_eq!(top_left, " ");
        assert!(cursor_visible);
    }

    #[test]
    fn custom_theme_paints_the_frame_and_exposes_colored_mode_tabs() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        let theme = Theme::resolve(crate::theme::ThemeId::FunDark);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();

        terminal
            .draw(|frame| regions = render(frame, &app, &theme))
            .unwrap();

        assert!(
            terminal.backend().buffer().content().iter().all(|cell| {
                cell.style().bg == theme.style(crate::theme::ThemeRole::Surface).bg
            })
        );
        assert_eq!(regions.mode_tabs.len(), 2);
        let build = regions.mode_tabs[1];
        assert_eq!(
            regions.target_at(build.x, build.y),
            Some(UiTarget::Mode(crate::composer::SessionMode::Build))
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell(Position::new(build.x + 2, build.y))
                .unwrap()
                .style()
                .fg,
            theme.style(crate::theme::ThemeRole::BuildMode).fg
        );
        let composer_border = terminal
            .backend()
            .buffer()
            .cell(Position::new(build.x.saturating_sub(11), build.y))
            .unwrap()
            .style();
        assert_eq!(
            composer_border.fg,
            theme.style(crate::theme::ThemeRole::BuildMode).fg
        );
    }

    #[test]
    fn theme_picker_lists_bundled_themes_as_previewable_regions() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.open_theme_dialog();

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Choose theme"));
        assert!(screen.contains("Terminal"));
        assert!(screen.contains("Fun Dark"));
        assert!(screen.contains("Midnight"));
        assert!(screen.contains("Paper"));
        assert_eq!(regions.theme_options.len(), 4);
        let paper = regions.theme_options[3];
        assert_eq!(
            regions.target_at(paper.x, paper.y),
            Some(UiTarget::Theme(3))
        );
        assert!(!cursor_visible);
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
    fn attached_files_render_inline_in_the_composer_before_send() {
        let mut app = App::with_files(["src/app.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("Review @src/app.rs");
        app.activate_suggestion(0);

        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Review @src/app.rs"));
    }

    #[test]
    fn inline_file_references_use_the_theme_accent_color() {
        let mut app = App::with_files(["src/app.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src/app.rs");
        app.activate_suggestion(0);
        let theme = Theme::default();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &theme);
            })
            .unwrap();

        let badge_cell = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|cell| cell.symbol() == "@")
            .unwrap();
        assert_eq!(
            badge_cell.style().fg,
            theme.style(crate::theme::ThemeRole::Accent).fg
        );
        assert!(badge_cell.style().add_modifier.contains(Modifier::REVERSED));
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
        app.transcript.submit(5, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 5 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 5,
            call_id: 1,
            name: "read_file".into(),
            summary: "Reading".into(),
        });
        app.composer.insert_text("@src/");

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 60, 20);

        assert!(screen.contains("src/app.rs"));
        assert!(screen.contains("@src/"));
        assert!(cursor_visible);
        assert_eq!(regions.suggestions.len(), 1);
    }

    #[test]
    fn models_dialog_renders_provider_and_model_identifiers() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.models_dialog = Some(ModelsDialogPhase::Loaded(vec![ProviderModels {
            provider: "ChatGPT".into(),
            source: "live provider API".into(),
            models: vec![ModelInfo {
                id: "gpt-test".into(),
                display_name: "GPT Test".into(),
            }],
        }]));

        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Available models"));
        assert!(screen.contains("ChatGPT"));
        assert!(screen.contains("live provider API"));
        assert!(screen.contains("GPT Test"));
        assert!(screen.contains("gpt-test"));
    }

    #[test]
    fn long_model_catalogs_can_scroll_to_the_last_model() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.models_dialog = Some(ModelsDialogPhase::Loaded(vec![ProviderModels {
            provider: "ChatGPT".into(),
            source: "live provider API".into(),
            models: (0..30)
                .map(|index| ModelInfo {
                    id: format!("model-{index}"),
                    display_name: format!("Model {index}"),
                })
                .collect(),
        }]));

        let (first_screen, _, _, _) = render_to_string(&app, 100, 30);
        assert!(!first_screen.contains("model-29"));

        for _ in 0..40 {
            app.scroll_models_down();
        }
        let (last_screen, _, _, _) = render_to_string(&app, 100, 30);
        assert!(last_screen.contains("model-29"));
    }

    #[test]
    fn auth_dialog_lists_chatgpt_subscription_as_a_clickable_provider() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "hello".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.open_auth_dialog();

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Authenticate"));
        assert!(screen.contains("ChatGPT subscription"));
        assert!(screen.contains("browser"));
        assert_eq!(regions.auth_providers.len(), 1);
        let provider = regions.auth_providers[0];
        assert_eq!(
            regions.target_at(provider.x, provider.y),
            Some(UiTarget::AuthProvider(0))
        );
        assert!(!regions.transcript_entries.is_empty());
        assert!(!cursor_visible);
    }

    #[test]
    fn transcript_content_has_balanced_horizontal_padding() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "x".repeat(54), Vec::new());
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
        assert_eq!(buffer.cell(Position::new(3, 1)).unwrap().symbol(), "┌");
        assert_eq!(buffer.cell(Position::new(56, 2)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell(Position::new(57, 2)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell(Position::new(58, 2)).unwrap().symbol(), " ");
    }

    #[test]
    fn reasoning_is_a_clickable_collapsed_transcript_block() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "hello".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::ReasoningDelta {
            request_id: 1,
            summary: "Checking the request.".into(),
        });

        let (screen, _, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("thinking"));
        assert!(!screen.contains("Checking the request."));
        assert!(screen.contains("Waiting"));
        let reasoning_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Reasoning(_)))
            .unwrap()
            .id;
        let thinking = regions
            .transcript_entries
            .iter()
            .find(|region| region.id == reasoning_id)
            .copied()
            .unwrap();
        assert_eq!(
            regions.target_at(thinking.area.x, thinking.area.y),
            Some(UiTarget::TranscriptEntry(thinking.id))
        );

        app.activate_transcript_entry(thinking.id);
        let (expanded, _, _, _) = render_to_string(&app, 100, 30);
        assert!(expanded.contains("Checking the request."));

        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 1,
            text: "reply".into(),
        });
        let (streaming, _, regions, _) = render_to_string(&app, 100, 30);
        assert!(streaming.contains("thinking"));
        assert!(!regions.transcript_entries.is_empty());
    }

    #[test]
    fn completed_reasoning_without_a_summary_does_not_keep_working() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "hello".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::Completed { request_id: 1 });
        let reasoning_id = app
            .transcript
            .entries()
            .iter()
            .find(|entry| matches!(entry.kind, crate::transcript::EntryKind::Reasoning(_)))
            .unwrap()
            .id;
        app.activate_transcript_entry(reasoning_id);

        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("No reasoning summary was provided"));
        assert!(!screen.contains("Working"));
    }

    #[test]
    fn tools_are_persistent_clickable_transcript_blocks() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(3, "inspect".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 3 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            call_id: 3,
            name: "read_file".into(),
            summary: "Reading src/main.rs".into(),
        });

        let (collapsed, _, regions, _) = render_to_string(&app, 100, 30);
        assert!(collapsed.contains("tool"));
        assert!(collapsed.contains("read_file"));
        let tools = *regions.transcript_entries.last().unwrap();
        assert_eq!(
            regions.target_at(tools.area.x, tools.area.y),
            Some(UiTarget::TranscriptEntry(tools.id))
        );

        app.activate_transcript_entry(tools.id);
        let (expanded, _, _, _) = render_to_string(&app, 100, 30);
        assert!(expanded.contains("Reading src/main.rs"));
    }

    #[test]
    fn expanded_tool_output_can_scroll_through_content_larger_than_the_terminal() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(3, "inspect".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 3 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            call_id: 3,
            name: "read_file".into(),
            summary: "Reading src/main.rs".into(),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 3,
            call_id: 3,
            summary: None,
            artifacts: vec![ToolArtifact::TextDetail(
                (0..40)
                    .map(|line| format!("tool-line-{line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )],
        });
        let tool_id = app.transcript.entries()[3].id;
        app.activate_transcript_entry(tool_id);
        let (bottom, _, _, _) = render_to_string(&app, 60, 20);

        for _ in 0..5 {
            app.scroll_transcript_up();
        }
        let (scrolled, _, _, _) = render_to_string(&app, 60, 20);

        assert!(bottom.contains("tool-line-39"));
        assert_ne!(bottom, scrolled);
        assert!(scrolled.contains("↑ End to follow"));
    }

    #[test]
    fn user_message_modal_exposes_inline_files_and_a_deferred_revert_control() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(
            1,
            "Review this".into(),
            vec![crate::transcript::Attachment::workspace_file("src/app.rs")],
        );
        app.open_message_dialog(app.transcript.entries()[0].id);

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Message"));
        assert!(screen.contains("Copy"));
        assert!(screen.contains("Revert (coming soon)"));
        assert!(screen.contains("@src/app.rs"));
        assert!(regions.message_copy.is_some());
        assert!(!cursor_visible);
    }

    #[test]
    fn submitted_user_messages_render_file_references_in_the_transcript() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(
            1,
            "Please inspect this".into(),
            vec![crate::transcript::Attachment::workspace_file("src/app.rs")],
        );

        let (screen, _, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("┌─ you"));
        assert!(screen.contains("@src/app.rs"));
        assert_eq!(
            regions.target_at(
                regions.transcript_entries[0].area.x,
                regions.transcript_entries[0].area.y,
            ),
            Some(UiTarget::TranscriptEntry(regions.transcript_entries[0].id))
        );
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
                .any(|cell| cell.style().fg == theme.style(crate::theme::ThemeRole::Accent).fg)
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.style().fg == theme.style(crate::theme::ThemeRole::Text).fg)
        );
        assert!(
            cells
                .iter()
                .all(|cell| cell.style().fg != Some(Color::Black))
        );
    }
}
