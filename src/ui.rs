pub mod transcript;

pub use crate::app::PointerTarget as UiTarget;
use crate::{
    app::{
        App, AuthProvider, ModelsDialogPhase, PendingSubmissionView, Screen, Suggestion,
        SuggestionKind,
    },
    composer::{DisplayLine, DisplayRunKind},
    session::SessionMode,
    theme::{Theme, ThemeId, ThemeRole},
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

const FUN_MIN_WIDTH: u16 = 60;
const FUN_MIN_HEIGHT: u16 = 20;
const COMPOSER_HORIZONTAL_PADDING: u16 = 2;
const COMPOSER_VERTICAL_PADDING: u16 = 1;
const COMPOSER_BORDER_HEIGHT: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelRegion {
    pub index: usize,
    pub area: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiRegions {
    pub transcript_entries: Vec<transcript::EntryRegion>,
    pub auth_providers: Vec<Rect>,
    pub suggestions: Vec<Rect>,
    pub message_copy: Option<Rect>,
    pub composer_input: Option<Rect>,
    pub theme_options: Vec<Rect>,
    pub models: Vec<ModelRegion>,
    pub model_refresh: Option<Rect>,
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
        } else if self
            .model_refresh
            .is_some_and(|area| area.contains(position))
        {
            Some(UiTarget::ModelRefresh)
        } else if let Some(region) = self
            .models
            .iter()
            .find(|region| region.area.contains(position))
        {
            Some(UiTarget::Model(region.index))
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
        Screen::Chat if area.width < FUN_MIN_WIDTH || area.height < FUN_MIN_HEIGHT => {
            render_too_small(frame, area, FUN_MIN_WIDTH, FUN_MIN_HEIGHT, theme);
            UiRegions::default()
        }
        Screen::Chat => render_chat(frame, area, app, theme),
    };

    if app.auth_dialog.is_some() && area.width >= FUN_MIN_WIDTH && area.height >= FUN_MIN_HEIGHT {
        regions = UiRegions::default();
        regions.auth_providers = render_auth_dialog(frame, area, app, theme);
    } else if app.message_dialog.is_some()
        && area.width >= FUN_MIN_WIDTH
        && area.height >= FUN_MIN_HEIGHT
    {
        regions = UiRegions::default();
        regions.message_copy = render_message_dialog(frame, area, app, theme);
    } else if app.theme_dialog.is_some()
        && area.width >= FUN_MIN_WIDTH
        && area.height >= FUN_MIN_HEIGHT
    {
        regions = UiRegions::default();
        regions.theme_options = render_theme_dialog(frame, area, app, theme);
    } else if app.models_dialog.is_some()
        && area.width >= FUN_MIN_WIDTH
        && area.height >= FUN_MIN_HEIGHT
    {
        regions = UiRegions::default();
        (regions.models, regions.model_refresh) = render_models_dialog(frame, area, app, theme);
    } else if app.paste_confirmation().is_some()
        && area.width >= FUN_MIN_WIDTH
        && area.height >= FUN_MIN_HEIGHT
    {
        regions = UiRegions::default();
        render_paste_confirmation(frame, area, app, theme);
    } else if app.pending_submission_view().is_some()
        && area.width >= FUN_MIN_WIDTH
        && area.height >= FUN_MIN_HEIGHT
    {
        regions = UiRegions::default();
        render_pending_submission(frame, area, app, theme);
    }
    regions
}

fn render_pending_submission(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let Some(pending) = app.pending_submission_view() else {
        return;
    };
    let width = area.width.saturating_sub(8).min(62);
    let height = 7.min(area.height.saturating_sub(4));
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    let (title, lines) = match pending {
        PendingSubmissionView::Preflighting => (
            " Preparing request ",
            vec![
                Line::styled(
                    "Reading attachments and measuring the final request…",
                    theme.style(ThemeRole::Text),
                ),
                Line::raw(""),
                Line::styled("Esc cancel", theme.style(ThemeRole::MutedText)),
            ],
        ),
        PendingSubmissionView::Confirming { bytes } => (
            " Large request ",
            vec![
                Line::styled(
                    format!("Send the prepared {} KiB request?", bytes.div_ceil(1024)),
                    theme.style(ThemeRole::Text),
                ),
                Line::raw(""),
                Line::styled(
                    "Enter/y confirm · Esc/n keep editing",
                    theme.style(ThemeRole::MutedText),
                ),
            ],
        ),
    };
    let block = panel_block(title, theme);
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_paste_confirmation(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let Some(proposal) = app.paste_confirmation() else {
        return;
    };
    let width = area.width.saturating_sub(8).min(58);
    let height = 7.min(area.height.saturating_sub(4));
    let dialog_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog_area);
    let block = panel_block(" Large paste ", theme);
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);
    let content = Text::from(vec![
        Line::styled(
            format!(
                "Paste {} lines ({} KiB)?",
                proposal.line_count(),
                proposal.projected_bytes().div_ceil(1024)
            ),
            theme.style(ThemeRole::Text),
        ),
        Line::raw(""),
        Line::styled(
            "Enter/y confirm · Esc/n cancel",
            theme.style(ThemeRole::MutedText),
        ),
    ]);
    frame.render_widget(Paragraph::new(content), inner);
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

fn render_models_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: &Theme,
) -> (Vec<ModelRegion>, Option<Rect>) {
    let Some(phase) = app.models_dialog.as_ref() else {
        return (Vec::new(), None);
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
    let block = panel_block(" Select model ", theme);
    let inner = block.inner(dialog_area).inner(Margin::new(1, 1));
    frame.render_widget(block, dialog_area);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let content_area = rows[0];
    let footer_area = rows[1];

    let mut model_lines = Vec::new();
    let mut refresh_enabled = false;
    let mut lines = match phase {
        ModelsDialogPhase::Loading => {
            frame.render_widget(
                Paragraph::new("Esc close").style(theme.style(ThemeRole::MutedText)),
                footer_area,
            );
            vec![Line::styled(
                "Loading provider catalogs…",
                theme.style(ThemeRole::Accent),
            )]
        }
        ModelsDialogPhase::Failed(message) => {
            refresh_enabled = true;
            vec![
                Line::styled("Could not load models", theme.style(ThemeRole::Warning)),
                Line::styled(message.clone(), theme.style(ThemeRole::MutedText)),
            ]
        }
        ModelsDialogPhase::Loaded(catalogs) => {
            refresh_enabled = true;
            let mut lines = Vec::new();
            let mut model_index = 0;
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
                    let selected = model_index == app.selected_model_index();
                    let marker = if model.id == app.current_model() {
                        "✓"
                    } else {
                        " "
                    };
                    let line_index = lines.len();
                    let selection_style = if selected {
                        theme
                            .style(ThemeRole::Text)
                            .add_modifier(Modifier::REVERSED)
                    } else {
                        theme.style(ThemeRole::Text)
                    };
                    lines.push(
                        Line::from(vec![
                            Span::styled(
                                format!(" {marker} {}", model.display_name),
                                theme.style(ThemeRole::Accent),
                            ),
                            Span::styled(
                                format!("  {}", model.id),
                                theme.style(ThemeRole::MutedText),
                            ),
                        ])
                        .style(selection_style),
                    );
                    model_lines.push((model_index, line_index));
                    model_index += 1;
                }
            }
            lines
        }
    };
    if lines.is_empty() {
        lines.push(Line::styled(
            "No providers configured",
            theme.style(ThemeRole::MutedText),
        ));
    }
    let mut scroll = 0;
    if let Some((_, selected_line)) = model_lines
        .iter()
        .find(|(index, _)| *index == app.selected_model_index())
    {
        if *selected_line < scroll {
            scroll = *selected_line;
        } else if *selected_line >= scroll.saturating_add(content_area.height as usize) {
            scroll = selected_line
                .saturating_add(1)
                .saturating_sub(content_area.height as usize);
        }
    }
    let visible_regions = model_lines
        .into_iter()
        .filter_map(|(index, line)| {
            let visible_row = line.checked_sub(scroll)?;
            (visible_row < content_area.height as usize).then_some(ModelRegion {
                index,
                area: Rect::new(
                    content_area.x,
                    content_area.y + visible_row as u16,
                    content_area.width,
                    1,
                ),
            })
        })
        .collect();
    let refresh_region = refresh_enabled.then_some(Rect::new(
        footer_area.x,
        footer_area.y,
        9.min(footer_area.width),
        1,
    ));
    if refresh_enabled {
        let help = if matches!(phase, ModelsDialogPhase::Failed(_)) {
            "  r retry · /auth sign in · Esc close"
        } else {
            "  r refresh · ↑/↓ select · Enter use · Esc close"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Refresh ",
                    theme
                        .style(ThemeRole::Accent)
                        .add_modifier(Modifier::REVERSED),
                ),
                Span::styled(help, theme.style(ThemeRole::MutedText)),
            ])),
            footer_area,
        );
    }
    let scroll = scroll.min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
        content_area,
    );
    (visible_regions, refresh_region)
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
    let message_layout = message.content.layout(rows[0].width.max(1) as usize);
    let lines = display_lines(
        &message_layout.visible_rows(0, rows[0].height as usize),
        theme,
    );
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[0]);
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

