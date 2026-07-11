use crate::{
    agent::{AgentEvent, RequestId},
    auth::AuthEvent,
    commands::{Command, CommandBehavior, CommandRegistry},
    composer::{ComposerDocument, SessionMode},
    llm::ProviderModels,
    model_catalog::ModelCatalogEvent,
    theme::ThemeId,
    transcript::{Attachment, EntryId, EntryKind, ToolArtifact, Transcript, TranscriptEvent},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

const INTERRUPT_WINDOW: Duration = Duration::from_millis(500);
const SCROLL_STEP: usize = 5;
pub(crate) const FILE_SUGGESTION_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Home,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    ChatGptSubscription,
}

impl AuthProvider {
    pub const ALL: [Self; 1] = [Self::ChatGptSubscription];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatGptSubscription => "ChatGPT subscription",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::ChatGptSubscription => "Sign in through your browser",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDialogPhase {
    Selecting,
    Starting,
    WaitingForBrowser {
        authorization_url: String,
        browser_opened: bool,
    },
    Succeeded {
        account_id: Option<String>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDialog {
    pub phase: AuthDialogPhase,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeDialog {
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelsDialogPhase {
    Loading,
    Loaded(Vec<ProviderModels>),
    Failed(String),
}

impl Default for AuthDialog {
    fn default() -> Self {
        Self {
            phase: AuthDialogPhase::Selecting,
            selected: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    Submit {
        request_id: RequestId,
        prompt: String,
        attachments: Vec<Attachment>,
        mode: SessionMode,
    },
    Cancel {
        request_id: RequestId,
    },
    Authenticate {
        provider: AuthProvider,
    },
    CancelAuthentication {
        quit: bool,
    },
    CopyToClipboard {
        text: String,
    },
    ListModels,
    SaveTheme {
        theme_id: ThemeId,
    },
    RefreshModels,
    SelectModel {
        model: String,
    },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Command,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub description: String,
    pub kind: SuggestionKind,
}

pub type Composer = ComposerDocument;

#[derive(Debug, Default)]
pub struct App {
    pub screen: Screen,
    pub composer: Composer,
    pub session_mode: SessionMode,
    pub transcript: Transcript,
    pub active_request: Option<RequestId>,
    pub animation_frame: usize,
    pub scroll_from_bottom: usize,
    pub follow_output: bool,
    pub expanded_entries: Vec<EntryId>,
    pub collapsed_entries: Vec<EntryId>,
    pub message_dialog: Option<EntryId>,
    pub notice: Option<String>,
    pub auth_dialog: Option<AuthDialog>,
    pub theme_dialog: Option<ThemeDialog>,
    pub(crate) models_dialog: Option<ModelsDialogPhase>,
    active_theme: ThemeId,
    commands: CommandRegistry,
    workspace_files: Vec<String>,
    indexed_workspace_search: bool,
    indexed_suggestion_query: Option<String>,
    indexed_file_suggestions: Vec<String>,
    pending_indexed_file_selection: Option<String>,
    suggestion_selected: usize,
    models_selected: usize,
    current_model: String,
    next_request_id: RequestId,
    last_escape: Option<Instant>,
    cancellation_requested: bool,
    auth_only: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            follow_output: true,
            next_request_id: 1,
            commands: CommandRegistry::with_builtins(),
            ..Self::default()
        }
    }

    pub fn with_files<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut app = Self::new();
        app.set_workspace_files(files);
        app
    }

    pub(crate) fn set_workspace_files<I, S>(&mut self, files: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.workspace_files = files.into_iter().map(Into::into).collect();
        self.workspace_files.sort();
        self.indexed_workspace_search = false;
        self.indexed_suggestion_query = None;
        self.indexed_file_suggestions.clear();
        self.pending_indexed_file_selection = None;
        self.suggestion_selected = 0;
    }

    pub(crate) fn use_indexed_workspace_search(&mut self) {
        self.indexed_workspace_search = true;
        self.indexed_suggestion_query = None;
        self.indexed_file_suggestions.clear();
        self.pending_indexed_file_selection = None;
        self.suggestion_selected = 0;
    }

    pub(crate) fn workspace_file_query(&self) -> Option<String> {
        self.composer
            .active_file_query()
            .map(|(_, query)| query.to_owned())
    }

    pub(crate) fn set_indexed_file_suggestions(&mut self, query: String, paths: Vec<String>) {
        if self.workspace_file_query().as_deref() != Some(query.as_str()) {
            if self.pending_indexed_file_selection.as_deref() == Some(query.as_str()) {
                self.pending_indexed_file_selection = None;
            }
            return;
        }
        let same_query = self.indexed_suggestion_query.as_deref() == Some(query.as_str());
        let complete_pending_selection =
            self.pending_indexed_file_selection.as_deref() == Some(query.as_str());
        if same_query && self.indexed_file_suggestions == paths {
            return;
        }
        let selected_path = if same_query {
            self.indexed_file_suggestions
                .get(self.suggestion_selected)
                .cloned()
        } else {
            None
        };
        let previous_selection = self.suggestion_selected;
        self.indexed_suggestion_query = Some(query);
        self.indexed_file_suggestions = paths;
        self.suggestion_selected = if same_query {
            selected_path
                .and_then(|selected| {
                    self.indexed_file_suggestions
                        .iter()
                        .position(|path| path == &selected)
                })
                .unwrap_or_else(|| {
                    previous_selection.min(self.indexed_file_suggestions.len().saturating_sub(1))
                })
        } else {
            0
        };
        if complete_pending_selection {
            self.pending_indexed_file_selection = None;
            if let Some(path) = self.indexed_file_suggestions.first().cloned()
                && let Some((range, _)) = self.composer.active_file_query()
            {
                self.composer.insert_file_reference(range, path);
            }
        }
    }

    pub fn for_auth() -> Self {
        let mut app = Self {
            auth_only: true,
            ..Self::new()
        };
        app.open_auth_dialog();
        app
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Option<AppAction> {
        if self.auth_dialog.is_some() {
            return self.handle_auth_key(key);
        }

        if self.message_dialog.is_some() {
            return self.handle_message_dialog_key(key);
        }

        if self.theme_dialog.is_some() {
            return self.handle_theme_dialog_key(key);
        }

        if self.models_dialog.is_some() {
            return self.handle_models_dialog_key(key);
        }

        if self.screen == Screen::Home {
            if key.code == KeyCode::Enter {
                self.screen = Screen::Chat;
            }
            return None;
        }

        if key.code == KeyCode::Esc {
            if key.modifiers.contains(KeyModifiers::ALT) {
                return self.cancel_active_request();
            }
            return self.handle_escape(now);
        }
        self.last_escape = None;

        if matches!(key.code, KeyCode::Enter | KeyCode::Tab)
            && let Some(query) = self.pending_indexed_file_query()
        {
            self.pending_indexed_file_selection = Some(query);
            return None;
        }

        if !self.suggestions().is_empty() {
            match key.code {
                KeyCode::Enter | KeyCode::Tab => {
                    return self.activate_suggestion(self.selected_suggestion());
                }
                KeyCode::Up => {
                    self.move_suggestion_selection(-1);
                    return None;
                }
                KeyCode::Down => {
                    self.move_suggestion_selection(1);
                    return None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => {
                self.toggle_mode();
                None
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.composer.insert_text("\n");
                None
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_text("\n");
                None
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.composer.insert_text(&character.to_string());
                self.pending_indexed_file_selection = None;
                self.suggestion_selected = 0;
                None
            }
            KeyCode::Left => {
                self.composer.move_left();
                None
            }
            KeyCode::Right => {
                self.composer.move_right();
                None
            }
            KeyCode::Up => {
                self.composer.move_up();
                None
            }
            KeyCode::Down => {
                self.composer.move_down();
                None
            }
            KeyCode::Home => {
                self.composer.move_home();
                None
            }
            KeyCode::End if !self.follow_output => {
                self.scroll_from_bottom = 0;
                self.follow_output = true;
                None
            }
            KeyCode::End => {
                self.composer.move_end();
                None
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                self.pending_indexed_file_selection = None;
                self.suggestion_selected = 0;
                None
            }
            KeyCode::Delete => {
                self.composer.delete();
                self.pending_indexed_file_selection = None;
                self.suggestion_selected = 0;
                None
            }
            KeyCode::PageUp => {
                self.scroll_transcript_up();
                None
            }
            KeyCode::PageDown => {
                self.scroll_transcript_down();
                None
            }
            _ => None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.screen == Screen::Chat && self.auth_dialog.is_none() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.composer.insert_text(&normalized);
            self.pending_indexed_file_selection = None;
            self.suggestion_selected = 0;
            self.last_escape = None;
        }
    }

    pub fn scroll_transcript_up(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(SCROLL_STEP);
        self.follow_output = false;
    }

    pub fn scroll_transcript_down(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(SCROLL_STEP);
        self.follow_output = self.scroll_from_bottom == 0;
    }

    pub fn effective_mode(&self) -> SessionMode {
        self.composer
            .content()
            .requested_mode()
            .unwrap_or(self.session_mode)
    }

    pub fn select_mode(&mut self, mode: SessionMode) {
        if self.effective_mode() == mode {
            return;
        }
        self.composer.set_mode(mode);
        self.suggestion_selected = 0;
    }

    fn toggle_mode(&mut self) {
        let mode = match self.effective_mode() {
            SessionMode::Plan => SessionMode::Build,
            SessionMode::Build => SessionMode::Plan,
        };
        self.select_mode(mode);
    }

    pub fn register_command(&mut self, command: impl Command + 'static) {
        self.commands.register(command);
    }

    fn pending_indexed_file_query(&self) -> Option<String> {
        if !self.indexed_workspace_search {
            return None;
        }
        let query = self.workspace_file_query()?;
        (self.indexed_suggestion_query.as_deref() != Some(query.as_str())).then_some(query)
    }

    pub fn suggestions(&self) -> Vec<Suggestion> {
        if let Some((range, query)) = self.composer.active_command_query() {
            let standalone =
                range.start == 0 && self.composer.cursor() == self.composer.text().len();
            let commands: Vec<_> = self
                .commands
                .matching(query)
                .filter(|command| {
                    standalone || matches!(command.behavior(), CommandBehavior::Mode(_))
                })
                .map(|command| Suggestion {
                    label: format!("/{}", command.name()),
                    description: command.description().to_owned(),
                    kind: SuggestionKind::Command,
                })
                .collect();
            if !commands.is_empty() {
                return commands;
            }
        }

        let Some((_, query)) = self.composer.active_file_query() else {
            return Vec::new();
        };
        if self.indexed_workspace_search {
            if self.indexed_suggestion_query.as_deref() != Some(query) {
                return Vec::new();
            }
            return self
                .indexed_file_suggestions
                .iter()
                .map(|path| Suggestion {
                    label: path.clone(),
                    description: "File".to_owned(),
                    kind: SuggestionKind::File,
                })
                .collect();
        }
        let query = query.to_lowercase();
        self.workspace_files
            .iter()
            .filter(|path| path.to_lowercase().contains(&query))
            .take(FILE_SUGGESTION_LIMIT)
            .map(|path| Suggestion {
                label: path.clone(),
                description: "File".to_owned(),
                kind: SuggestionKind::File,
            })
            .collect()
    }

    pub fn available_commands(&self) -> Vec<Suggestion> {
        self.command_suggestions("")
    }

    fn command_suggestions(&self, query: &str) -> Vec<Suggestion> {
        self.commands
            .matching(query)
            .map(|command| Suggestion {
                label: format!("/{}", command.name()),
                description: command.description().to_owned(),
                kind: SuggestionKind::Command,
            })
            .collect()
    }

    pub fn activate_suggestion(&mut self, index: usize) -> Option<AppAction> {
        let suggestion = self.suggestions().get(index)?.clone();
        match suggestion.kind {
            SuggestionKind::Command => {
                let command = self
                    .commands
                    .find(suggestion.label.trim_start_matches('/'))?;
                let (range, _) = self.composer.active_command_query()?;
                match command.behavior() {
                    CommandBehavior::Immediate => {
                        if range.start != 0 || self.composer.cursor() != self.composer.text().len()
                        {
                            return None;
                        }
                        self.composer.take_submission();
                        command.execute(self)
                    }
                    CommandBehavior::Mode(mode) => {
                        self.composer.insert_mode(range, mode);
                        self.suggestion_selected = 0;
                        None
                    }
                }
            }
            SuggestionKind::File => {
                let (range, _) = self.composer.active_file_query()?;
                self.composer.insert_file_reference(range, suggestion.label);
                self.suggestion_selected = 0;
                None
            }
        }
    }

    pub fn selected_suggestion(&self) -> usize {
        self.suggestion_selected
            .min(self.suggestions().len().saturating_sub(1))
    }

    pub fn set_suggestion_selection(&mut self, index: usize) {
        if index < self.suggestions().len() {
            self.suggestion_selected = index;
        }
    }

    pub fn move_suggestion_selection(&mut self, direction: i8) {
        let count = self.suggestions().len();
        if count == 0 {
            return;
        }
        let current = self.selected_suggestion();
        self.suggestion_selected = if direction < 0 {
            current.checked_sub(1).unwrap_or(count - 1)
        } else {
            (current + 1) % count
        };
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        let (request_id, transcript_event, finishes_request) = match event {
            AgentEvent::Started { request_id } => {
                if self.transcript.is_queued(request_id) {
                    self.active_request = Some(request_id);
                    self.cancellation_requested = false;
                }
                (
                    request_id,
                    TranscriptEvent::Started {
                        turn_id: request_id,
                    },
                    false,
                )
            }
            AgentEvent::TextDelta { request_id, text } => (
                request_id,
                TranscriptEvent::TextDelta {
                    turn_id: request_id,
                    text,
                },
                false,
            ),
            AgentEvent::ReasoningDelta {
                request_id,
                summary,
            } => (
                request_id,
                TranscriptEvent::ReasoningDelta {
                    turn_id: request_id,
                    summary,
                },
                false,
            ),
            AgentEvent::ToolStarted {
                request_id,
                call_id,
                name,
                summary,
                artifacts,
            } => (
                request_id,
                TranscriptEvent::ToolStarted {
                    turn_id: request_id,
                    call_id,
                    name,
                    summary,
                    artifacts,
                },
                false,
            ),
            AgentEvent::ToolOutputDelta {
                request_id,
                call_id,
                chunk,
            } => (
                request_id,
                TranscriptEvent::ToolOutputDelta {
                    turn_id: request_id,
                    call_id,
                    chunk,
                },
                false,
            ),
            AgentEvent::ToolFinished {
                request_id,
                call_id,
                summary,
                artifacts,
            } => (
                request_id,
                TranscriptEvent::ToolFinished {
                    turn_id: request_id,
                    call_id,
                    summary,
                    artifacts,
                },
                false,
            ),
            AgentEvent::ToolFailed {
                request_id,
                call_id,
                message,
            } => (
                request_id,
                TranscriptEvent::ToolFailed {
                    turn_id: request_id,
                    call_id,
                    message,
                },
                false,
            ),
            AgentEvent::Completed { request_id } => (
                request_id,
                TranscriptEvent::Completed {
                    turn_id: request_id,
                },
                true,
            ),
            AgentEvent::Interrupted { request_id } => (
                request_id,
                TranscriptEvent::Interrupted {
                    turn_id: request_id,
                },
                true,
            ),
            AgentEvent::Failed {
                request_id,
                message,
            } => (
                request_id,
                TranscriptEvent::Failed {
                    turn_id: request_id,
                    message,
                },
                true,
            ),
        };
        self.transcript.apply(transcript_event);
        if finishes_request {
            self.finish_request(request_id);
        }
    }

    pub fn handle_auth_event(&mut self, event: AuthEvent) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        match event {
            AuthEvent::BrowserOpened {
                authorization_url,
                browser_opened,
            } => {
                dialog.phase = AuthDialogPhase::WaitingForBrowser {
                    authorization_url,
                    browser_opened,
                };
            }
            AuthEvent::Succeeded { account_id } => {
                dialog.phase = AuthDialogPhase::Succeeded { account_id };
            }
            AuthEvent::Failed { message } => {
                dialog.phase = AuthDialogPhase::Failed { message };
            }
            AuthEvent::Cancelled => self.auth_dialog = None,
        }
    }

    pub fn tick(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
        if self
            .last_escape
            .is_some_and(|pressed| pressed.elapsed() > INTERRUPT_WINDOW)
        {
            self.last_escape = None;
        }
    }

    pub fn toggle_entry(&mut self, entry_id: EntryId) {
        let default_expanded = self.entry_is_expanded_by_default(entry_id);
        if self.entry_is_expanded(entry_id) {
            self.expanded_entries
                .retain(|expanded| *expanded != entry_id);
            if default_expanded && !self.collapsed_entries.contains(&entry_id) {
                self.collapsed_entries.push(entry_id);
            }
        } else {
            self.collapsed_entries
                .retain(|collapsed| *collapsed != entry_id);
            if !default_expanded && !self.expanded_entries.contains(&entry_id) {
                self.expanded_entries.push(entry_id);
            }
        }
    }

    pub fn entry_is_expanded(&self, entry_id: EntryId) -> bool {
        if self.collapsed_entries.contains(&entry_id) {
            false
        } else {
            self.expanded_entries.contains(&entry_id) || self.entry_is_expanded_by_default(entry_id)
        }
    }

    fn entry_is_expanded_by_default(&self, entry_id: EntryId) -> bool {
        self.transcript
            .entries()
            .iter()
            .find(|entry| entry.id == entry_id)
            .is_some_and(|entry| match &entry.kind {
                EntryKind::Tool(tool) => {
                    tool.name == "terminal"
                        || tool.artifacts.iter().any(|artifact| {
                            matches!(
                                artifact,
                                ToolArtifact::Patch { .. } | ToolArtifact::Terminal { .. }
                            )
                        })
                }
                _ => false,
            })
    }

    pub fn open_message_dialog(&mut self, entry_id: EntryId) {
        if self.transcript.user_message(entry_id).is_some() {
            self.message_dialog = Some(entry_id);
        }
    }

    pub fn activate_transcript_entry(&mut self, entry_id: EntryId) {
        let kind = self
            .transcript
            .entries()
            .iter()
            .find(|entry| entry.id == entry_id)
            .map(|entry| &entry.kind);
        match kind {
            Some(EntryKind::User(_)) => self.open_message_dialog(entry_id),
            Some(EntryKind::Reasoning(_) | EntryKind::Tool(_)) => self.toggle_entry(entry_id),
            Some(EntryKind::Assistant(_)) | None => {}
        }
    }

    pub fn close_message_dialog(&mut self) {
        self.message_dialog = None;
    }

    pub fn copy_message_dialog(&mut self) -> Option<AppAction> {
        let entry_id = self.message_dialog?;
        let message = self.transcript.user_message(entry_id)?;
        Some(AppAction::CopyToClipboard {
            text: message.copy_text(),
        })
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn set_active_theme(&mut self, theme_id: ThemeId) {
        self.active_theme = theme_id;
    }

    pub fn effective_theme_id(&self) -> ThemeId {
        self.theme_dialog
            .and_then(|dialog| ThemeId::ALL.get(dialog.selected).copied())
            .unwrap_or(self.active_theme)
    }

    pub fn open_theme_dialog(&mut self) {
        let selected = ThemeId::ALL
            .iter()
            .position(|theme_id| *theme_id == self.active_theme)
            .unwrap_or_default();
        self.theme_dialog = Some(ThemeDialog { selected });
        self.last_escape = None;
    }

    pub fn set_theme_selection(&mut self, index: usize) {
        if index < ThemeId::ALL.len()
            && let Some(dialog) = self.theme_dialog.as_mut()
        {
            dialog.selected = index;
        }
    }

    pub fn move_theme_selection(&mut self, direction: i8) {
        let Some(dialog) = self.theme_dialog.as_mut() else {
            return;
        };
        let count = ThemeId::ALL.len();
        dialog.selected = if direction < 0 {
            dialog.selected.checked_sub(1).unwrap_or(count - 1)
        } else {
            (dialog.selected + 1) % count
        };
    }

    pub fn commit_theme_selection(&mut self) -> Option<AppAction> {
        self.theme_dialog?;
        let theme_id = self.effective_theme_id();
        self.active_theme = theme_id;
        self.theme_dialog = None;
        Some(AppAction::SaveTheme { theme_id })
    }

    pub(crate) fn open_models_dialog(&mut self) {
        self.models_dialog = Some(ModelsDialogPhase::Loading);
        self.models_selected = 0;
        self.last_escape = None;
    }

    pub(crate) fn set_current_model(&mut self, model: impl Into<String>) {
        self.current_model = model.into();
    }

    pub(crate) fn current_model(&self) -> &str {
        &self.current_model
    }

    pub(crate) fn selected_model_index(&self) -> usize {
        self.models_selected
            .min(self.available_models().len().saturating_sub(1))
    }

    pub(crate) fn set_model_selection(&mut self, index: usize) {
        if index < self.available_models().len() {
            self.models_selected = index;
        }
    }

    pub(crate) fn activate_model(&mut self, index: usize) -> Option<AppAction> {
        self.set_model_selection(index);
        self.select_highlighted_model()
    }

    pub(crate) fn refresh_models(&mut self) -> Option<AppAction> {
        if matches!(self.models_dialog, Some(ModelsDialogPhase::Loading)) {
            return None;
        }
        self.models_dialog = Some(ModelsDialogPhase::Loading);
        self.models_selected = 0;
        Some(AppAction::RefreshModels)
    }

    fn move_model_selection(&mut self, direction: isize) {
        let count = self.available_models().len();
        if count == 0 {
            return;
        }
        let current = self.selected_model_index();
        self.models_selected = if direction < 0 {
            current
                .saturating_sub(direction.unsigned_abs())
                .min(count - 1)
        } else {
            current.saturating_add(direction as usize).min(count - 1)
        };
    }

    fn available_models(&self) -> Vec<&crate::llm::ModelInfo> {
        match self.models_dialog.as_ref() {
            Some(ModelsDialogPhase::Loaded(catalogs)) => catalogs
                .iter()
                .flat_map(|catalog| catalog.models.iter())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn select_highlighted_model(&mut self) -> Option<AppAction> {
        let model = self
            .available_models()
            .get(self.selected_model_index())?
            .id
            .clone();
        self.current_model = model.clone();
        self.models_dialog = None;
        Some(AppAction::SelectModel { model })
    }

    pub(crate) fn scroll_models_up(&mut self) {
        self.move_model_selection(-1);
    }

    pub(crate) fn scroll_models_down(&mut self) {
        self.move_model_selection(1);
    }

    pub(crate) fn handle_model_catalog_event(&mut self, event: ModelCatalogEvent) {
        if self.models_dialog.is_none() {
            return;
        }
        self.models_dialog = Some(match event {
            ModelCatalogEvent::Loaded(catalogs) => {
                self.models_selected = catalogs
                    .iter()
                    .flat_map(|catalog| catalog.models.iter())
                    .position(|model| model.id == self.current_model)
                    .unwrap_or(0);
                ModelsDialogPhase::Loaded(catalogs)
            }
            ModelCatalogEvent::Failed(message) => ModelsDialogPhase::Failed(message),
        });
    }

    pub fn open_auth_dialog(&mut self) {
        self.auth_dialog = Some(AuthDialog::default());
        self.last_escape = None;
    }

    pub fn select_auth_provider(&mut self) -> Option<AppAction> {
        let dialog = self.auth_dialog.as_mut()?;
        if dialog.phase != AuthDialogPhase::Selecting {
            return None;
        }
        let provider = *AuthProvider::ALL.get(dialog.selected)?;
        dialog.phase = AuthDialogPhase::Starting;
        Some(AppAction::Authenticate { provider })
    }

    pub fn set_auth_selection(&mut self, index: usize) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        if dialog.phase == AuthDialogPhase::Selecting && index < AuthProvider::ALL.len() {
            dialog.selected = index;
        }
    }

    pub fn move_auth_selection(&mut self, direction: i8) {
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        if dialog.phase != AuthDialogPhase::Selecting {
            return;
        }
        let provider_count = AuthProvider::ALL.len();
        if direction < 0 {
            dialog.selected = if dialog.selected == 0 {
                provider_count - 1
            } else {
                dialog.selected - 1
            };
        } else {
            dialog.selected = if dialog.selected + 1 >= provider_count {
                0
            } else {
                dialog.selected + 1
            };
        }
    }

    fn handle_auth_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Esc => {
                let should_cancel = self.auth_dialog.as_ref().is_some_and(|dialog| {
                    matches!(
                        dialog.phase,
                        AuthDialogPhase::Starting | AuthDialogPhase::WaitingForBrowser { .. }
                    )
                });
                self.auth_dialog = None;
                if should_cancel {
                    Some(AppAction::CancelAuthentication {
                        quit: self.auth_only,
                    })
                } else if self.auth_only {
                    Some(AppAction::Quit)
                } else {
                    None
                }
            }
            KeyCode::Up => {
                self.move_auth_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_auth_selection(1);
                None
            }
            KeyCode::Enter => match self.auth_dialog.as_ref().map(|dialog| &dialog.phase) {
                Some(AuthDialogPhase::Selecting) => self.select_auth_provider(),
                Some(AuthDialogPhase::Succeeded { .. }) => {
                    self.auth_dialog = None;
                    self.auth_only.then_some(AppAction::Quit)
                }
                Some(AuthDialogPhase::Failed { .. }) => {
                    self.open_auth_dialog();
                    None
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn handle_message_dialog_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Esc => {
                self.close_message_dialog();
                None
            }
            KeyCode::Enter | KeyCode::Char('c') => self.copy_message_dialog(),
            _ => None,
        }
    }

    fn handle_theme_dialog_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Esc => {
                self.theme_dialog = None;
                None
            }
            KeyCode::Up => {
                self.move_theme_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_theme_selection(1);
                None
            }
            KeyCode::Enter => self.commit_theme_selection(),
            _ => None,
        }
    }

    fn handle_models_dialog_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Esc => self.models_dialog = None,
            KeyCode::Enter => return self.select_highlighted_model(),
            KeyCode::Up => self.move_model_selection(-1),
            KeyCode::Down => self.move_model_selection(1),
            KeyCode::PageUp => self.move_model_selection(-5),
            KeyCode::PageDown => self.move_model_selection(5),
            KeyCode::Char('r') => return self.refresh_models(),
            _ => {}
        }
        None
    }

    fn handle_escape(&mut self, now: Instant) -> Option<AppAction> {
        self.active_request?;
        if self.cancellation_requested {
            return None;
        }

        if self
            .last_escape
            .is_some_and(|pressed| now.saturating_duration_since(pressed) <= INTERRUPT_WINDOW)
        {
            self.last_escape = None;
            self.cancel_active_request()
        } else {
            self.last_escape = Some(now);
            None
        }
    }

    fn cancel_active_request(&mut self) -> Option<AppAction> {
        let request_id = self.active_request?;
        if self.cancellation_requested {
            return None;
        }
        self.last_escape = None;
        self.cancellation_requested = true;
        Some(AppAction::Cancel { request_id })
    }

    fn submit_composer(&mut self) -> Option<AppAction> {
        if self.composer.content().tokens().is_empty()
            && let Some(command) = self
                .composer
                .text()
                .strip_prefix('/')
                .and_then(|name| self.commands.find(name))
            && command.behavior() == CommandBehavior::Immediate
        {
            self.composer.take_submission();
            return command.execute(self);
        }
        let content = self.composer.content();
        if content.prompt_text().trim().is_empty() {
            return None;
        }

        let content = self.composer.take_submission();
        let mode = content.requested_mode().unwrap_or(self.session_mode);
        self.session_mode = mode;
        let prompt = content.prompt_text();
        let attachments = content.attachments();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.transcript.submit_content(request_id, content);
        if self.follow_output {
            self.scroll_from_bottom = 0;
        }

        Some(AppAction::Submit {
            request_id,
            prompt,
            attachments,
            mode,
        })
    }

    fn finish_request(&mut self, request_id: RequestId) {
        if self.active_request == Some(request_id) {
            self.active_request = None;
            self.cancellation_requested = false;
            self.last_escape = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{App, AppAction, AuthDialogPhase, AuthProvider, Screen, SuggestionKind};
    use crate::{
        agent::AgentEvent,
        auth::AuthEvent,
        commands::Command,
        composer::SessionMode,
        theme::ThemeId,
        transcript::{AssistantStatus, EntryKind, ToolArtifact},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn slash_at_the_start_discovers_and_runs_registered_commands() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/a");

        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].label, "/auth");
        assert_eq!(suggestions[0].kind, SuggestionKind::Command);

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert!(app.auth_dialog.is_some());
        assert!(app.composer.text().is_empty());
    }

    #[test]
    fn models_command_starts_provider_catalog_loading() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/models");

        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::ListModels)
        );
        assert!(matches!(
            app.models_dialog,
            Some(super::ModelsDialogPhase::Loading)
        ));
    }

    #[test]
    fn theme_picker_previews_rolls_back_and_commits_the_selected_theme() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.set_active_theme(ThemeId::FunDark);
        app.composer.insert_text("/theme");

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert!(app.theme_dialog.is_some());
        assert_eq!(app.effective_theme_id(), ThemeId::FunDark);

        app.handle_key(key(KeyCode::Down), Instant::now());
        assert_eq!(app.effective_theme_id(), ThemeId::Midnight);
        app.handle_key(key(KeyCode::Esc), Instant::now());
        assert_eq!(app.effective_theme_id(), ThemeId::FunDark);

        app.open_theme_dialog();
        app.set_theme_selection(3);
        assert_eq!(app.effective_theme_id(), ThemeId::Paper);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::SaveTheme {
                theme_id: ThemeId::Paper
            })
        );
        assert_eq!(app.effective_theme_id(), ThemeId::Paper);
        assert!(app.theme_dialog.is_none());
    }

    #[test]
    fn model_catalog_failure_is_shown_in_the_open_dialog() {
        let mut app = App::new();
        app.open_models_dialog();

        app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Failed(
            "catalog unavailable".into(),
        ));

        assert!(matches!(
            app.models_dialog,
            Some(super::ModelsDialogPhase::Failed(ref message))
                if message == "catalog unavailable"
        ));
    }

    #[test]
    fn closing_models_dialog_ignores_a_late_catalog_result() {
        let mut app = App::new();
        app.open_models_dialog();
        app.handle_key(key(KeyCode::Esc), Instant::now());

        app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Loaded(Vec::new()));

        assert!(app.models_dialog.is_none());
    }

    #[test]
    fn models_dialog_keyboard_selects_a_model_for_the_session() {
        let mut app = App::new();
        app.set_current_model("model-a");
        app.open_models_dialog();
        app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Loaded(vec![
            crate::llm::ProviderModels {
                provider: "Test".into(),
                source: "built-in catalog".into(),
                models: vec![
                    crate::llm::ModelInfo {
                        id: "model-a".into(),
                        display_name: "Model A".into(),
                    },
                    crate::llm::ModelInfo {
                        id: "model-b".into(),
                        display_name: "Model B".into(),
                    },
                ],
            },
        ]));

        assert_eq!(app.selected_model_index(), 0);
        app.handle_key(key(KeyCode::Down), Instant::now());
        assert_eq!(app.selected_model_index(), 1);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::SelectModel {
                model: "model-b".into()
            })
        );
        assert_eq!(app.current_model(), "model-b");
        assert!(app.models_dialog.is_none());
    }

    #[test]
    fn models_dialog_refresh_shortcut_requests_a_fresh_catalog() {
        let mut app = App::new();
        app.open_models_dialog();
        app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Loaded(vec![]));

        assert_eq!(
            app.handle_key(key(KeyCode::Char('r')), Instant::now()),
            Some(AppAction::RefreshModels)
        );
        assert!(matches!(
            app.models_dialog,
            Some(super::ModelsDialogPhase::Loading)
        ));
    }

    #[test]
    fn ctrl_c_does_not_exit_the_chat() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Instant::now(),
            ),
            None
        );
        assert_eq!(app.screen, Screen::Chat);
    }

    #[test]
    fn at_query_at_a_token_boundary_attaches_the_selected_file() {
        let mut app = App::with_files(["Cargo.toml", "src/main.rs", "src/runtime.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("please inspect @src/ma");

        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].label, "src/main.rs");
        assert_eq!(suggestions[0].kind, SuggestionKind::File);

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(app.composer.text(), "please inspect @src/main.rs");
        assert_eq!(app.composer.attachments()[0].path, "src/main.rs");
    }

    #[test]
    fn indexed_at_query_attaches_the_ranked_file_snapshot() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("please inspect @src/maim");
        app.set_indexed_file_suggestions("src/maim".into(), vec!["src/main.rs".into()]);

        assert_eq!(app.suggestions()[0].label, "src/main.rs");
        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(app.composer.text(), "please inspect @src/main.rs");
        assert_eq!(app.composer.attachments()[0].path, "src/main.rs");
    }

    #[test]
    fn enter_before_indexed_results_arrive_attaches_the_ranked_file_when_ready() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("please inspect @src/maim");
        assert!(app.suggestions().is_empty());

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(app.composer.text(), "please inspect @src/maim");
        assert!(app.composer.attachments().is_empty());

        app.set_indexed_file_suggestions("src/maim".into(), vec!["src/main.rs".into()]);

        assert_eq!(app.composer.text(), "please inspect @src/main.rs");
        assert_eq!(app.composer.attachments()[0].path, "src/main.rs");
    }

    #[test]
    fn late_indexed_suggestions_cannot_replace_the_current_query_snapshot() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src/main");
        app.set_indexed_file_suggestions("src/main".into(), vec!["src/main.rs".into()]);
        assert_eq!(app.suggestions()[0].label, "src/main.rs");

        app.composer.take_submission();
        app.composer.insert_text("@src/runtime");
        app.set_indexed_file_suggestions("src/main".into(), vec!["src/main.rs".into()]);

        assert!(app.suggestions().is_empty());
    }

    #[test]
    fn indexed_refresh_preserves_the_selected_file_across_reranking() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src");
        app.set_indexed_file_suggestions(
            "src".into(),
            vec!["src/app.rs".into(), "src/runtime.rs".into()],
        );
        app.set_suggestion_selection(1);

        app.set_indexed_file_suggestions(
            "src".into(),
            vec!["src/runtime.rs".into(), "src/app.rs".into()],
        );

        assert_eq!(app.selected_suggestion(), 0);
        assert_eq!(app.suggestions()[0].label, "src/runtime.rs");
    }

    #[test]
    fn indexed_results_for_a_new_query_reset_selection_to_the_first_file() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src");
        app.set_indexed_file_suggestions(
            "src".into(),
            vec!["src/app.rs".into(), "src/runtime.rs".into()],
        );
        app.set_suggestion_selection(1);

        app.composer.insert_text("m");
        app.set_indexed_file_suggestions(
            "srcm".into(),
            vec!["src/runtime.rs".into(), "src/main.rs".into()],
        );

        assert_eq!(app.selected_suggestion(), 0);
        assert_eq!(app.suggestions()[0].label, "src/runtime.rs");
    }

    #[test]
    fn at_query_does_not_trigger_inside_an_email_like_token() {
        let mut app = App::with_files(["src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("contact me@example.com");

        assert!(app.suggestions().is_empty());
    }

    #[test]
    fn at_query_triggers_at_the_start_or_after_whitespace() {
        let mut app = App::with_files(["src/main.rs"]);
        app.screen = Screen::Chat;

        app.composer.insert_text("@src/ma");
        assert_eq!(app.suggestions()[0].label, "src/main.rs");

        app.composer.take_submission();
        app.composer.insert_text("inspect @src/ma");
        assert_eq!(app.suggestions()[0].label, "src/main.rs");
    }

    #[test]
    fn unmatched_at_text_is_submitted_as_plain_text() {
        let mut app = App::with_files(["src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("please inspect @somebf here");

        assert!(app.suggestions().is_empty());
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit { prompt, .. }) if prompt == "please inspect @somebf here"
        ));
    }

    #[test]
    fn arrow_keys_choose_a_command_and_slash_text_with_arguments_submits_normally() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/");

        app.handle_key(key(KeyCode::Down), Instant::now());
        assert_eq!(app.selected_suggestion(), 1);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Quit)
        );

        app.composer.insert_text("/auth later");
        assert!(app.suggestions().is_empty());
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit { prompt, .. }) if prompt == "/auth later"
        ));
    }

    #[test]
    fn plan_and_build_tokens_snapshot_the_session_mode_for_each_submission() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("review this /plan");
        assert_eq!(app.suggestions()[0].label, "/plan");
        app.activate_suggestion(0);
        app.composer.insert_text(" carefully");

        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit {
                mode: SessionMode::Plan,
                ..
            })
        ));
        assert_eq!(app.session_mode, SessionMode::Plan);

        app.composer.insert_text("follow up");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit {
                mode: SessionMode::Plan,
                ..
            })
        ));

        app.composer.insert_text("/build");
        app.activate_suggestion(0);
        app.composer.insert_text(" make it now");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit {
                mode: SessionMode::Build,
                ..
            })
        ));
        assert_eq!(app.session_mode, SessionMode::Build);
    }

    #[test]
    fn tab_switches_the_effective_mode_without_stealing_suggestion_completion() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        assert_eq!(app.effective_mode(), SessionMode::Build);
        app.select_mode(SessionMode::Build);
        assert!(app.composer.text().is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Tab), Instant::now()), None);
        assert_eq!(app.effective_mode(), SessionMode::Plan);
        assert_eq!(
            app.composer.content().requested_mode(),
            Some(SessionMode::Plan)
        );

        assert_eq!(app.handle_key(key(KeyCode::Tab), Instant::now()), None);
        assert_eq!(app.effective_mode(), SessionMode::Build);

        app.composer.take_submission();
        app.composer.insert_text("/plan");
        assert_eq!(app.handle_key(key(KeyCode::Tab), Instant::now()), None);
        assert_eq!(app.effective_mode(), SessionMode::Plan);
        assert_eq!(app.composer.text(), "[Plan]");
    }

    #[test]
    fn selecting_plan_twice_keeps_one_inline_mode_token() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/plan");
        app.activate_suggestion(0);
        app.composer.insert_text(" /plan");
        app.activate_suggestion(0);

        assert_eq!(app.composer.text(), "[Plan] ");
        assert_eq!(
            app.composer
                .content()
                .tokens()
                .iter()
                .filter(|token| matches!(token.kind, crate::composer::InlineTokenKind::Mode(_)))
                .count(),
            1
        );
    }

    #[derive(Debug)]
    struct ToggleToolsCommand;

    impl Command for ToggleToolsCommand {
        fn name(&self) -> &'static str {
            "toggle-tools"
        }

        fn description(&self) -> &'static str {
            "Toggle tools"
        }

        fn execute(&self, app: &mut App) -> Option<AppAction> {
            app.set_notice("command executed");
            None
        }
    }

    #[test]
    fn a_new_command_only_needs_a_trait_implementation_and_registration() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.register_command(ToggleToolsCommand);
        app.composer.insert_text("/toggle-tools");

        app.handle_key(key(KeyCode::Enter), Instant::now());

        assert_eq!(app.notice.as_deref(), Some("command executed"));
        assert!(app.composer.text().is_empty());
    }

    #[test]
    fn auth_command_opens_the_provider_picker_without_submitting_a_prompt() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/auth");

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert!(app.transcript.entries().is_empty());
        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| &dialog.phase),
            Some(&AuthDialogPhase::Selecting)
        );
    }

    #[test]
    fn enter_selects_chatgpt_subscription_and_escape_closes_the_picker() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.open_auth_dialog();

        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Authenticate {
                provider: AuthProvider::ChatGptSubscription,
            })
        );

        app.open_auth_dialog();
        assert_eq!(app.handle_key(key(KeyCode::Esc), Instant::now()), None);
        assert!(app.auth_dialog.is_none());
    }

    #[test]
    fn browser_auth_events_are_reflected_in_the_dialog() {
        let mut app = App::new();
        app.open_auth_dialog();
        app.select_auth_provider();

        app.handle_auth_event(AuthEvent::BrowserOpened {
            authorization_url: "https://example.test/sign-in".into(),
            browser_opened: true,
        });
        assert!(matches!(
            app.auth_dialog.as_ref().map(|dialog| &dialog.phase),
            Some(AuthDialogPhase::WaitingForBrowser { .. })
        ));

        app.handle_auth_event(AuthEvent::Succeeded {
            account_id: Some("workspace-123".into()),
        });
        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| &dialog.phase),
            Some(&AuthDialogPhase::Succeeded {
                account_id: Some("workspace-123".into()),
            })
        );
    }

    #[test]
    fn auth_only_mode_starts_in_the_picker_and_exits_after_success() {
        let mut app = App::for_auth();
        assert!(app.auth_dialog.is_some());

        app.select_auth_provider();
        app.handle_auth_event(AuthEvent::Succeeded { account_id: None });
        assert_eq!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Quit)
        );
    }

    #[test]
    fn provider_picker_accepts_arrow_navigation_with_one_provider() {
        let mut app = App::new();
        app.open_auth_dialog();

        app.handle_key(key(KeyCode::Down), Instant::now());
        app.handle_key(key(KeyCode::Up), Instant::now());

        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| dialog.selected),
            Some(0)
        );
    }

    #[test]
    fn a_submitted_prompt_is_updated_by_correlated_stream_events() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Enter), Instant::now());
        assert_eq!(app.screen, Screen::Chat);

        app.composer.insert_text("hello");
        let action = app.handle_key(key(KeyCode::Enter), Instant::now());
        assert_eq!(
            action,
            Some(AppAction::Submit {
                request_id: 1,
                prompt: "hello".into(),
                attachments: Vec::new(),
                mode: SessionMode::Build,
            })
        );
        assert!(matches!(
            &app.transcript.entries()[1].kind,
            EntryKind::Assistant(message) if message.status == AssistantStatus::Queued
        ));

        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 1,
            text: "streamed".into(),
        });
        app.handle_agent_event(AgentEvent::Completed { request_id: 1 });

        assert!(matches!(
            &app.transcript.entries()[2].kind,
            EntryKind::Assistant(message)
                if message.text == "streamed" && message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn two_escape_presses_within_500ms_cancel_only_the_active_request() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(7, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 7 });
        let start = Instant::now();

        assert_eq!(app.handle_key(key(KeyCode::Esc), start), None);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc), start + Duration::from_millis(499)),
            Some(AppAction::Cancel { request_id: 7 })
        );
    }

    #[test]
    fn shift_enter_and_ctrl_j_insert_newlines_without_submitting() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("one");

        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                Instant::now()
            ),
            None
        );
        app.composer.insert_text("two");
        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                Instant::now()
            ),
            None
        );
        app.composer.insert_text("three");

        assert_eq!(app.composer.text(), "one\ntwo\nthree");
    }

    #[test]
    fn composer_edits_unicode_on_character_boundaries() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("a界b");
        app.handle_key(key(KeyCode::Left), Instant::now());
        app.handle_key(key(KeyCode::Backspace), Instant::now());

        assert_eq!(app.composer.text(), "ab");
    }

    #[test]
    fn an_expired_or_broken_escape_sequence_does_not_cancel() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(4, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 4 });
        let start = Instant::now();

        app.handle_key(key(KeyCode::Esc), start);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc), start + Duration::from_millis(501)),
            None
        );
        app.handle_key(key(KeyCode::Char('x')), start + Duration::from_millis(510));
        assert_eq!(
            app.handle_key(key(KeyCode::Esc), start + Duration::from_millis(520)),
            None
        );
    }

    #[test]
    fn completed_turns_ignore_late_stream_events() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(9, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 9 });
        app.handle_agent_event(AgentEvent::Completed { request_id: 9 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 9,
            text: "late".into(),
        });

        assert!(matches!(
            &app.transcript.entries()[2].kind,
            EntryKind::Assistant(message)
                if message.text.is_empty() && message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn late_started_event_does_not_reactivate_a_completed_turn() {
        let mut app = App::new();
        app.transcript.submit(9, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 9 });
        app.handle_agent_event(AgentEvent::Completed { request_id: 9 });
        app.handle_agent_event(AgentEvent::Started { request_id: 9 });

        assert!(app.active_request.is_none());
        assert!(matches!(
            &app.transcript.entries()[2].kind,
            EntryKind::Assistant(message) if message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn page_up_stops_auto_follow_and_end_resumes_it() {
        let mut app = App::new();
        app.screen = Screen::Chat;

        app.handle_key(key(KeyCode::PageUp), Instant::now());
        assert!(!app.follow_output);
        assert_eq!(app.scroll_from_bottom, 5);

        app.handle_key(key(KeyCode::End), Instant::now());
        assert!(app.follow_output);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn legacy_alt_escape_encoding_interrupts_the_active_request() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(12, "prompt".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 12 });

        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT),
                Instant::now()
            ),
            Some(AppAction::Cancel { request_id: 12 })
        );
    }

    #[test]
    fn tool_activity_persists_and_can_expand_after_the_tool_finishes() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(13, "inspect".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 13 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 13,
            call_id: 4,
            name: "read_file".into(),
            summary: "Reading Cargo.toml".into(),
            artifacts: Vec::new(),
        });

        let tool_id = app.transcript.entries()[2].id;
        app.activate_transcript_entry(tool_id);
        assert!(app.entry_is_expanded(tool_id));

        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 13,
            call_id: 4,
            summary: None,
            artifacts: vec![ToolArtifact::FileReference("Cargo.toml".into())],
        });
        assert!(matches!(
            &app.transcript.entries()[2].kind,
            EntryKind::Tool(tool) if tool.artifacts.len() == 1
        ));
    }

    #[test]
    fn terminal_and_diff_tools_are_expanded_by_default_but_other_tools_are_not() {
        let mut app = App::new();
        app.transcript
            .submit(14, "change and verify".into(), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id: 14 });

        for (call_id, name) in [(1, "read_file"), (2, "edit_file"), (3, "terminal")] {
            app.handle_agent_event(AgentEvent::ToolStarted {
                request_id: 14,
                call_id,
                name: name.into(),
                summary: name.into(),
                artifacts: Vec::new(),
            });
        }
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 14,
            call_id: 1,
            summary: None,
            artifacts: vec![ToolArtifact::FileReference("src/app.rs".into())],
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 14,
            call_id: 2,
            summary: None,
            artifacts: vec![ToolArtifact::Patch {
                path: "src/app.rs".into(),
                diff: "-old\n+new".into(),
            }],
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 14,
            call_id: 3,
            summary: None,
            artifacts: vec![ToolArtifact::Terminal {
                description: "Run tests".into(),
                command: "cargo test".into(),
                output: "ok".into(),
                exit_code: Some(0),
            }],
        });

        let tool_ids = app
            .transcript
            .entries()
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Tool(_)))
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert!(!app.entry_is_expanded(tool_ids[0]));
        assert!(app.entry_is_expanded(tool_ids[1]));
        assert!(app.entry_is_expanded(tool_ids[2]));

        app.activate_transcript_entry(tool_ids[1]);
        assert!(!app.entry_is_expanded(tool_ids[1]));
    }

    #[test]
    fn clicking_a_user_message_opens_a_copyable_modal_with_its_attachments() {
        let mut app = App::with_files(["src/lib.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("Review @src/lib.rs");
        app.activate_suggestion(0);
        let _ = app.handle_key(key(KeyCode::Enter), Instant::now());
        let user_entry = app.transcript.entries()[0].id;

        app.activate_transcript_entry(user_entry);
        assert_eq!(app.message_dialog, Some(user_entry));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c')), Instant::now()),
            Some(AppAction::CopyToClipboard {
                text: "Review @src/lib.rs".into(),
            })
        );

        assert_eq!(app.handle_key(key(KeyCode::Esc), Instant::now()), None);
        assert!(app.message_dialog.is_none());
    }
}
