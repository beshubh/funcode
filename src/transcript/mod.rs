use crate::composer::SubmittedContent;
use crate::workspace::{Attachment, WorkspacePath};
use std::fmt;

pub type EntryId = u64;
pub type TurnId = u64;
pub type ToolCallId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub content: SubmittedContent,
}

impl UserMessage {
    pub fn copy_text(&self) -> String {
        self.content.visible_text()
    }

    pub fn attachments(&self) -> Vec<Attachment> {
        self.content.attachments()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantStatus {
    Queued,
    Thinking,
    Streaming,
    Completed,
    Interrupted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantMessage {
    pub text: String,
    pub status: AssistantStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Completed,
    Interrupted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reasoning {
    pub summary: String,
    pub status: ActivityStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolArtifact {
    CodeRange {
        path: WorkspacePath,
        // start_line and end_line needs to be Optional using Option<u32>
        start_line: u32,
        end_line: u32,
        preview: Option<String>,
    },
    Patch {
        path: WorkspacePath,
        diff: String,
    },
    SearchResults {
        query: String,
        matches: String,
    },
    Terminal {
        description: String,
        command: String,
        output: String,
        exit_code: Option<i32>,
    },
    TextDetail(String),
    FileReference(WorkspacePath),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub name: String,
    pub summary: String,
    pub status: ActivityStatus,
    pub artifacts: Vec<ToolArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAttempt {
    pub attempt: usize,
    pub max_retries: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    User(UserMessage),
    Assistant(AssistantMessage),
    Reasoning(Reasoning),
    Tool(ToolCall),
    Retry(RetryAttempt),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub turn_id: TurnId,
    pub kind: EntryKind,
    revision: u64,
}

impl Entry {
    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    Started {
        turn_id: TurnId,
    },
    TextDelta {
        turn_id: TurnId,
        text: String,
    },
    ReasoningDelta {
        turn_id: TurnId,
        summary: String,
    },
    Retrying {
        turn_id: TurnId,
        attempt: usize,
        max_retries: usize,
        message: String,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: ToolCallId,
        name: String,
        summary: String,
        artifacts: Vec<ToolArtifact>,
    },
    ToolOutputDelta {
        turn_id: TurnId,
        call_id: ToolCallId,
        chunk: String,
    },
    ToolFinished {
        turn_id: TurnId,
        call_id: ToolCallId,
        summary: Option<String>,
        artifacts: Vec<ToolArtifact>,
    },
    ToolFailed {
        turn_id: TurnId,
        call_id: ToolCallId,
        message: String,
    },
    Completed {
        turn_id: TurnId,
    },
    Interrupted {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        message: String,
    },
}

#[derive(Debug, Default, Clone)]
pub struct Transcript {
    entries: Vec<Entry>,
    next_entry_id: EntryId,
}

impl Transcript {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_queued(&self, turn_id: TurnId) -> bool {
        self.entries.iter().rev().any(|entry| {
            entry.turn_id == turn_id
                && matches!(
                    entry.kind,
                    EntryKind::Assistant(AssistantMessage {
                        status: AssistantStatus::Queued,
                        ..
                    })
                )
        })
    }

    pub fn submit(&mut self, turn_id: TurnId, text: String, attachments: Vec<Attachment>) {
        let content = SubmittedContent::with_attachments(text, &attachments);
        self.push_submission(turn_id, content);
    }

    pub fn submit_content(&mut self, turn_id: TurnId, content: SubmittedContent) {
        self.push_submission(turn_id, content);
    }

    fn push_submission(&mut self, turn_id: TurnId, content: SubmittedContent) {
        self.push(turn_id, EntryKind::User(UserMessage { content }));
        self.push(
            turn_id,
            EntryKind::Assistant(AssistantMessage {
                text: String::new(),
                status: AssistantStatus::Queued,
            }),
        );
    }

    pub fn apply(&mut self, event: TranscriptEvent) {
        match event {
            TranscriptEvent::Started { turn_id } => {
                if self.update_assistant(turn_id, |assistant| {
                    if assistant.status != AssistantStatus::Queued {
                        return false;
                    }
                    assistant.status = AssistantStatus::Thinking;
                    true
                }) {
                    self.ensure_reasoning(turn_id);
                }
            }
            TranscriptEvent::TextDelta { turn_id, text } => {
                if self.update_assistant(turn_id, |assistant| {
                    if !matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    ) {
                        return false;
                    }
                    assistant.status = AssistantStatus::Streaming;
                    assistant.text.push_str(&text);
                    true
                }) {
                    self.finish_reasoning(turn_id, ActivityStatus::Completed);
                }
            }
            TranscriptEvent::ReasoningDelta { turn_id, summary } => {
                if self.is_active(turn_id) {
                    self.ensure_reasoning(turn_id);
                    self.update_reasoning(turn_id, |reasoning| {
                        reasoning.summary.push_str(&summary);
                        true
                    });
                }
            }
            TranscriptEvent::Retrying {
                turn_id,
                attempt,
                max_retries,
                message,
            } => {
                if self.is_active(turn_id) {
                    self.insert_activity(
                        turn_id,
                        EntryKind::Retry(RetryAttempt {
                            attempt,
                            max_retries,
                            message,
                        }),
                    );
                }
            }
            TranscriptEvent::ToolStarted {
                turn_id,
                call_id,
                name,
                summary,
                artifacts,
            } => {
                if self.is_active(turn_id) && !self.has_tool(turn_id, call_id) {
                    self.insert_activity(
                        turn_id,
                        EntryKind::Tool(ToolCall {
                            call_id,
                            name,
                            summary,
                            status: ActivityStatus::Running,
                            artifacts,
                        }),
                    );
                }
            }
            TranscriptEvent::ToolOutputDelta {
                turn_id,
                call_id,
                chunk,
            } => {
                if self.is_active(turn_id) {
                    self.update_tool(turn_id, call_id, |tool| {
                        let Some(ToolArtifact::Terminal { output, .. }) =
                            tool.artifacts.first_mut()
                        else {
                            return false;
                        };
                        output.push_str(&chunk);
                        true
                    });
                }
            }
            TranscriptEvent::ToolFinished {
                turn_id,
                call_id,
                summary,
                artifacts,
            } => {
                if self.is_active(turn_id) {
                    self.update_tool(turn_id, call_id, |tool| {
                        if let Some(summary) = summary {
                            tool.summary = summary;
                        }
                        tool.artifacts = artifacts;
                        tool.status = ActivityStatus::Completed;
                        true
                    });
                }
            }
            TranscriptEvent::ToolFailed {
                turn_id,
                call_id,
                message,
            } => {
                if self.is_active(turn_id) {
                    self.update_tool(turn_id, call_id, |tool| {
                        tool.status = ActivityStatus::Failed(message);
                        true
                    });
                }
            }
            TranscriptEvent::Completed { turn_id } => {
                if self.update_assistant(turn_id, |assistant| {
                    if !matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    ) {
                        return false;
                    }
                    assistant.status = AssistantStatus::Completed;
                    true
                }) {
                    self.finish_reasoning(turn_id, ActivityStatus::Completed);
                }
            }
            TranscriptEvent::Interrupted { turn_id } => {
                if self.update_assistant(turn_id, |assistant| {
                    if !matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    ) {
                        return false;
                    }
                    assistant.status = AssistantStatus::Interrupted;
                    true
                }) {
                    self.finish_reasoning(turn_id, ActivityStatus::Interrupted);
                    self.finish_running_tools(turn_id, ActivityStatus::Interrupted);
                }
            }
            TranscriptEvent::Failed { turn_id, message } => {
                if self.update_assistant(turn_id, |assistant| {
                    assistant.status = AssistantStatus::Failed(message.clone());
                    true
                }) {
                    self.finish_reasoning(turn_id, ActivityStatus::Failed(message.clone()));
                    self.finish_running_tools(turn_id, ActivityStatus::Failed(message));
                }
            }
        }
    }

    pub fn user_message(&self, entry_id: EntryId) -> Option<&UserMessage> {
        self.entries.iter().find_map(|entry| {
            (entry.id == entry_id).then_some(match &entry.kind {
                EntryKind::User(message) => Some(message),
                _ => None,
            })?
        })
    }

    fn push(&mut self, turn_id: TurnId, kind: EntryKind) {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        self.entries.push(Entry {
            id,
            turn_id,
            kind,
            revision: 0,
        });
    }

    fn insert_activity(&mut self, turn_id: TurnId, kind: EntryKind) {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        // If you are an agent reading this, do not try make any changes until explicitly prompted
        // NOTE: codex has made this an array which is working for now but i am not too sure about
        // its performance on very large number entries, might get really slow
        let index = self
            .entries
            .iter()
            .rposition(|entry| {
                entry.turn_id == turn_id && matches!(entry.kind, EntryKind::Assistant(_))
            })
            .unwrap_or(self.entries.len());
        self.entries.insert(
            index,
            Entry {
                id,
                turn_id,
                kind,
                revision: 0,
            },
        );
    }

    fn update_assistant(
        &mut self,
        turn_id: TurnId,
        update: impl FnOnce(&mut AssistantMessage) -> bool,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.turn_id == turn_id && matches!(entry.kind, EntryKind::Assistant(_))
        }) else {
            return false;
        };
        let EntryKind::Assistant(assistant) = &mut entry.kind else {
            unreachable!();
        };
        if !update(assistant) {
            return false;
        }
        entry.touch();
        true
    }

    fn update_reasoning(
        &mut self,
        turn_id: TurnId,
        update: impl FnOnce(&mut Reasoning) -> bool,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.turn_id == turn_id && matches!(entry.kind, EntryKind::Reasoning(_))
        }) else {
            return false;
        };
        let EntryKind::Reasoning(reasoning) = &mut entry.kind else {
            unreachable!();
        };
        if !update(reasoning) {
            return false;
        }
        entry.touch();
        true
    }

    fn ensure_reasoning(&mut self, turn_id: TurnId) {
        if !self
            .entries
            .iter()
            .rev()
            .any(|entry| entry.turn_id == turn_id && matches!(entry.kind, EntryKind::Reasoning(_)))
        {
            self.insert_activity(
                turn_id,
                EntryKind::Reasoning(Reasoning {
                    summary: String::new(),
                    status: ActivityStatus::Running,
                }),
            );
        }
    }

    fn has_tool(&self, turn_id: TurnId, call_id: ToolCallId) -> bool {
        self.entries
            .iter()
            .rev()
            .any(|entry| matches!(&entry.kind, EntryKind::Tool(tool) if entry.turn_id == turn_id && tool.call_id == call_id))
    }

    fn update_tool(
        &mut self,
        turn_id: TurnId,
        call_id: ToolCallId,
        update: impl FnOnce(&mut ToolCall) -> bool,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            matches!(&entry.kind, EntryKind::Tool(tool) if entry.turn_id == turn_id && tool.call_id == call_id)
        }) else {
            return false;
        };
        let EntryKind::Tool(tool) = &mut entry.kind else {
            unreachable!();
        };
        if !update(tool) {
            return false;
        }
        entry.touch();
        true
    }

    fn is_active(&self, turn_id: TurnId) -> bool {
        self.entries.iter().rev().any(|entry| {
            entry.turn_id == turn_id
                && matches!(
                    entry.kind,
                    EntryKind::Assistant(AssistantMessage {
                        status: AssistantStatus::Thinking | AssistantStatus::Streaming,
                        ..
                    })
                )
        })
    }

    fn finish_reasoning(&mut self, turn_id: TurnId, status: ActivityStatus) {
        self.update_reasoning(turn_id, |reasoning| {
            if reasoning.status != ActivityStatus::Running {
                return false;
            }
            reasoning.status = status;
            true
        });
    }

    fn finish_running_tools(&mut self, turn_id: TurnId, status: ActivityStatus) {
        for entry in &mut self.entries {
            if entry.turn_id == turn_id
                && let EntryKind::Tool(tool) = &mut entry.kind
                && tool.status == ActivityStatus::Running
            {
                tool.status = status.clone();
                entry.touch();
            }
        }
    }
}