fn render_welcome(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let inner = area.inner(Margin::new(2, 0));
    let rows = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(fun_logo(theme)).alignment(Alignment::Center),
        rows[0],
    );

    render_welcome_help(frame, rows[2], app, theme);
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

fn render_welcome_help(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
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
        "Type a request below  ·  /exit quit",
        theme.style(ThemeRole::MutedText),
    ));
    let card_area = welcome_help_area(area, lines.len() as u16);
    let help = Text::from(lines);
    frame.render_widget(
        Paragraph::new(help)
            .wrap(Wrap { trim: false })
            .block(panel_block("Help", theme)),
        card_area,
    );
}

fn welcome_help_area(area: Rect, content_lines: u16) -> Rect {
    const MAX_WIDTH: u16 = 58;
    let width = area.width.min(MAX_WIDTH);
    let height = content_lines.saturating_add(2).min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y,
        width,
        height,
    )
}

fn render_chat(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> UiRegions {
    let inner = area.inner(Margin::new(1, 1));
    let suggestions = app.suggestions();
    let activity_height = u16::from(!activity_text(app).is_empty());
    let composer_height =
        composer_height(app, inner.width).min(inner.height.saturating_sub(activity_height));

    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(activity_height),
        Constraint::Length(composer_height),
    ])
    .split(inner);

    let mut regions = UiRegions::default();
    if app.transcript.entries().is_empty() {
        render_welcome(frame, rows[0], app, theme);
    } else {
        regions.transcript_entries = transcript::render(frame, rows[0], app, theme);
    }
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
    regions.composer_input = Some(render_composer(frame, composer_area, app, theme));
    render_session_usage(frame, area, app, theme);
    regions
}

