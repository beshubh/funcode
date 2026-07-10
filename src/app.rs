use crate::agent::{AgentEvent, RequestId};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Queued,
    Thinking,
    Streaming,
    Completed,
    Interrupted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub request_id: RequestId,
    pub prompt: String,
    pub response: String,
    pub response_status: ResponseStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActivity {
    pub request_id: RequestId,
    pub name: String,
    pub summary: String,
}

impl Turn {
    pub(crate) fn queued(request_id: RequestId, prompt: String) -> Self {
        Self {
            request_id,
            prompt,
            response: String::new(),
            response_status: ResponseStatus::Queued,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    Submit {
        request_id: RequestId,
        prompt: String,
    },
    Cancel {
        request_id: RequestId,
    },
    Quit,
}

#[derive(Debug, Clone, Default)]
pub struct Composer {
    text: String,
    cursor: usize,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_text(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
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
    pub turns: Vec<Turn>,
    pub active_request: Option<RequestId>,
    pub animation_frame: usize,
    pub scroll_from_bottom: usize,
    pub follow_output: bool,
    pub thinking_expanded: bool,
    pub tools_expanded: bool,
    pub active_tool: Option<ToolActivity>,
    next_request_id: RequestId,
    last_escape: Option<Instant>,
    cancellation_requested: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            follow_output: true,
            next_request_id: 1,
            ..Self::default()
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Option<AppAction> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(AppAction::Quit);
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
                None
            }
            KeyCode::Delete => {
                self.composer.delete();
                None
            }
            KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(SCROLL_STEP);
                self.follow_output = false;
                None
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(SCROLL_STEP);
                self.follow_output = self.scroll_from_bottom == 0;
                None
            }
            _ => None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.screen == Screen::Chat {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.composer.insert_text(&normalized);
            self.last_escape = None;
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Started { request_id } => {
                if let Some(turn) = self.turn_mut(request_id)
                    && turn.response_status == ResponseStatus::Queued
                {
                    turn.response_status = ResponseStatus::Thinking;
                    self.active_request = Some(request_id);
                    self.cancellation_requested = false;
                    self.thinking_expanded = false;
                    self.tools_expanded = false;
                    self.active_tool = None;
                }
            }
            AgentEvent::TextDelta { request_id, text } => {
                if let Some(turn) = self.turn_mut(request_id)
                    && matches!(
                        turn.response_status,
                        ResponseStatus::Thinking | ResponseStatus::Streaming
                    )
                {
                    turn.response_status = ResponseStatus::Streaming;
                    turn.response.push_str(&text);
                    self.thinking_expanded = false;
                }
            }
            AgentEvent::ToolStarted {
                request_id,
                name,
                summary,
            } => {
                if self.active_request == Some(request_id) {
                    self.active_tool = Some(ToolActivity {
                        request_id,
                        name,
                        summary,
                    });
                    self.tools_expanded = false;
                }
            }
            AgentEvent::ToolFinished { request_id } => {
                if self
                    .active_tool
                    .as_ref()
                    .is_some_and(|tool| tool.request_id == request_id)
                {
                    self.active_tool = None;
                    self.tools_expanded = false;
                }
            }
            AgentEvent::Completed { request_id } => {
                if let Some(turn) = self.turn_mut(request_id)
                    && matches!(
                        turn.response_status,
                        ResponseStatus::Thinking | ResponseStatus::Streaming
                    )
                {
                    turn.response_status = ResponseStatus::Completed;
                }
                self.finish_request(request_id);
            }
            AgentEvent::Interrupted { request_id } => {
                if let Some(turn) = self.turn_mut(request_id)
                    && matches!(
                        turn.response_status,
                        ResponseStatus::Thinking | ResponseStatus::Streaming
                    )
                {
                    turn.response_status = ResponseStatus::Interrupted;
                }
                self.finish_request(request_id);
            }
            AgentEvent::Failed {
                request_id,
                message,
            } => {
                if let Some(turn) = self.turn_mut(request_id) {
                    turn.response_status = ResponseStatus::Failed(message);
                }
                self.finish_request(request_id);
            }
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

    pub fn is_thinking(&self) -> bool {
        self.active_request.is_some_and(|request_id| {
            self.turns.iter().any(|turn| {
                turn.request_id == request_id && turn.response_status == ResponseStatus::Thinking
            })
        })
    }

    pub fn toggle_thinking(&mut self) {
        if self.is_thinking() {
            self.thinking_expanded = !self.thinking_expanded;
        }
    }

    pub fn toggle_tools(&mut self) {
        if self.active_tool.is_some() {
            self.tools_expanded = !self.tools_expanded;
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
        if self.composer.text().trim() == "/exit" {
            self.composer.take();
            return Some(AppAction::Quit);
        }
        if self.composer.text().trim().is_empty() {
            return None;
        }

        let prompt = self.composer.take();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.turns.push(Turn::queued(request_id, prompt.clone()));
        if self.follow_output {
            self.scroll_from_bottom = 0;
        }

        Some(AppAction::Submit { request_id, prompt })
    }

    fn turn_mut(&mut self, request_id: RequestId) -> Option<&mut Turn> {
        self.turns
            .iter_mut()
            .find(|turn| turn.request_id == request_id)
    }

    fn finish_request(&mut self, request_id: RequestId) {
        if self.active_request == Some(request_id) {
            self.active_request = None;
            self.cancellation_requested = false;
            self.last_escape = None;
            self.thinking_expanded = false;
            self.tools_expanded = false;
            self.active_tool = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{App, AppAction, ResponseStatus, Screen};
    use crate::agent::AgentEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
            })
        );
        assert_eq!(app.turns[0].response_status, ResponseStatus::Queued);

        app.handle_agent_event(AgentEvent::Started { request_id: 1 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 1,
            text: "streamed".into(),
        });
        app.handle_agent_event(AgentEvent::Completed { request_id: 1 });

        assert_eq!(app.turns[0].response, "streamed");
        assert_eq!(app.turns[0].response_status, ResponseStatus::Completed);
    }

    #[test]
    fn two_escape_presses_within_500ms_cancel_only_the_active_request() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(super::Turn::queued(7, "prompt".into()));
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
        app.turns.push(super::Turn::queued(4, "prompt".into()));
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
        app.turns.push(super::Turn::queued(9, "prompt".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 9 });
        app.handle_agent_event(AgentEvent::Completed { request_id: 9 });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id: 9,
            text: "late".into(),
        });

        assert!(app.turns[0].response.is_empty());
        assert_eq!(app.turns[0].response_status, ResponseStatus::Completed);
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
        app.turns.push(super::Turn::queued(12, "prompt".into()));
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
    fn tool_activity_can_expand_and_is_removed_when_the_tool_finishes() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.turns.push(super::Turn::queued(13, "inspect".into()));
        app.handle_agent_event(AgentEvent::Started { request_id: 13 });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id: 13,
            name: "read_file".into(),
            summary: "Reading Cargo.toml".into(),
        });

        app.toggle_tools();
        assert!(app.tools_expanded);
        assert_eq!(app.active_tool.as_ref().unwrap().name, "read_file");

        app.handle_agent_event(AgentEvent::ToolFinished { request_id: 13 });
        assert!(app.active_tool.is_none());
        assert!(!app.tools_expanded);
    }
}