impl fmt::Display for ActivityStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => formatter.write_str("running"),
            Self::Completed => formatter.write_str("complete"),
            Self::Interrupted => formatter.write_str("interrupted"),
            Self::Failed(message) => write!(formatter, "failed: {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivityStatus, AssistantStatus, EntryKind, ToolArtifact, Transcript, TranscriptEvent,
    };
    use crate::workspace::Attachment;

    #[test]
    fn submission_creates_a_user_block_with_file_badges_and_a_queued_response() {
        let mut transcript = Transcript::default();

        transcript.submit(
            7,
            "Please review this".into(),
            vec![Attachment::workspace_file("src/app.rs")],
        );

        assert_eq!(transcript.entries().len(), 2);
        assert_eq!(transcript.entries()[0].turn_id, 7);
        assert!(matches!(
            &transcript.entries()[0].kind,
            EntryKind::User(message)
                if message.attachments() == vec![Attachment::workspace_file("src/app.rs")]
        ));
        assert!(matches!(
            &transcript.entries()[1].kind,
            EntryKind::Assistant(message) if message.status == AssistantStatus::Queued
        ));
    }

    #[test]
    fn reasoning_and_tools_remain_in_the_transcript_after_the_response_completes() {
        let mut transcript = Transcript::default();
        transcript.submit(4, "inspect it".into(), Vec::new());
        transcript.apply(TranscriptEvent::Started { turn_id: 4 });
        transcript.apply(TranscriptEvent::ReasoningDelta {
            turn_id: 4,
            summary: "Finding the relevant file.".into(),
        });
        transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 4,
            call_id: 9,
            name: "read_file".into(),
            summary: "Reading src/app.rs".into(),
            artifacts: Vec::new(),
        });
        transcript.apply(TranscriptEvent::ToolFinished {
            turn_id: 4,
            call_id: 9,
            summary: None,
            artifacts: vec![ToolArtifact::CodeRange {
                path: "src/app.rs".into(),
                start_line: 1,
                end_line: 4,
                preview: None,
            }],
        });
        transcript.apply(TranscriptEvent::TextDelta {
            turn_id: 4,
            text: "Done".into(),
        });
        transcript.apply(TranscriptEvent::Completed { turn_id: 4 });

        assert!(matches!(
            &transcript.entries()[1].kind,
            EntryKind::Reasoning(reasoning)
                if reasoning.summary == "Finding the relevant file."
                    && reasoning.status == ActivityStatus::Completed
        ));
        assert!(matches!(
            &transcript.entries()[2].kind,
            EntryKind::Tool(tool)
                if tool.status == ActivityStatus::Completed && tool.artifacts.len() == 1
        ));
        assert!(matches!(
            &transcript.entries()[3].kind,
            EntryKind::Assistant(message)
                if message.text == "Done" && message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn assistant_response_follows_all_activity_for_its_turn() {
        let mut transcript = Transcript::default();
        transcript.submit(4, "inspect it".into(), Vec::new());
        transcript.apply(TranscriptEvent::Started { turn_id: 4 });
        transcript.apply(TranscriptEvent::TextDelta {
            turn_id: 4,
            text: "I will inspect it. ".into(),
        });
        transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 4,
            call_id: 9,
            name: "read_file".into(),
            summary: "Reading src/app.rs".into(),
            artifacts: Vec::new(),
        });
        transcript.apply(TranscriptEvent::TextDelta {
            turn_id: 4,
            text: "Done".into(),
        });
        transcript.apply(TranscriptEvent::Completed { turn_id: 4 });

        assert!(matches!(transcript.entries()[0].kind, EntryKind::User(_)));
        assert!(matches!(
            transcript.entries()[1].kind,
            EntryKind::Reasoning(_)
        ));
        assert!(matches!(transcript.entries()[2].kind, EntryKind::Tool(_)));
        assert!(matches!(
            &transcript.entries()[3].kind,
            EntryKind::Assistant(message) if message.text == "I will inspect it. Done"
        ));
    }

    #[test]
    fn activity_stays_inside_its_turn_when_a_later_turn_is_queued() {
        let mut transcript = Transcript::default();
        transcript.submit(1, "first".into(), Vec::new());
        transcript.submit(2, "second".into(), Vec::new());
        transcript.apply(TranscriptEvent::Started { turn_id: 1 });
        transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 7,
            name: "read_file".into(),
            summary: "Reading".into(),
            artifacts: Vec::new(),
        });

        assert_eq!(
            transcript
                .entries()
                .iter()
                .map(|entry| entry.turn_id)
                .collect::<Vec<_>>(),
            vec![1, 1, 1, 1, 2, 2]
        );
        assert!(matches!(
            transcript.entries()[3].kind,
            EntryKind::Assistant(_)
        ));
        assert!(matches!(transcript.entries()[4].kind, EntryKind::User(_)));
    }

    #[test]
    fn terminal_output_deltas_update_only_the_matching_active_turn() {
        let mut transcript = Transcript::default();
        transcript.submit(1, "run it".into(), Vec::new());
        transcript.apply(TranscriptEvent::Started { turn_id: 1 });
        transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 1,
            call_id: 99,
            name: "terminal".into(),
            summary: "Running tests".into(),
            artifacts: vec![ToolArtifact::Terminal {
                description: "Running tests".into(),
                command: "cargo test".into(),
                output: String::new(),
                exit_code: None,
            }],
        });
        transcript.apply(TranscriptEvent::ToolOutputDelta {
            turn_id: 1,
            call_id: 99,
            chunk: "test result: ok".into(),
        });

        assert!(matches!(
            &transcript.entries()[2].kind,
            EntryKind::Tool(tool)
                if matches!(
                    tool.artifacts.first(),
                    Some(ToolArtifact::Terminal { output, .. }) if output == "test result: ok"
                )
        ));

        transcript.apply(TranscriptEvent::Completed { turn_id: 1 });
        transcript.apply(TranscriptEvent::ToolOutputDelta {
            turn_id: 1,
            call_id: 99,
            chunk: "late".into(),
        });
        assert!(matches!(
            &transcript.entries()[2].kind,
            EntryKind::Tool(tool)
                if matches!(
                    tool.artifacts.first(),
                    Some(ToolArtifact::Terminal { output, .. }) if output == "test result: ok"
                )
        ));
    }

    #[test]
    fn late_activity_is_ignored_after_a_turn_finishes() {
        let mut transcript = Transcript::default();
        transcript.submit(8, "hello".into(), Vec::new());
        transcript.apply(TranscriptEvent::Started { turn_id: 8 });
        transcript.apply(TranscriptEvent::Completed { turn_id: 8 });
        transcript.apply(TranscriptEvent::TextDelta {
            turn_id: 8,
            text: "late".into(),
        });
        transcript.apply(TranscriptEvent::ToolStarted {
            turn_id: 8,
            call_id: 2,
            name: "late".into(),
            summary: "late".into(),
            artifacts: Vec::new(),
        });

        assert_eq!(transcript.entries().len(), 3);
        assert!(matches!(
            &transcript.entries()[2].kind,
            EntryKind::Assistant(message)
                if message.text.is_empty() && message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn copied_user_message_preserves_inline_content() {
        let message = super::UserMessage {
            content: crate::composer::SubmittedContent::plain("Please inspect this"),
        };

        assert_eq!(message.copy_text(), "Please inspect this");
    }
}