fn render_session_usage(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    const WIDGET_WIDTH: u16 = 24;
    const WIDGET_HEIGHT: u16 = 3;

    // Keep the compact transcript readable at the documented 60-column
    // minimum. At normal widths the widget sits in the unused top-right area.
    if area.width < 70 || area.height < WIDGET_HEIGHT + 2 {
        return;
    }

    let total_tokens = app.session_usage.total_tokens();
    let context_percent = app
        .session_usage
        .context_utilization_percent(app.current_model_context_window());
    if total_tokens.is_none() && context_percent.is_none() {
        return;
    }
    let widget_area = Rect::new(
        area.x + area.width.saturating_sub(WIDGET_WIDTH + 1),
        area.y + 1,
        WIDGET_WIDTH,
        WIDGET_HEIGHT,
    );
    let token_text = total_tokens
        .map(format_token_count)
        .unwrap_or_else(|| "—".into());
    let context_text = context_percent
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "—".into());
    let block = Block::bordered()
        .border_set(theme.border_set())
        .border_style(theme.style(ThemeRole::MutedText));
    let inner = block.inner(widget_area);
    frame.render_widget(Clear, widget_area);
    frame.render_widget(block, widget_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {token_text} tok"),
                theme.style(ThemeRole::Text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", theme.style(ThemeRole::MutedText)),
            Span::styled(
                format!("{context_text} ctx"),
                theme.style(ThemeRole::Accent).add_modifier(Modifier::BOLD),
            ),
        ])),
        inner,
    );
}

fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 10_000 {
        return format!("{:.1}K", tokens as f64 / 1_000.0);
    }
    if tokens < 1_000_000 {
        return format!("{}K", tokens / 1_000);
    }
    format!("{:.1}M", tokens as f64 / 1_000_000.0)
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
    frame.render_widget(
        Paragraph::new(activity_text(app)).style(theme.style(ThemeRole::Accent)),
        area,
    );
}

