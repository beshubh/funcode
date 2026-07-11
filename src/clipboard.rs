use std::{
    fmt,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

pub trait Clipboard: Send + 'static {
    fn copy(&mut self, text: &str) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn copy(&mut self, text: &str) -> Result<(), String> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|error| format!("could not access the system clipboard: {error}"))?;
        clipboard
            .set_text(text)
            .map_err(|error| format!("could not copy the message: {error}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEvent {
    Copied(String),
    Failed(String),
}

enum ClipboardCommand {
    Copy { text: String, confirmation: String },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardUnavailable;

impl fmt::Display for ClipboardUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the background clipboard runner is unavailable")
    }
}

impl std::error::Error for ClipboardUnavailable {}

pub struct ClipboardTaskRunner {
    commands: Sender<ClipboardCommand>,
    events: Receiver<ClipboardEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardTaskRunner {
    pub fn spawn() -> Self {
        Self::spawn_with_impl(SystemClipboard)
    }

    pub fn copy(
        &self,
        text: String,
        confirmation: impl Into<String>,
    ) -> Result<(), ClipboardUnavailable> {
        self.commands
            .send(ClipboardCommand::Copy {
                text,
                confirmation: confirmation.into(),
            })
            .map_err(|_| ClipboardUnavailable)
    }

    pub fn try_event(&self) -> Option<ClipboardEvent> {
        self.events.try_recv().ok()
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(ClipboardCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn spawn_with<C>(clipboard: C) -> Self
    where
        C: Clipboard,
    {
        Self::spawn_with_impl(clipboard)
    }

    fn spawn_with_impl<C>(clipboard: C) -> Self
    where
        C: Clipboard,
    {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || run(clipboard, command_rx, event_tx));
        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    #[cfg(test)]
    pub(crate) fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<ClipboardEvent, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

impl Drop for ClipboardTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run<C>(mut clipboard: C, commands: Receiver<ClipboardCommand>, events: Sender<ClipboardEvent>)
where
    C: Clipboard,
{
    while let Ok(command) = commands.recv() {
        match command {
            ClipboardCommand::Copy { text, confirmation } => {
                let event = match clipboard.copy(&text) {
                    Ok(()) => ClipboardEvent::Copied(confirmation),
                    Err(error) => ClipboardEvent::Failed(error),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
            ClipboardCommand::Shutdown => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Clipboard, ClipboardEvent, ClipboardTaskRunner};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingClipboard {
        copied: Vec<String>,
    }

    impl Clipboard for RecordingClipboard {
        fn copy(&mut self, text: &str) -> Result<(), String> {
            self.copied.push(text.to_owned());
            Ok(())
        }
    }

    #[test]
    fn copying_is_performed_by_a_background_runner() {
        let mut runner = ClipboardTaskRunner::spawn_with(RecordingClipboard::default());

        runner.copy("message".into(), "Selection copied").unwrap();

        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClipboardEvent::Copied("Selection copied".into())
        );
        runner.shutdown();
    }
}
