use crate::{
    agent::{AgentEvent, RequestId},
    auth::AuthEvent,
    commands::{Command, CommandRegistry},
    composer::{
        ComposerDocument, DocumentRevision, PasteProposal, QueryId, QueryKind, QueryView,
        REQUEST_CONFIRM_BYTES, SubmittedContent,
    },
    llm::ProviderModels,
    model_catalog::ModelCatalogEvent,
    session::SessionMode,
    submission::{DraftId, PreparedRequest, SubmissionEvent},
    theme::ThemeId,
    transcript::{EntryId, EntryKind, ToolArtifact, Transcript, TranscriptEvent},
    workspace::WorkspacePath,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    cell::{Cell, RefCell},
    sync::Arc,
    time::{Duration, Instant},
};

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
    Preflight {
        draft_id: DraftId,
        content: SubmittedContent,
        mode: SessionMode,
    },
    CancelPreflight {
        draft_id: DraftId,
    },
    Submit {
        request_id: RequestId,
        request: Arc<PreparedRequest>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerTarget {
    TranscriptEntry(EntryId),
    AuthProvider(usize),
    Suggestion(usize),
    MessageCopy,
    Theme(usize),
    Model(usize),
    ModelRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerEvent {
    Activate(Option<PointerTarget>),
    Hover(Option<PointerTarget>),
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub description: String,
    pub kind: SuggestionKind,
    file_path: Option<WorkspacePath>,
}

#[derive(Debug, Clone)]
struct SuggestionCache {
    query_id: Option<QueryId>,
    source_revision: u64,
    suggestions: Arc<[Suggestion]>,
}

pub type Composer = ComposerDocument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOwner {
    Auth,
    Message,
    Theme,
    Models,
    PasteConfirmation,
    PendingSubmission,
    Home,
    Suggestions,
    Composer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSubmissionPhase {
    Preflighting,
    Confirming(Arc<PreparedRequest>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSubmission {
    draft_id: DraftId,
    content: SubmittedContent,
    mode: SessionMode,
    approved_bytes: Option<usize>,
    phase: PendingSubmissionPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingSubmissionView {
    Preflighting,
    Confirming { bytes: usize },
}

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
    large_paste_confirmation: Option<PasteProposal>,
    pending_submission: Option<PendingSubmission>,
    approved_draft: Option<(DocumentRevision, usize)>,
    active_theme: ThemeId,
    commands: CommandRegistry,
    workspace_files: Vec<WorkspacePath>,
    indexed_workspace_search: bool,
    indexed_suggestion_query: Option<QueryId>,
    indexed_file_suggestions: Vec<WorkspacePath>,
    pending_indexed_file_selection: Option<QueryId>,
    suggestion_source_revision: u64,
    suggestion_cache: RefCell<Option<SuggestionCache>>,
    suggestion_builds: Cell<usize>,
    suggestion_selected: usize,
    models_selected: usize,
    current_model: String,
    composer_width: u16,
    next_request_id: RequestId,
    next_draft_id: DraftId,
    last_escape: Option<Instant>,
    cancellation_requested: bool,
    auth_only: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            follow_output: true,
            composer_width: u16::MAX,
            next_request_id: 1,
            next_draft_id: 1,
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
        self.workspace_files = files
            .into_iter()
            .map(|path| WorkspacePath::from_raw(path.into()))
            .collect();
        self.workspace_files.sort();
        self.indexed_workspace_search = false;
        self.indexed_suggestion_query = None;
        self.indexed_file_suggestions.clear();
        self.pending_indexed_file_selection = None;
        self.suggestion_selected = 0;
        self.invalidate_suggestions();
    }

    pub(crate) fn use_indexed_workspace_search(&mut self) {
        self.indexed_workspace_search = true;
        self.indexed_suggestion_query = None;
        self.indexed_file_suggestions.clear();
        self.pending_indexed_file_selection = None;
        self.suggestion_selected = 0;
        self.invalidate_suggestions();
    }

    pub(crate) fn workspace_file_query(&self) -> Option<(QueryId, String)> {
        let query = self.composer.active_query()?;
        (query.kind() == QueryKind::FileReference).then(|| (query.id(), query.text().to_owned()))
    }

    pub(crate) fn set_indexed_file_suggestions(
        &mut self,
        query_id: QueryId,
        paths: Vec<WorkspacePath>,
    ) {
        if self.workspace_file_query().map(|(id, _)| id) != Some(query_id) {
            if self.pending_indexed_file_selection == Some(query_id) {
                self.pending_indexed_file_selection = None;
            }
            return;
        }
        let same_query = self.indexed_suggestion_query == Some(query_id);
        let complete_pending_selection = self.pending_indexed_file_selection == Some(query_id);
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
        self.indexed_suggestion_query = Some(query_id);
        self.indexed_file_suggestions = paths;
        self.invalidate_suggestions();
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
            if let Some(path) = self.indexed_file_suggestions.first().cloned() {
                let _ = self.composer.complete_file_reference(query_id, path);
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
        match self.input_owner() {
            InputOwner::Auth => return self.handle_auth_key(key),
            InputOwner::Message => return self.handle_message_dialog_key(key),
            InputOwner::Theme => return self.handle_theme_dialog_key(key),
            InputOwner::Models => return self.handle_models_dialog_key(key),
            InputOwner::PasteConfirmation => return self.handle_large_paste_key(key),
            InputOwner::PendingSubmission => return self.handle_pending_submission_key(key),
            InputOwner::Home => {
                if key.code == KeyCode::Enter {
                    self.screen = Screen::Chat;
                }
                return None;
            }
            InputOwner::Suggestions | InputOwner::Composer => {}
        }

        if key.code == KeyCode::Esc {
            if key.modifiers.contains(KeyModifiers::ALT) {
                return self.cancel_active_request();
            }
            return self.handle_escape(now);
        }
        self.last_escape = None;

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.composer.clear();
            self.approved_draft = None;
            self.pending_indexed_file_selection = None;
            self.suggestion_selected = 0;
            return None;
        }

        if key.code == KeyCode::Enter
            && let Some((query_id, _)) = self.pending_indexed_file_query()
        {
            self.pending_indexed_file_selection = Some(query_id);
            return None;
        }

        if !self.suggestions().is_empty() {
            match key.code {
                KeyCode::Enter => {
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
                self.composer.move_up(self.composer_width as usize);
                None
            }
            KeyCode::Down => {
                self.composer.move_down(self.composer_width as usize);
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
        if !matches!(
            self.input_owner(),
            InputOwner::Composer | InputOwner::Suggestions
        ) {
            return;
        }
        match self.composer.propose_paste(text) {
            Ok(proposal) if proposal.requires_confirmation() => {
                self.large_paste_confirmation = Some(proposal);
            }
            Ok(proposal) => {
                if let Err(error) = self.composer.commit_paste(proposal) {
                    self.notice = Some(error.to_string());
                }
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
        self.pending_indexed_file_selection = None;
        self.suggestion_selected = 0;
        self.last_escape = None;
    }

    pub(crate) fn composer_cursor_visible(&self) -> bool {
        matches!(
            self.input_owner(),
            InputOwner::Composer | InputOwner::Suggestions
        )
    }

    pub(crate) fn paste_confirmation(&self) -> Option<&PasteProposal> {
        self.large_paste_confirmation.as_ref()
    }

    pub(crate) fn pending_submission_view(&self) -> Option<PendingSubmissionView> {
        self.pending_submission
            .as_ref()
            .map(|pending| match &pending.phase {
                PendingSubmissionPhase::Preflighting => PendingSubmissionView::Preflighting,
                PendingSubmissionPhase::Confirming(request) => PendingSubmissionView::Confirming {
                    bytes: request.serialized_bytes(),
                },
            })
    }

    pub(crate) fn handle_pointer(&mut self, event: PointerEvent) -> Option<AppAction> {
        let owner = self.input_owner();
        match event {
            PointerEvent::Activate(target) => match (owner, target) {
                (InputOwner::Auth, Some(PointerTarget::AuthProvider(index))) => {
                    self.set_auth_selection(index);
                    self.select_auth_provider()
                }
                (InputOwner::Message, Some(PointerTarget::MessageCopy)) => {
                    self.copy_message_dialog()
                }
                (InputOwner::Theme, Some(PointerTarget::Theme(index))) => {
                    self.set_theme_selection(index);
                    self.commit_theme_selection()
                }
                (InputOwner::Models, Some(PointerTarget::Model(index))) => {
                    self.activate_model(index)
                }
                (InputOwner::Models, Some(PointerTarget::ModelRefresh)) => self.refresh_models(),
                (InputOwner::Suggestions, Some(PointerTarget::Suggestion(index))) => {
                    self.activate_suggestion(index)
                }
                (InputOwner::Composer, Some(PointerTarget::TranscriptEntry(entry_id))) => {
                    self.activate_transcript_entry(entry_id);
                    None
                }
                _ => None,
            },
            PointerEvent::Hover(target) => {
                match (owner, target) {
                    (InputOwner::Auth, Some(PointerTarget::AuthProvider(index))) => {
                        self.set_auth_selection(index)
                    }
                    (InputOwner::Theme, Some(PointerTarget::Theme(index))) => {
                        self.set_theme_selection(index)
                    }
                    (InputOwner::Models, Some(PointerTarget::Model(index))) => {
                        self.set_model_selection(index)
                    }
                    (InputOwner::Suggestions, Some(PointerTarget::Suggestion(index))) => {
                        self.set_suggestion_selection(index)
                    }
                    _ => {}
                }
                None
            }
            PointerEvent::ScrollUp => {
                match owner {
                    InputOwner::Auth => self.move_auth_selection(-1),
                    InputOwner::Theme => self.move_theme_selection(-1),
                    InputOwner::Models => self.scroll_models_up(),
                    InputOwner::Suggestions => self.move_suggestion_selection(-1),
                    InputOwner::Composer => self.scroll_transcript_up(),
                    InputOwner::Message
                    | InputOwner::PasteConfirmation
                    | InputOwner::PendingSubmission
                    | InputOwner::Home => {}
                }
                None
            }
            PointerEvent::ScrollDown => {
                match owner {
                    InputOwner::Auth => self.move_auth_selection(1),
                    InputOwner::Theme => self.move_theme_selection(1),
                    InputOwner::Models => self.scroll_models_down(),
                    InputOwner::Suggestions => self.move_suggestion_selection(1),
                    InputOwner::Composer => self.scroll_transcript_down(),
                    InputOwner::Message
                    | InputOwner::PasteConfirmation
                    | InputOwner::PendingSubmission
                    | InputOwner::Home => {}
                }
                None
            }
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
        self.session_mode
    }

    pub(crate) fn set_composer_width(&mut self, width: u16) {
        self.composer_width = width.max(1);
    }

    pub fn select_mode(&mut self, mode: SessionMode) {
        self.session_mode = mode;
        self.suggestion_selected = 0;
    }

    fn toggle_mode(&mut self) {
        let mode = match self.effective_mode() {
            SessionMode::Plan => SessionMode::Build,
            SessionMode::Build => SessionMode::Plan,
        };
        self.select_mode(mode);
    }

    fn input_owner(&self) -> InputOwner {
        if self.auth_dialog.is_some() {
            InputOwner::Auth
        } else if self.message_dialog.is_some() {
            InputOwner::Message
        } else if self.theme_dialog.is_some() {
            InputOwner::Theme
        } else if self.models_dialog.is_some() {
            InputOwner::Models
        } else if self.large_paste_confirmation.is_some() {
            InputOwner::PasteConfirmation
        } else if self.pending_submission.is_some() {
            InputOwner::PendingSubmission
        } else if self.screen == Screen::Home {
            InputOwner::Home
        } else if !self.suggestions().is_empty() {
            InputOwner::Suggestions
        } else {
            InputOwner::Composer
        }
    }

    fn handle_large_paste_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                if let Some(proposal) = self.large_paste_confirmation.take() {
                    match self.composer.commit_paste(proposal) {
                        Ok(commit) => {
                            self.approved_draft =
                                Some((self.composer.revision(), commit.projected_bytes));
                        }
                        Err(error) => self.notice = Some(error.to_string()),
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => self.large_paste_confirmation = None,
            _ => {}
        }
        None
    }

    fn handle_pending_submission_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        let pending = self.pending_submission.as_ref()?;
        match (&pending.phase, key.code) {
            (PendingSubmissionPhase::Preflighting, KeyCode::Esc) => {
                let draft_id = pending.draft_id;
                self.pending_submission = None;
                Some(AppAction::CancelPreflight { draft_id })
            }
            (PendingSubmissionPhase::Confirming(_), KeyCode::Enter | KeyCode::Char('y')) => {
                let PendingSubmissionPhase::Confirming(request) =
                    &self.pending_submission.as_ref()?.phase
                else {
                    return None;
                };
                self.accept_prepared_request(request.clone())
            }
            (PendingSubmissionPhase::Confirming(_), KeyCode::Esc | KeyCode::Char('n')) => {
                self.pending_submission = None;
                None
            }
            _ => None,
        }
    }

    pub fn register_command(&mut self, command: impl Command + 'static) {
        self.commands.register(command);
        self.invalidate_suggestions();
    }

    fn pending_indexed_file_query(&self) -> Option<(QueryId, String)> {
        if !self.indexed_workspace_search {
            return None;
        }
        let query = self.workspace_file_query()?;
        (self.indexed_suggestion_query != Some(query.0)).then_some(query)
    }

    pub fn suggestions(&self) -> Arc<[Suggestion]> {
        let query = self.composer.active_query();
        let query_id = query.as_ref().map(QueryView::id);
        if let Some(cache) = self.suggestion_cache.borrow().as_ref()
            && cache.query_id == query_id
            && cache.source_revision == self.suggestion_source_revision
        {
            return cache.suggestions.clone();
        }
        let suggestions: Arc<[Suggestion]> = self.compute_suggestions(query.as_ref()).into();
        self.suggestion_builds
            .set(self.suggestion_builds.get().saturating_add(1));
        *self.suggestion_cache.borrow_mut() = Some(SuggestionCache {
            query_id,
            source_revision: self.suggestion_source_revision,
            suggestions: suggestions.clone(),
        });
        suggestions
    }

    fn compute_suggestions(&self, query: Option<&QueryView>) -> Vec<Suggestion> {
        if let Some(query) = query
            && query.kind() == QueryKind::Command
            && query.is_standalone()
        {
            let commands: Vec<_> = self
                .commands
                .matching(query.text())
                .map(|command| Suggestion {
                    label: format!("/{}", command.name()),
                    description: command.description().to_owned(),
                    kind: SuggestionKind::Command,
                    file_path: None,
                })
                .collect();
            if !commands.is_empty() {
                return commands;
            }
        }

        let Some(query) = query else {
            return Vec::new();
        };
        if query.kind() != QueryKind::FileReference {
            return Vec::new();
        }
        if self.indexed_workspace_search {
            if self.indexed_suggestion_query != Some(query.id()) {
                return Vec::new();
            }
            return self
                .indexed_file_suggestions
                .iter()
                .map(|path| Suggestion {
                    label: path.display(),
                    description: "File".to_owned(),
                    kind: SuggestionKind::File,
                    file_path: Some(path.clone()),
                })
                .collect();
        }
        let query = query.text().to_lowercase();
        self.workspace_files
            .iter()
            .filter(|path| path.display().to_lowercase().contains(&query))
            .take(FILE_SUGGESTION_LIMIT)
            .map(|path| Suggestion {
                label: path.display(),
                description: "File".to_owned(),
                kind: SuggestionKind::File,
                file_path: Some(path.clone()),
            })
            .collect()
    }

    fn invalidate_suggestions(&mut self) {
        self.suggestion_source_revision = self.suggestion_source_revision.wrapping_add(1);
        *self.suggestion_cache.borrow_mut() = None;
    }

    #[cfg(test)]
    fn suggestion_build_count(&self) -> usize {
        self.suggestion_builds.get()
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
                file_path: None,
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
                let query = self.composer.active_query()?;
                if query.kind() != QueryKind::Command || !query.is_standalone() {
                    return None;
                }
                self.composer.discard_active_command(query.id()).ok()?;
                command.execute(self)
            }
            SuggestionKind::File => {
                let query = self.composer.active_query()?;
                let path = suggestion.file_path?;
                self.composer
                    .complete_file_reference(query.id(), path)
                    .ok()?;
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

    pub fn handle_submission_event(&mut self, event: SubmissionEvent) -> Option<AppAction> {
        match event {
            SubmissionEvent::Prepared { draft_id, request } => {
                let pending = self.pending_submission.as_mut()?;
                if pending.draft_id != draft_id
                    || request.content() != &pending.content
                    || request.mode() != pending.mode
                {
                    return None;
                }
                if request.serialized_bytes() > REQUEST_CONFIRM_BYTES
                    && pending
                        .approved_bytes
                        .is_none_or(|approved| request.serialized_bytes() > approved)
                {
                    pending.phase = PendingSubmissionPhase::Confirming(request);
                    None
                } else {
                    self.accept_prepared_request(request)
                }
            }
            SubmissionEvent::Failed { draft_id, message } => {
                if self
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| pending.draft_id == draft_id)
                {
                    self.pending_submission = None;
                    self.notice = Some(message);
                }
                None
            }
            SubmissionEvent::Cancelled { draft_id } => {
                if self
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| pending.draft_id == draft_id)
                {
                    self.pending_submission = None;
                    self.notice = Some("Submission cancelled".into());
                }
                None
            }
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

    pub fn tick(&mut self) -> bool {
        let animation_visible = self.active_request.is_some();
        let previous_frame = self.animation_frame;
        if animation_visible {
            self.animation_frame = self.animation_frame.wrapping_add(1);
        }
        if self
            .last_escape
            .is_some_and(|pressed| pressed.elapsed() > INTERRUPT_WINDOW)
        {
            self.last_escape = None;
        }
        animation_visible
            && (previous_frame / 2 != self.animation_frame / 2
                || previous_frame / 5 != self.animation_frame / 5)
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
        let command_text = self.composer.submission_text();
        if !self.composer.has_structural_atoms()
            && let Some(command) = command_text
                .strip_prefix('/')
                .and_then(|name| self.commands.find(name))
        {
            self.composer.clear();
            return command.execute(self);
        }
        let content = self.composer.freeze();
        if content.is_effectively_empty() {
            return None;
        }

        let mode = self.session_mode;
        let draft_id = self.next_draft_id;
        self.next_draft_id = self.next_draft_id.wrapping_add(1);
        let approved_bytes = self
            .approved_draft
            .filter(|(revision, _)| *revision == self.composer.revision())
            .map(|(_, bytes)| bytes);
        self.pending_submission = Some(PendingSubmission {
            draft_id,
            content: content.clone(),
            mode,
            approved_bytes,
            phase: PendingSubmissionPhase::Preflighting,
        });
        Some(AppAction::Preflight {
            draft_id,
            content,
            mode,
        })
    }

    fn accept_prepared_request(&mut self, request: Arc<PreparedRequest>) -> Option<AppAction> {
        let pending = self.pending_submission.as_ref()?;
        if request.content() != &pending.content || self.composer.freeze() != pending.content {
            self.pending_submission = None;
            self.notice = Some("The pending draft changed before submission".into());
            return None;
        }
        self.composer.clear();
        self.approved_draft = None;
        self.pending_submission = None;
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.transcript
            .submit_content(request_id, request.content().clone());
        if self.follow_output {
            self.scroll_from_bottom = 0;
        }
        Some(AppAction::Submit {
            request_id,
            request,
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
        session::SessionMode,
        theme::ThemeId,
        transcript::{AssistantStatus, EntryKind, ToolArtifact},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn set_indexed_files(app: &mut App, paths: &[&str]) {
        let query_id = app.workspace_file_query().unwrap().0;
        app.set_indexed_file_suggestions(
            query_id,
            paths
                .iter()
                .map(|path| crate::workspace::WorkspacePath::from_raw(*path))
                .collect(),
        );
    }

    fn resolve_preflight(app: &mut App, action: Option<AppAction>) -> Option<AppAction> {
        match action {
            Some(AppAction::Preflight {
                draft_id,
                content,
                mode,
            }) => app.handle_submission_event(crate::submission::SubmissionEvent::Prepared {
                draft_id,
                request: crate::submission::PreparedRequest::for_test(content, mode),
            }),
            action => action,
        }
    }

    fn press_and_preflight(app: &mut App, code: KeyCode) -> Option<AppAction> {
        let action = app.handle_key(key(code), Instant::now());
        resolve_preflight(app, action)
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
        assert!(app.composer.submission_text().is_empty());
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
        app.composer.insert_text("discard me");

        assert_eq!(
            app.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Instant::now(),
            ),
            None
        );
        assert_eq!(app.screen, Screen::Chat);
        assert!(app.composer.submission_text().is_empty());
    }

    #[test]
    fn paste_is_consumed_by_every_non_composer_owner() {
        let mut apps = Vec::new();

        let mut auth = App::new();
        auth.open_auth_dialog();
        apps.push(auth);

        let mut message = App::new();
        message.transcript.submit(1, "sent".into(), Vec::new());
        message.open_message_dialog(message.transcript.entries()[0].id);
        apps.push(message);

        let mut theme = App::new();
        theme.open_theme_dialog();
        apps.push(theme);

        let mut models = App::new();
        models.open_models_dialog();
        apps.push(models);

        let mut large_paste = App::new();
        large_paste.screen = Screen::Chat;
        large_paste.handle_paste(&format!(
            "{}\nend",
            "x".repeat(crate::composer::REQUEST_CONFIRM_BYTES)
        ));
        assert!(large_paste.paste_confirmation().is_some());
        apps.push(large_paste);

        let mut pending_confirmation = App::new();
        pending_confirmation.screen = Screen::Chat;
        pending_confirmation
            .composer
            .insert_text(&"x".repeat(crate::composer::REQUEST_CONFIRM_BYTES + 1));
        let Some(AppAction::Preflight {
            draft_id,
            content,
            mode,
        }) = pending_confirmation.handle_key(key(KeyCode::Enter), Instant::now())
        else {
            panic!("large draft should start preflight");
        };
        pending_confirmation.handle_submission_event(
            crate::submission::SubmissionEvent::Prepared {
                draft_id,
                request: crate::submission::PreparedRequest::for_test(content, mode),
            },
        );
        assert!(matches!(
            pending_confirmation.pending_submission_view(),
            Some(super::PendingSubmissionView::Confirming { .. })
        ));
        apps.push(pending_confirmation);

        for app in &mut apps {
            app.screen = Screen::Chat;
            app.composer.insert_text("unchanged");
            let before = app.composer.freeze();
            app.handle_paste("hidden paste");
            assert_eq!(app.composer.freeze(), before);
        }
    }

    #[test]
    fn pending_preflight_freezes_and_restores_the_unchanged_draft_on_cancel() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("keep this draft");
        let before = app.composer.freeze();

        let Some(AppAction::Preflight { draft_id, .. }) =
            app.handle_key(key(KeyCode::Enter), Instant::now())
        else {
            panic!("Enter should start preflight");
        };
        assert_eq!(app.composer.freeze(), before);
        assert!(!app.composer_cursor_visible());

        app.handle_paste("hidden");
        app.handle_key(key(KeyCode::Char('x')), Instant::now());
        assert_eq!(app.composer.freeze(), before);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc), Instant::now()),
            Some(AppAction::CancelPreflight { draft_id })
        );
        assert_eq!(app.composer.freeze(), before);
        assert!(app.composer_cursor_visible());
    }

    #[test]
    fn failed_preflight_restores_the_draft_without_a_transcript_entry() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("keep this draft");
        let before = app.composer.freeze();
        let Some(AppAction::Preflight { draft_id, .. }) =
            app.handle_key(key(KeyCode::Enter), Instant::now())
        else {
            panic!("Enter should start preflight");
        };

        assert_eq!(
            app.handle_submission_event(crate::submission::SubmissionEvent::Failed {
                draft_id,
                message: "attachment disappeared".into(),
            }),
            None
        );
        assert_eq!(app.composer.freeze(), before);
        assert!(app.transcript.entries().is_empty());
        assert_eq!(app.notice.as_deref(), Some("attachment disappeared"));
    }

    #[test]
    fn multiline_paste_stays_structural_in_transcript_and_copy_projection() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.handle_paste("alpha\r\nbeta");

        assert_eq!(app.composer.visible_text(), "[2 lines pasted]");
        assert_eq!(app.composer.submission_text(), "alpha\nbeta");
        let action = press_and_preflight(&mut app, KeyCode::Enter);
        assert!(matches!(action, Some(AppAction::Submit { .. })));

        let EntryKind::User(message) = &app.transcript.entries()[0].kind else {
            panic!("submission should create a user message");
        };
        assert_eq!(message.copy_text(), "[2 lines pasted]");
        assert_eq!(message.content.submission_text(), "alpha\nbeta");
        assert!(!message.copy_text().contains("alpha"));
    }

    #[test]
    fn prepared_request_above_the_soft_limit_waits_for_confirmation() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer
            .insert_text(&"x".repeat(crate::composer::REQUEST_CONFIRM_BYTES + 1));
        let action = app.handle_key(key(KeyCode::Enter), Instant::now());
        let Some(AppAction::Preflight {
            draft_id,
            content,
            mode,
        }) = action
        else {
            panic!("Enter should start preflight");
        };
        let request = crate::submission::PreparedRequest::for_test(content, mode);

        assert_eq!(
            app.handle_submission_event(crate::submission::SubmissionEvent::Prepared {
                draft_id,
                request,
            }),
            None
        );
        assert!(matches!(
            app.pending_submission_view(),
            Some(super::PendingSubmissionView::Confirming { .. })
        ));
        assert!(!app.composer.is_empty());

        assert!(matches!(
            app.handle_key(key(KeyCode::Enter), Instant::now()),
            Some(AppAction::Submit { .. })
        ));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn attachment_bytes_require_a_new_confirmation_after_large_paste_approval() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "attachment bytes").unwrap();
        let mut app = App::with_files(["note.txt"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@note");
        app.activate_suggestion(0);
        app.handle_paste(&"x".repeat(crate::composer::REQUEST_CONFIRM_BYTES + 1));
        assert!(app.paste_confirmation().is_some());
        app.handle_key(key(KeyCode::Enter), Instant::now());
        assert!(app.paste_confirmation().is_none());

        let Some(AppAction::Preflight {
            draft_id,
            content,
            mode,
        }) = app.handle_key(key(KeyCode::Enter), Instant::now())
        else {
            panic!("Enter should start preflight");
        };
        let mut runner = crate::submission::SubmissionTaskRunner::spawn(root.path().to_owned());
        runner.request(draft_id, content, mode).unwrap();
        let event = loop {
            if let Some(event) = runner.try_event() {
                break event;
            }
            std::thread::sleep(Duration::from_millis(1));
        };

        assert_eq!(app.handle_submission_event(event), None);
        assert!(matches!(
            app.pending_submission_view(),
            Some(super::PendingSubmissionView::Confirming { .. })
        ));
        assert!(!app.composer.is_empty());
        runner.shutdown();
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
        assert_eq!(
            app.composer.submission_text(),
            "please inspect @src/main.rs"
        );
        assert_eq!(app.composer.attachments()[0].path().raw(), "src/main.rs");
    }

    #[test]
    fn suggestions_are_built_once_per_query_revision() {
        let mut app = App::with_files(["src/app.rs", "src/runtime.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src");

        for _ in 0..20 {
            assert_eq!(app.suggestions().len(), 2);
            let _ = app.selected_suggestion();
        }
        assert_eq!(app.suggestion_build_count(), 1);

        app.composer.insert_text("/app");
        assert_eq!(app.suggestions().len(), 1);
        assert_eq!(app.suggestion_build_count(), 2);
    }

    #[test]
    fn indexed_at_query_attaches_the_ranked_file_snapshot() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("please inspect @src/maim");
        set_indexed_files(&mut app, &["src/main.rs"]);

        assert_eq!(app.suggestions()[0].label, "src/main.rs");
        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(
            app.composer.submission_text(),
            "please inspect @src/main.rs"
        );
        assert_eq!(app.composer.attachments()[0].path().raw(), "src/main.rs");
    }

    #[test]
    fn enter_before_indexed_results_arrive_attaches_the_ranked_file_when_ready() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("please inspect @src/maim");
        assert!(app.suggestions().is_empty());

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(app.composer.submission_text(), "please inspect @src/maim");
        assert!(app.composer.attachments().is_empty());

        set_indexed_files(&mut app, &["src/main.rs"]);

        assert_eq!(
            app.composer.submission_text(),
            "please inspect @src/main.rs"
        );
        assert_eq!(app.composer.attachments()[0].path().raw(), "src/main.rs");
    }

    #[test]
    fn late_indexed_suggestions_cannot_replace_the_current_query_snapshot() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src/main");
        let stale_query_id = app.workspace_file_query().unwrap().0;
        set_indexed_files(&mut app, &["src/main.rs"]);
        assert_eq!(app.suggestions()[0].label, "src/main.rs");

        app.composer.clear();
        app.composer.insert_text("@src/runtime");
        app.set_indexed_file_suggestions(
            stale_query_id,
            vec![crate::workspace::WorkspacePath::from_raw("src/main.rs")],
        );

        assert!(app.suggestions().is_empty());
    }

    #[test]
    fn indexed_refresh_preserves_the_selected_file_across_reranking() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src");
        set_indexed_files(&mut app, &["src/app.rs", "src/runtime.rs"]);
        app.set_suggestion_selection(1);

        set_indexed_files(&mut app, &["src/runtime.rs", "src/app.rs"]);

        assert_eq!(app.selected_suggestion(), 0);
        assert_eq!(app.suggestions()[0].label, "src/runtime.rs");
    }

    #[test]
    fn indexed_results_for_a_new_query_reset_selection_to_the_first_file() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.use_indexed_workspace_search();
        app.composer.insert_text("@src");
        set_indexed_files(&mut app, &["src/app.rs", "src/runtime.rs"]);
        app.set_suggestion_selection(1);

        app.composer.insert_text("m");
        set_indexed_files(&mut app, &["src/runtime.rs", "src/main.rs"]);

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

        app.composer.clear();
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
            press_and_preflight(&mut app, KeyCode::Enter),
            Some(AppAction::Submit { request, .. })
                if request.history_prompt() == "please inspect @somebf here"
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
            press_and_preflight(&mut app, KeyCode::Enter),
            Some(AppAction::Submit { request, .. }) if request.history_prompt() == "/auth later"
        ));
    }

    #[test]
    fn tab_selected_mode_is_snapshotted_for_each_submission() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("review this carefully");
        app.handle_key(key(KeyCode::Tab), Instant::now());

        assert!(matches!(
            press_and_preflight(&mut app, KeyCode::Enter),
            Some(AppAction::Submit { request, .. }) if request.mode() == SessionMode::Plan
        ));
        assert_eq!(app.session_mode, SessionMode::Plan);

        app.composer.insert_text("make it now");
        app.handle_key(key(KeyCode::Tab), Instant::now());
        assert!(matches!(
            press_and_preflight(&mut app, KeyCode::Enter),
            Some(AppAction::Submit { request, .. }) if request.mode() == SessionMode::Build
        ));
        assert_eq!(app.session_mode, SessionMode::Build);
    }

    #[test]
    fn tab_switches_mode_without_modifying_text_even_with_suggestions_open() {
        let mut app = App::with_files(["src/app.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("keep @src/");
        assert!(!app.suggestions().is_empty());

        assert_eq!(app.effective_mode(), SessionMode::Build);
        app.select_mode(SessionMode::Build);
        assert_eq!(app.composer.submission_text(), "keep @src/");
        assert_eq!(app.handle_key(key(KeyCode::Tab), Instant::now()), None);
        assert_eq!(app.effective_mode(), SessionMode::Plan);
        assert_eq!(app.composer.submission_text(), "keep @src/");

        assert_eq!(app.handle_key(key(KeyCode::Tab), Instant::now()), None);
        assert_eq!(app.effective_mode(), SessionMode::Build);
        assert_eq!(app.composer.submission_text(), "keep @src/");
    }

    #[test]
    fn plan_and_build_are_not_slash_command_suggestions() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/");

        let labels: Vec<_> = app
            .suggestions()
            .iter()
            .map(|suggestion| suggestion.label.clone())
            .collect();
        assert!(!labels.contains(&"/plan".to_owned()));
        assert!(!labels.contains(&"/build".to_owned()));
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
        assert!(app.composer.submission_text().is_empty());
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
        let action = press_and_preflight(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Submit { request_id: 1, request })
                if request.history_prompt() == "hello"
                    && request.mode() == SessionMode::Build
        ));
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

        assert_eq!(app.composer.submission_text(), "one\ntwo\nthree");
    }

    #[test]
    fn composer_edits_unicode_on_character_boundaries() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("a界b");
        app.handle_key(key(KeyCode::Left), Instant::now());
        app.handle_key(key(KeyCode::Backspace), Instant::now());

        assert_eq!(app.composer.submission_text(), "ab");
    }

    #[test]
    fn up_and_down_follow_soft_wrapped_visual_rows() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.set_composer_width(10);
        app.composer.insert_text("abcdefghij-END");

        app.handle_key(key(KeyCode::Up), Instant::now());
        app.handle_key(key(KeyCode::Char('X')), Instant::now());
        assert_eq!(app.composer.submission_text(), "abcdXefghij-END");

        app.handle_key(key(KeyCode::Down), Instant::now());
        app.handle_key(key(KeyCode::Char('Y')), Instant::now());
        assert_eq!(app.composer.submission_text(), "abcdXefghij-ENDY");
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
        let _ = press_and_preflight(&mut app, KeyCode::Enter);
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
