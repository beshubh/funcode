use crate::{
    agent::{AgentEvent, RequestId},
    auth::AuthEvent,
    commands::{Command, CommandRegistry},
    transcript::{Attachment, EntryId, EntryKind, Transcript, TranscriptEvent},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

const INTERRUPT_WINDOW: Duration = Duration::from_millis(500);
const SCROLL_STEP: usize = 5;

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

#[derive(Debug, Clone, Default)]
pub struct Composer {
    text: String,
    cursor: usize,
    attachments: Vec<Attachment>,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    pub fn insert_text(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn take_submission(&mut self) -> (String, Vec<Attachment>) {
        (self.take(), std::mem::take(&mut self.attachments))
    }

    fn attach_file(&mut self, path: String) {
        if !self
            .attachments
            .iter()
            .any(|attachment| attachment.path == path)
        {
            self.attachments.push(Attachment::workspace_file(path));
        }
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
    }

    fn move_up(&mut self) {
        let current_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if current_start == 0 {
            return;
        }

        let column = self.text[current_start..self.cursor].chars().count();
        let previous_end = current_start - 1;
        let previous_start = self.text[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = previous_start
            + byte_index_at_character(&self.text[previous_start..previous_end], column);
    }

    fn move_down(&mut self) {
        let current_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = self.text[current_start..self.cursor].chars().count();
        let Some(end_offset) = self.text[self.cursor..].find('\n') else {
            return;
        };
        let next_start = self.cursor + end_offset + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |index| next_start + index);
        self.cursor =
            next_start + byte_index_at_character(&self.text[next_start..next_end], column);
    }

    fn backspace(&mut self) {
        let old_cursor = self.cursor;
        self.move_left();
        if self.cursor != old_cursor {
            self.text.drain(self.cursor..old_cursor);
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.text
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        self.text.replace_range(range.clone(), replacement);
        self.cursor = range.start + replacement.len();
    }
}

fn byte_index_at_character(text: &str, character_index: usize) -> usize {
    text.char_indices()
        .nth(character_index)
        .map_or(text.len(), |(index, _)| index)
}

#[derive(Debug, Default)]
pub struct App {
    pub screen: Screen,
    pub composer: Composer,
    pub transcript: Transcript,
    pub active_request: Option<RequestId>,
    pub animation_frame: usize,
    pub scroll_from_bottom: usize,
    pub follow_output: bool,
    pub expanded_entries: Vec<EntryId>,
    pub message_dialog: Option<EntryId>,
    pub notice: Option<String>,
    pub auth_dialog: Option<AuthDialog>,
    commands: CommandRegistry,
    workspace_files: Vec<String>,
    suggestion_selected: usize,
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
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(AppAction::Quit);
        }

        if self.auth_dialog.is_some() {
            return self.handle_auth_key(key);
        }

        if self.message_dialog.is_some() {
            return self.handle_message_dialog_key(key);
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
                self.suggestion_selected = 0;
                None
            }
            KeyCode::Delete => {
                self.composer.delete();
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

    pub fn register_command(&mut self, command: impl Command + 'static) {
        self.commands.register(command);
    }

    pub fn suggestions(&self) -> Vec<Suggestion> {
        let text = self.composer.text();
        if self.composer.cursor() == text.len()
            && text.starts_with('/')
            && !text.chars().any(char::is_whitespace)
        {
            let commands: Vec<_> = self
                .commands
                .matching(&text[1..])
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

        let Some((_, query)) = active_file_query(text, self.composer.cursor()) else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        self.workspace_files
            .iter()
            .filter(|path| path.to_lowercase().contains(&query))
            .take(8)
            .map(|path| Suggestion {
                label: path.clone(),
                description: "File".to_owned(),
                kind: SuggestionKind::File,
            })
            .collect()
    }

    pub fn activate_suggestion(&mut self, index: usize) -> Option<AppAction> {
        let suggestion = self.suggestions().get(index)?.clone();
        match suggestion.kind {
            SuggestionKind::Command => {
                self.composer.take();
                self.commands
                    .find(suggestion.label.trim_start_matches('/'))?
                    .execute(self)
            }
            SuggestionKind::File => {
                let (range, _) = active_file_query(self.composer.text(), self.composer.cursor())?;
                self.composer.replace_range(range, "");
                self.composer.attach_file(suggestion.label);
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
            } => (
                request_id,
                TranscriptEvent::ToolStarted {
                    turn_id: request_id,
                    call_id,
                    name,
                    summary,
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
        if let Some(index) = self
            .expanded_entries
            .iter()
            .position(|expanded| *expanded == entry_id)
        {
            self.expanded_entries.remove(index);
        } else {
            self.expanded_entries.push(entry_id);
        }
    }

    pub fn entry_is_expanded(&self, entry_id: EntryId) -> bool {
        self.expanded_entries.contains(&entry_id)
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
        let command = self
            .composer
            .text()
            .strip_prefix('/')
            .and_then(|name| self.commands.find(name));
        if let Some(command) = command {
            self.composer.take();
            return command.execute(self);
        }
        if self.composer.text().trim().is_empty() {
            return None;
        }

        let (prompt, attachments) = self.composer.take_submission();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.transcript
            .submit(request_id, prompt.clone(), attachments.clone());
        if self.follow_output {
            self.scroll_from_bottom = 0;
        }

        Some(AppAction::Submit {
            request_id,
            prompt,
            attachments,
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

fn active_file_query(text: &str, cursor: usize) -> Option<(std::ops::Range<usize>, &str)> {
    let start = text[..cursor].rfind('@')?;
    let is_token_start = start == 0
        || text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
    if !is_token_start {
        return None;
    }
    let query = &text[start + 1..cursor];
    (!query.chars().any(char::is_whitespace)).then_some((start..cursor, query))
}

#[cfg(test)]
mod tests {
    use super::{App, AppAction, AuthDialogPhase, AuthProvider, Screen, SuggestionKind};
    use crate::{
        agent::AgentEvent,
        auth::AuthEvent,
        commands::Command,
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
    fn at_query_at_a_token_boundary_attaches_the_selected_file() {
        let mut app = App::with_files(["Cargo.toml", "src/main.rs", "src/runtime.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("please inspect @src/ma");

        let suggestions = app.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].label, "src/main.rs");
        assert_eq!(suggestions[0].kind, SuggestionKind::File);

        assert_eq!(app.handle_key(key(KeyCode::Enter), Instant::now()), None);
        assert_eq!(app.composer.text(), "please inspect ");
        assert_eq!(app.composer.attachments()[0].path, "src/main.rs");
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

        app.composer.take();
        app.composer.insert_text("inspect @src/ma");
        assert_eq!(app.suggestions()[0].label, "src/main.rs");
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
            &app.transcript.entries()[1].kind,
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
            &app.transcript.entries()[1].kind,
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
            &app.transcript.entries()[1].kind,
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
        });

        let tool_id = app.transcript.entries()[3].id;
        app.activate_transcript_entry(tool_id);
        assert!(app.entry_is_expanded(tool_id));

        app.handle_agent_event(AgentEvent::ToolFinished {
            request_id: 13,
            call_id: 4,
            summary: None,
            artifacts: vec![ToolArtifact::FileReference("Cargo.toml".into())],
        });
        assert!(matches!(
            &app.transcript.entries()[3].kind,
            EntryKind::Tool(tool) if tool.artifacts.len() == 1
        ));
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
                text: "Review \n\nAttached files:\n- src/lib.rs".into(),
            })
        );

        assert_eq!(app.handle_key(key(KeyCode::Esc), Instant::now()), None);
        assert!(app.message_dialog.is_none());
    }
}