fn activity_text(app: &App) -> String {
    if app.active_request.is_some() {
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
    }
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) -> Rect {
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
            format!(
                " Enter send · Shift+Enter new line · Tab mode · Ctrl+C clear · Model: {} ",
                app.current_model()
            ),
            theme.style(ThemeRole::MutedText),
        ))
        .border_set(theme.border_set())
        .border_style(theme.style(mode_role));
    let inner = block.inner(area);
    let input_area = inner.inner(Margin::new(
        COMPOSER_HORIZONTAL_PADDING,
        COMPOSER_VERTICAL_PADDING,
    ));
    let layout = app.composer.layout(input_area.width.max(1) as usize);
    let cursor = app.composer.cursor_geometry(&layout);
    let viewport_height = input_area.height as usize;
    let vertical_scroll = cursor.row.saturating_sub(viewport_height.saturating_sub(1));
    let content = if app.composer.is_empty() {
        Text::from(Line::styled(
            "Type something…",
            theme.style(ThemeRole::MutedText),
        ))
    } else {
        Text::from(display_lines(
            &layout.visible_rows(vertical_scroll, viewport_height),
            theme,
        ))
    };

    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(content), input_area);
    if app.composer_cursor_visible() {
        frame.set_cursor_position((
            input_area
                .x
                .saturating_add(cursor.column.min(u16::MAX as usize) as u16),
            input_area.y.saturating_add(
                cursor
                    .row
                    .saturating_sub(vertical_scroll)
                    .min(u16::MAX as usize) as u16,
            ),
        ));
    }
    input_area
}

fn composer_height(app: &App, width: u16) -> u16 {
    let content_width = composer_content_width(width);
    let line_count = app
        .composer
        .layout(content_width as usize)
        .total_rows()
        .min(u16::MAX as usize) as u16;
    line_count
        .saturating_add(COMPOSER_BORDER_HEIGHT)
        .saturating_add(COMPOSER_VERTICAL_PADDING.saturating_mul(2))
        .max(5)
}

