pub use crate::composer::Attachment;
use crate::composer::ComposerContent;
use std::fmt;

pub type EntryId = u64;
pub type TurnId = u64;
pub type ToolCallId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub content: ComposerContent,
}

impl UserMessage {
    pub fn copy_text(&self) -> String {
        self.content.text().to_owned()
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
        path: String,
        start_line: u32,
        end_line: u32,
        preview: Option<String>,
    },
    Patch {
        path: String,
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
    FileReference(String),
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
pub enum EntryKind {
    User(UserMessage),
    Assistant(AssistantMessage),
    Reasoning(Reasoning),
    Tool(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub turn_id: TurnId,
    pub kind: EntryKind,
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
        self.entries.iter().any(|entry| {
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
        let content = ComposerContent::with_attachments(text, &attachments);
        self.push_submission(turn_id, content, attachments);
    }

    pub fn submit_content(&mut self, turn_id: TurnId, content: ComposerContent) {
        let attachments = content.attachments();
        self.push_submission(turn_id, content, attachments);
    }

    fn push_submission(
        &mut self,
        turn_id: TurnId,
        content: ComposerContent,
        attachments: Vec<Attachment>,
    ) {
        let text = content.prompt_text();
        self.push(
            turn_id,
            EntryKind::User(UserMessage {
                text,
                attachments,
                content,
            }),
        );
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
                if let Some(assistant) = self.assistant_mut(turn_id)
                    && assistant.status == AssistantStatus::Queued
                {
                    assistant.status = AssistantStatus::Thinking;
                    self.ensure_reasoning(turn_id);
                }
            }
            TranscriptEvent::TextDelta { turn_id, text } => {
                if let Some(assistant) = self.assistant_mut(turn_id)
                    && matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    )
                {
                    assistant.status = AssistantStatus::Streaming;
                    assistant.text.push_str(&text);
                    self.finish_reasoning(turn_id, ActivityStatus::Completed);
                }
            }
            TranscriptEvent::ReasoningDelta { turn_id, summary } => {
                if self.is_active(turn_id) {
                    let reasoning = self.ensure_reasoning(turn_id);
                    reasoning.summary.push_str(&summary);
                }
            }
            TranscriptEvent::ToolStarted {
                turn_id,
                call_id,
                name,
                summary,
                artifacts,
            } => {
                if self.is_active(turn_id) && self.tool_mut(turn_id, call_id).is_none() {
                    self.push(
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
                if self.is_active(turn_id)
                    && let Some(tool) = self.tool_mut(turn_id, call_id)
                    && let Some(ToolArtifact::Terminal { output, .. }) = tool.artifacts.first_mut()
                {
                    output.push_str(&chunk);
                }
            }
            TranscriptEvent::ToolFinished {
                turn_id,
                call_id,
                summary,
                artifacts,
            } => {
                if self.is_active(turn_id)
                    && let Some(tool) = self.tool_mut(turn_id, call_id)
                {
                    if let Some(summary) = summary {
                        tool.summary = summary;
                    }
                    tool.artifacts = artifacts;
                    tool.status = ActivityStatus::Completed;
                }
            }
            TranscriptEvent::ToolFailed {
                turn_id,
                call_id,
                message,
            } => {
                if self.is_active(turn_id)
                    && let Some(tool) = self.tool_mut(turn_id, call_id)
                {
                    tool.status = ActivityStatus::Failed(message);
                }
            }
            TranscriptEvent::Completed { turn_id } => {
                if let Some(assistant) = self.assistant_mut(turn_id)
                    && matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    )
                {
                    assistant.status = AssistantStatus::Completed;
                    self.finish_reasoning(turn_id, ActivityStatus::Completed);
                }
            }
            TranscriptEvent::Interrupted { turn_id } => {
                if let Some(assistant) = self.assistant_mut(turn_id)
                    && matches!(
                        assistant.status,
                        AssistantStatus::Thinking | AssistantStatus::Streaming
                    )
                {
                    assistant.status = AssistantStatus::Interrupted;
                    self.finish_reasoning(turn_id, ActivityStatus::Interrupted);
                    self.finish_running_tools(turn_id, ActivityStatus::Interrupted);
                }
            }
            TranscriptEvent::Failed { turn_id, message } => {
                if let Some(assistant) = self.assistant_mut(turn_id) {
                    assistant.status = AssistantStatus::Failed(message.clone());
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
        self.entries.push(Entry { id, turn_id, kind });
    }

    fn assistant_mut(&mut self, turn_id: TurnId) -> Option<&mut AssistantMessage> {
        self.entries.iter_mut().find_map(|entry| {
            (entry.turn_id == turn_id).then_some(match &mut entry.kind {
                EntryKind::Assistant(message) => Some(message),
                _ => None,
            })?
        })
    }

    fn reasoning_mut(&mut self, turn_id: TurnId) -> Option<&mut Reasoning> {
        self.entries.iter_mut().find_map(|entry| {
            (entry.turn_id == turn_id).then_some(match &mut entry.kind {
                EntryKind::Reasoning(reasoning) => Some(reasoning),
                _ => None,
            })?
        })
    }

    fn ensure_reasoning(&mut self, turn_id: TurnId) -> &mut Reasoning {
        if self.reasoning_mut(turn_id).is_none() {
            self.push(
                turn_id,
                EntryKind::Reasoning(Reasoning {
                    summary: String::new(),
                    status: ActivityStatus::Running,
                }),
            );
        }
        self.reasoning_mut(turn_id)
            .expect("reasoning entry was just inserted")
    }

    fn tool_mut(&mut self, turn_id: TurnId, call_id: ToolCallId) -> Option<&mut ToolCall> {
        self.entries
            .iter_mut()
            .find_map(|entry| match &mut entry.kind {
                EntryKind::Tool(tool) if entry.turn_id == turn_id && tool.call_id == call_id => {
                    Some(tool)
                }
                _ => None,
            })
    }

    fn is_active(&self, turn_id: TurnId) -> bool {
        self.entries.iter().any(|entry| {
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
        if let Some(reasoning) = self.reasoning_mut(turn_id)
            && reasoning.status == ActivityStatus::Running
        {
            reasoning.status = status;
        }
    }

    fn finish_running_tools(&mut self, turn_id: TurnId, status: ActivityStatus) {
        for entry in &mut self.entries {
            if entry.turn_id == turn_id
                && let EntryKind::Tool(tool) = &mut entry.kind
                && tool.status == ActivityStatus::Running
            {
                tool.status = status.clone();
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
        ActivityStatus, AssistantStatus, Attachment, EntryKind, ToolArtifact, Transcript,
        TranscriptEvent,
    };

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
                if message.attachments == vec![Attachment::workspace_file("src/app.rs")]
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
            &transcript.entries()[2].kind,
            EntryKind::Reasoning(reasoning)
                if reasoning.summary == "Finding the relevant file."
                    && reasoning.status == ActivityStatus::Completed
        ));
        assert!(matches!(
            &transcript.entries()[3].kind,
            EntryKind::Tool(tool)
                if tool.status == ActivityStatus::Completed && tool.artifacts.len() == 1
        ));
        assert!(matches!(
            &transcript.entries()[1].kind,
            EntryKind::Assistant(message)
                if message.text == "Done" && message.status == AssistantStatus::Completed
        ));
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
            &transcript.entries()[3].kind,
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
            &transcript.entries()[3].kind,
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
            &transcript.entries()[1].kind,
            EntryKind::Assistant(message)
                if message.text.is_empty() && message.status == AssistantStatus::Completed
        ));
    }

    #[test]
    fn copied_user_message_preserves_inline_content() {
        let message = super::UserMessage {
            text: "Please inspect this".into(),
            attachments: vec![Attachment::workspace_file("src/lib.rs")],
            content: crate::composer::ComposerContent::plain("Please inspect this"),
        };

        assert_eq!(message.copy_text(), "Please inspect this");
    }
}