fn display_lines(lines: &[DisplayLine], theme: &Theme) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            Line::from(
                line.runs
                    .iter()
                    .map(|run| {
                        let style = match run.kind {
                            DisplayRunKind::Text => theme.style(ThemeRole::Text),
                            DisplayRunKind::FileReference | DisplayRunKind::PastedBlock => {
                                theme.accent_badge()
                            }
                        };
                        Span::styled(run.text.clone(), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn composer_content_width(width: u16) -> u16 {
    width
        .saturating_sub(2)
        .saturating_sub(COMPOSER_HORIZONTAL_PADDING.saturating_mul(2))
        .max(1)
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
    use super::{UiRegions, UiTarget, composer_height, render, welcome_help_area};
    use crate::{
        agent::AgentEvent,
        app::{App, ModelsDialogPhase, Screen},
        llm::{ModelInfo, ProviderModels},
        theme::{Theme, ThemeRole},
        transcript::ToolArtifact,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::{Position, Rect},
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

    fn style_at_text(terminal: &Terminal<TestBackend>, needle: &str) -> ratatui::style::Style {
        let screen = terminal.backend().to_string();
        let (row, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap();
        let byte = line.find(needle).unwrap();
        let column = line[..byte].chars().count() as u16;
        terminal
            .backend()
            .buffer()
            .cell(Position::new(column, row as u16))
            .unwrap()
            .style()
    }

    fn position_of(terminal: &Terminal<TestBackend>, needle: &str) -> Position {
        let buffer = terminal.backend().buffer();
        let symbols: Vec<_> = needle
            .chars()
            .map(|character| character.to_string())
            .collect();
        for row in 0..buffer.area.height {
            for column in 0..=buffer.area.width.saturating_sub(symbols.len() as u16) {
                if symbols.iter().enumerate().all(|(offset, symbol)| {
                    buffer
                        .cell(Position::new(column + offset as u16, row))
                        .is_some_and(|cell| cell.symbol() == symbol)
                }) {
                    return Position::new(column, row);
                }
            }
        }
        panic!("{needle:?} was not rendered")
    }

    #[test]
    fn fun_launch_unifies_logo_help_and_active_composer() {
        let (screen, cursor_visible, regions, top_left) = render_to_string(&App::new(), 100, 30);

        assert!(screen.contains("██████████"));
        assert!(screen.contains("Help"));
        assert!(screen.contains("/auth"));
        assert!(screen.contains("/exit"));
        assert!(!screen.contains("/sessions"));
        assert!(!screen.contains("/help"));
        assert!(screen.contains("/models"));
        assert!(screen.contains("Type something"));
        assert!(!screen.contains("No messages yet"));
        assert!(!screen.contains("Model: not connected"));
        assert!(regions.transcript_entries.is_empty());
        assert_eq!(top_left, " ");
        assert!(cursor_visible);
    }

    #[test]
    fn idle_chat_has_no_transcript_entry_regions_and_no_app_border() {
        let app = App::new();

        let (screen, cursor_visible, regions, top_left) = render_to_string(&app, 100, 30);

        assert!(!screen.contains("Agent messages"));
        assert!(screen.contains("Type a request below"));
        assert!(screen.contains("Type something"));
        assert!(regions.transcript_entries.is_empty());
        assert_eq!(top_left, " ");
        assert!(cursor_visible);
    }

    #[test]
    fn welcome_help_is_a_centered_content_sized_card() {
        let available = Rect::new(0, 10, 100, 18);
        let card = welcome_help_area(available, 8);

        assert_eq!(card.width, 58);
        assert_eq!(card.height, 10);
        assert_eq!(card.x, 21);
        assert_eq!(card.y, 10);
    }

    #[test]
    fn chat_renders_a_rounded_session_widget_with_reported_usage_and_context() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.open_models_dialog();
        app.set_current_model("gpt-test");
        app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Loaded(vec![
            ProviderModels {
                provider: "ChatGPT".into(),
                source: "live provider API".into(),
                models: vec![ModelInfo {
                    id: "gpt-test".into(),
                    display_name: "GPT Test".into(),
                    context_window: Some(1_000),
                }],
            },
        ]));
        app.models_dialog = None;
        app.transcript.submit(1, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::Usage {
            request_id: 1,
            usage: crate::usage::TokenUsage {
                input_tokens: 250,
                output_tokens: 50,
                total_tokens: 300,
            },
        });

        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(!screen.contains("Session"));
        assert!(screen.contains("300 tok"));
        assert!(screen.contains("25% ctx"));
    }

    #[test]
    fn retry_failure_and_retrying_state_are_visible_together() {
        let mut app = App::new();
        app.transcript.submit(1, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::Retrying {
            request_id: 1,
            attempt: 2,
            max_retries: 20,
            message: "gateway timeout".into(),
        });

        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("gateway timeout"));
        assert!(screen.contains("Retrying"));
        assert!(screen.contains("2/20"));
    }

    #[test]
    fn custom_theme_paints_the_frame_and_exposes_colored_mode_labels() {
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
        let build = position_of(&terminal, "[ Build ]");
        assert!(regions.composer_input.is_some());
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
    fn accent_role_colors_commands_queued_thinking_tools_and_waiting() {
        let theme = Theme::resolve(crate::theme::ThemeId::FunDark);
        let accent = theme.style(crate::theme::ThemeRole::Accent).fg;

        let backend = TestBackend::new(100, 30);
        let mut home = Terminal::new(backend).unwrap();
        home.draw(|frame| {
            let _ = render(frame, &App::new(), &theme);
        })
        .unwrap();
        assert_eq!(style_at_text(&home, "/theme").fg, accent);

        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "first".into(), Vec::new());
        app.transcript.submit(2, "second".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 2 });
        app.handle_agent_event(AgentEvent::ReasoningDelta {
            request_id: 2,
            summary: "Checking".into(),
        });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 2,
            call_id: 1,
            name: "read_file".into(),
            summary: "Reading".into(),
            artifacts: Vec::new(),
        });
        let backend = TestBackend::new(100, 40);
        let mut chat = Terminal::new(backend).unwrap();
        chat.draw(|frame| {
            let _ = render(frame, &app, &theme);
        })
        .unwrap();
        for label in ["queued…", "thinking", "tool", "Waiting"] {
            assert_eq!(style_at_text(&chat, label).fg, accent, "{label}");
        }
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
    fn wrapped_words_keep_the_cursor_at_the_end_of_rendered_text() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer
            .insert_text("01234567890123456789012345678901234567890123456789 helloZ");
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &Theme::default());
            })
            .unwrap();

        let final_word = position_of(&terminal, "helloZ");
        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(final_word.x + "helloZ".len() as u16, final_word.y)
        );
    }

    #[test]
    fn wide_graphemes_wrap_with_the_cursor_after_the_rendered_symbol() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text(&format!("{}界", "a".repeat(51)));
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &Theme::default());
            })
            .unwrap();

        let symbol = position_of(&terminal, "界");
        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(symbol.x + 2, symbol.y)
        );
    }

    #[test]
    fn composer_grows_to_show_all_lines_with_padding() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript
            .submit(1, "earlier prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::Completed { request_id: 1 });
        let text = (1..=18)
            .map(|line| format!("LINE{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.composer.insert_text(&text);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();

        terminal
            .draw(|frame| regions = render(frame, &app, &Theme::default()))
            .unwrap();

        let first = position_of(&terminal, "LINE01");
        let last = position_of(&terminal, "LINE18");
        let composer_top = position_of(&terminal, "[ Plan ]").y;
        assert!(first.x >= 4, "text should have left padding");
        assert!(first.y >= composer_top + 2, "text should have top padding");
        assert_eq!(last.y, first.y + 17);
        assert!(composer_height(&app, 78) >= 22);
        assert!(regions.transcript_entries.is_empty());
    }

    #[test]
    fn mode_labels_are_not_clickable() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();
        terminal
            .draw(|frame| regions = render(frame, &app, &Theme::default()))
            .unwrap();

        let plan = position_of(&terminal, "[ Plan ]");
        let build = position_of(&terminal, "[ Build ]");
        assert_eq!(regions.target_at(plan.x + 2, plan.y), None);
        assert_eq!(regions.target_at(build.x + 2, build.y), None);
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
            artifacts: Vec::new(),
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
                context_window: None,
            }],
        }]));

        let (screen, _, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Select model"));
        assert!(screen.contains("ChatGPT"));
        assert!(screen.contains("live provider API"));
        assert!(screen.contains("GPT Test"));
        assert!(screen.contains("gpt-test"));
        let refresh = regions.model_refresh.unwrap();
        assert_eq!(
            regions.target_at(refresh.x, refresh.y),
            Some(UiTarget::ModelRefresh)
        );
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
                    context_window: None,
                })
                .collect(),
        }]));

        let (first_screen, _, first_regions, _) = render_to_string(&app, 100, 30);
        assert!(!first_screen.contains("model-29"));
        assert!(first_regions.model_refresh.is_some());

        for _ in 0..29 {
            app.handle_key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
                std::time::Instant::now(),
            );
        }
        let (last_screen, _, last_regions, _) = render_to_string(&app, 100, 30);
        assert!(last_screen.contains("model-29"));
        assert_eq!(app.selected_model_index(), 29);
        assert!(last_regions.model_refresh.is_some());
    }

    #[test]
    fn model_picker_shows_active_hoverable_selection_and_composer_model() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.set_current_model("model-a");
        app.models_dialog = Some(ModelsDialogPhase::Loaded(vec![ProviderModels {
            provider: "ChatGPT".into(),
            source: "live provider API".into(),
            models: vec![
                ModelInfo {
                    id: "model-a".into(),
                    display_name: "Model A".into(),
                    context_window: None,
                },
                ModelInfo {
                    id: "model-b".into(),
                    display_name: "Model B".into(),
                    context_window: None,
                },
            ],
        }]));
        app.set_model_selection(1);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = UiRegions::default();

        terminal
            .draw(|frame| regions = render(frame, &app, &Theme::default()))
            .unwrap();

        let screen = terminal.backend().to_string();
        assert!(screen.contains("✓ Model A"));
        let selected = regions
            .models
            .iter()
            .find(|region| region.index == 1)
            .unwrap();
        assert_eq!(
            regions.target_at(selected.area.x, selected.area.y),
            Some(UiTarget::Model(1))
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .cell(Position::new(selected.area.x, selected.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );

        app.models_dialog = None;
        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &Theme::default());
            })
            .unwrap();
        assert!(terminal.backend().to_string().contains("Model: model-a"));
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
        assert!(
            regions.transcript_entries.is_empty(),
            "the auth owner must hide background hit targets"
        );
        assert!(!cursor_visible);
    }

    #[test]
    fn large_paste_confirmation_hides_cursor_and_background_targets() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "sent".into(), Vec::new());
        app.handle_paste(&"x".repeat(crate::composer::REQUEST_CONFIRM_BYTES + 1));

        let (screen, cursor_visible, regions, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("Large paste"));
        assert!(screen.contains("confirm"));
        assert!(!cursor_visible);
        assert!(regions.transcript_entries.is_empty());
        assert!(regions.suggestions.is_empty());
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
    fn transcript_tail_remains_visible_beyond_u16_rows() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript
            .submit(1, format!("{}TAIL", "row\n".repeat(70_000)), Vec::new());

        let (screen, _, regions, _) = render_to_string(&app, 60, 20);

        assert!(screen.contains("TAIL"));
        assert!(!regions.transcript_entries.is_empty());
        assert!(
            regions
                .transcript_entries
                .iter()
                .all(|region| region.area.height <= 20)
        );
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
            artifacts: Vec::new(),
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
    fn terminal_tools_render_description_command_live_output_and_exit_status() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(3, "run it".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 3 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            call_id: 8,
            name: "terminal".into(),
            summary: "Checking the project".into(),
            artifacts: vec![ToolArtifact::Terminal {
                description: "Checking the project".into(),
                command: "cargo test".into(),
                output: String::new(),
                exit_code: None,
            }],
        });
        app.handle_agent_event(AgentEvent::ToolOutputDelta {
            request_id: 3,
            call_id: 8,
            chunk: "test result: ok".into(),
        });

        let (live, _, _, _) = render_to_string(&app, 100, 30);
        assert!(live.contains("$ cargo test"));
        assert!(live.contains("test result: ok"));
        assert!(!live.contains("exit 0"));

        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 3,
            call_id: 8,
            summary: Some("Exited with 0".into()),
            artifacts: vec![ToolArtifact::Terminal {
                description: "Checking the project".into(),
                command: "cargo test".into(),
                output: "test result: ok".into(),
                exit_code: Some(0),
            }],
        });
        let (screen, _, _, _) = render_to_string(&app, 100, 30);

        assert!(screen.contains("terminal"));
        assert!(screen.contains("Checking the project"));
        assert!(screen.contains("$ cargo test"));
        assert!(screen.contains("test result: ok"));
        assert!(screen.contains("exit 0"));
    }

    #[test]
    fn search_and_diff_tools_use_specialized_content_and_diff_colors() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(3, "change it".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 3 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            call_id: 11,
            name: "edit_file".into(),
            summary: "Editing value.txt".into(),
            artifacts: Vec::new(),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 3,
            call_id: 11,
            summary: None,
            artifacts: vec![ToolArtifact::Patch {
                path: "value.txt".into(),
                diff: "--- value.txt\n+++ value.txt\n-old\n+new".into(),
            }],
        });
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &app, &theme);
            })
            .unwrap();

        assert!(terminal.backend().to_string().contains("Edited value.txt"));
        assert_eq!(
            style_at_text(&terminal, "+new").fg,
            theme.style(ThemeRole::DiffAdded).fg
        );
        assert_eq!(
            style_at_text(&terminal, "-old").fg,
            theme.style(ThemeRole::DiffRemoved).fg
        );

        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 3,
            call_id: 12,
            name: "search_files".into(),
            summary: "Searching for marker".into(),
            artifacts: Vec::new(),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 3,
            call_id: 12,
            summary: None,
            artifacts: vec![ToolArtifact::SearchResults {
                query: "marker".into(),
                matches: "src/main.rs:1:marker".into(),
            }],
        });
        let search_id = app.transcript.entries()[3].id;
        app.activate_transcript_entry(search_id);
        let (screen, _, _, _) = render_to_string(&app, 100, 30);
        assert!(screen.contains("Search /marker/"));
        assert!(screen.contains("src/main.rs:1:marker"));
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
            artifacts: Vec::new(),
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
        let tool_id = app.transcript.entries()[2].id;
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
            vec![crate::workspace::Attachment::workspace_file("src/app.rs")],
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
            vec![crate::workspace::Attachment::workspace_file("src/app.rs")],
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
