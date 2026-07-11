use crate::{
    agent::{AgentEvent, AgentTaskRunner},
    app::{App, AppAction, AuthProvider},
    auth::{AuthEvent, AuthStore, AuthTaskRunner},
    clipboard::{ClipboardEvent, ClipboardTaskRunner},
    llm::{LlmClient, LlmConfig},
    theme::Theme,
    ui, workspace,
};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{Stdout, stdout},
    panic,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

const TICK_RATE: Duration = Duration::from_millis(50);

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    Interactive,
    AuthOnly,
}

impl LaunchMode {
    fn parse<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = args.into_iter();
        match (args.next(), args.next()) {
            (None, None) => Ok(Self::Interactive),
            (Some(command), None) if command.as_ref() == "auth" => Ok(Self::AuthOnly),
            (Some(command), None) => anyhow::bail!(
                "unknown command '{}'; supported command: auth",
                command.as_ref()
            ),
            _ => anyhow::bail!("too many arguments; usage: funcode [auth]"),
        }
    }
}

pub fn run() -> Result<()> {
    let launch_mode = LaunchMode::parse(std::env::args().skip(1))?;
    let session = TerminalSession::enter()?;
    install_restoring_panic_hook(session.keyboard_enhancement);

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("failed to create the terminal backend")?;
    let result = run_event_loop(&mut terminal, launch_mode);

    drop(terminal);
    drop(session);
    result
}

fn run_event_loop(terminal: &mut AppTerminal, launch_mode: LaunchMode) -> Result<()> {
    let mut app = match launch_mode {
        LaunchMode::Interactive => App::new(),
        LaunchMode::AuthOnly => App::for_auth(),
    };
    let mut workspace_discovery = match launch_mode {
        LaunchMode::Interactive => std::env::current_dir().ok().map(start_workspace_discovery),
        LaunchMode::AuthOnly => None,
    };
    let theme = Theme::default();
    let mut runner = match launch_mode {
        LaunchMode::Interactive => {
            let config = LlmConfig::from_env().context("failed to load LLM configuration")?;
            let auth_store =
                AuthStore::standard().context("failed to locate ChatGPT credentials")?;
            let llm = LlmClient::new(config, auth_store).context("failed to configure the LLM")?;
            Some(AgentTaskRunner::spawn(llm))
        }
        LaunchMode::AuthOnly => None,
    };
    let mut auth_runner = AuthTaskRunner::spawn();
    let mut clipboard = ClipboardTaskRunner::spawn();
    let mut next_tick = Instant::now() + TICK_RATE;
    let mut should_quit = false;
    let mut regions = ui::UiRegions::default();

    while !should_quit {
        if let Some(discovery) = workspace_discovery.as_ref() {
            match discovery.try_recv() {
                Ok(files) => {
                    app.set_workspace_files(files);
                    workspace_discovery = None;
                }
                Err(TryRecvError::Disconnected) => workspace_discovery = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(runner) = runner.as_ref() {
            while let Some(agent_event) = runner.try_event() {
                app.handle_agent_event(agent_event);
            }
        }
        while let Some(auth_event) = auth_runner.try_event() {
            app.handle_auth_event(auth_event);
        }
        while let Some(clipboard_event) = clipboard.try_event() {
            handle_clipboard_event(&mut app, clipboard_event);
        }

        terminal
            .draw(|frame| regions = ui::render(frame, &app, &theme))
            .context("failed to draw the terminal UI")?;

        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout).context("failed to poll terminal input")? {
            match event::read().context("failed to read terminal input")? {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press) => {
                    if let Some(action) = app.handle_key(key, Instant::now()) {
                        should_quit =
                            dispatch(action, &mut app, runner.as_ref(), &auth_runner, &clipboard);
                    }
                }
                Event::Paste(text) => app.handle_paste(&text),
                Event::Mouse(mouse) => {
                    if let Some(action) = handle_mouse_event(&mut app, &regions, mouse) {
                        should_quit =
                            dispatch(action, &mut app, runner.as_ref(), &auth_runner, &clipboard);
                    }
                }
                _ => {}
            }
        }

        if Instant::now() >= next_tick {
            app.tick();
            next_tick = Instant::now() + TICK_RATE;
        }
    }

    if let Some(runner) = runner.as_mut() {
        runner.shutdown();
    }
    auth_runner.shutdown();
    clipboard.shutdown();
    Ok(())
}

fn start_workspace_discovery(root: PathBuf) -> Receiver<Vec<String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(workspace::discover_files(&root));
    });
    receiver
}

fn handle_mouse_event(
    app: &mut App,
    regions: &ui::UiRegions,
    mouse: crossterm::event::MouseEvent,
) -> Option<AppAction> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => match regions.target_at(mouse.column, mouse.row)
        {
            Some(ui::UiTarget::TranscriptEntry(entry_id)) => {
                app.activate_transcript_entry(entry_id);
                None
            }
            Some(ui::UiTarget::MessageCopy) => app.copy_message_dialog(),
            Some(ui::UiTarget::AuthProvider(index)) => {
                app.set_auth_selection(index);
                app.select_auth_provider()
            }
            Some(ui::UiTarget::Suggestion(index)) => app.activate_suggestion(index),
            None => None,
        },
        MouseEventKind::Moved => {
            match regions.target_at(mouse.column, mouse.row) {
                Some(ui::UiTarget::Suggestion(index)) => app.set_suggestion_selection(index),
                Some(ui::UiTarget::AuthProvider(index)) => app.set_auth_selection(index),
                _ => {}
            }
            None
        }
        MouseEventKind::ScrollUp => {
            if app.auth_dialog.is_some() {
                app.move_auth_selection(-1);
            } else if !app.suggestions().is_empty() {
                app.move_suggestion_selection(-1);
            } else if app.message_dialog.is_none() {
                app.scroll_transcript_up();
            }
            None
        }
        MouseEventKind::ScrollDown => {
            if app.auth_dialog.is_some() {
                app.move_auth_selection(1);
            } else if !app.suggestions().is_empty() {
                app.move_suggestion_selection(1);
            } else if app.message_dialog.is_none() {
                app.scroll_transcript_down();
            }
            None
        }
        _ => None,
    }
}

fn dispatch(
    action: AppAction,
    app: &mut App,
    runner: Option<&AgentTaskRunner>,
    auth_runner: &AuthTaskRunner,
    clipboard: &ClipboardTaskRunner,
) -> bool {
    match action {
        AppAction::Submit {
            request_id,
            prompt,
            attachments,
            mode,
        } => {
            match runner {
                Some(runner) => {
                    if let Err(error) = runner.submit_with_attachments_and_mode(
                        request_id,
                        prompt,
                        attachments,
                        mode,
                    ) {
                        app.handle_agent_event(AgentEvent::Failed {
                            request_id,
                            message: error.to_string(),
                        });
                    }
                }
                None => app.handle_agent_event(AgentEvent::Failed {
                    request_id,
                    message: "the LLM is unavailable in authentication-only mode".into(),
                }),
            }
            false
        }
        AppAction::Cancel { request_id } => {
            match runner {
                Some(runner) => {
                    if let Err(error) = runner.cancel(request_id) {
                        app.handle_agent_event(AgentEvent::Failed {
                            request_id,
                            message: error.to_string(),
                        });
                    }
                }
                None => app.handle_agent_event(AgentEvent::Failed {
                    request_id,
                    message: "the LLM is unavailable in authentication-only mode".into(),
                }),
            }
            false
        }
        AppAction::Authenticate {
            provider: AuthProvider::ChatGptSubscription,
        } => {
            if let Err(error) = auth_runner.authenticate() {
                app.handle_auth_event(AuthEvent::Failed {
                    message: error.to_string(),
                });
            }
            false
        }
        AppAction::CancelAuthentication { quit } => {
            if let Err(error) = auth_runner.cancel() {
                app.handle_auth_event(AuthEvent::Failed {
                    message: error.to_string(),
                });
            }
            quit
        }
        AppAction::CopyToClipboard { text } => {
            if let Err(error) = clipboard.copy(text) {
                app.set_notice(error.to_string());
            }
            false
        }
        AppAction::Quit => true,
    }
}

fn handle_clipboard_event(app: &mut App, event: ClipboardEvent) {
    match event {
        ClipboardEvent::Copied => app.set_notice("Message copied"),
        ClipboardEvent::Failed(error) => app.set_notice(error),
    }
}

struct TerminalSession {
    keyboard_enhancement: bool,
    active: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut session = Self {
            keyboard_enhancement: false,
            active: true,
        };

        let setup_result = (|| -> Result<()> {
            execute!(
                stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture,
                Hide
            )
            .context("failed to enter the alternate terminal screen")?;

            if supports_keyboard_enhancement().unwrap_or(false) {
                execute!(
                    stdout(),
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                )
                .context("failed to enable enhanced keyboard reporting")?;
                session.keyboard_enhancement = true;
            }
            Ok(())
        })();

        if let Err(error) = setup_result {
            session.restore();
            return Err(error);
        }
        Ok(session)
    }

    fn restore(&mut self) {
        if self.active {
            restore_terminal(self.keyboard_enhancement);
            self.active = false;
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
    }
}

fn restore_terminal(keyboard_enhancement: bool) {
    let mut output = stdout();
    if keyboard_enhancement {
        let _ = execute!(output, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        output,
        Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

fn install_restoring_panic_hook(keyboard_enhancement: bool) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        restore_terminal(keyboard_enhancement);
        previous(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchMode, dispatch, handle_clipboard_event, handle_mouse_event, start_workspace_discovery,
    };
    use crate::{
        app::{App, AppAction, Screen},
        auth::AuthTaskRunner,
        clipboard::{Clipboard, ClipboardTaskRunner},
        ui::UiRegions,
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use std::{fs, time::Duration};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[derive(Default)]
    struct RecordingClipboard;

    impl Clipboard for RecordingClipboard {
        fn copy(&mut self, text: &str) -> Result<(), String> {
            assert_eq!(text, "copied message");
            Ok(())
        }
    }

    #[test]
    fn auth_is_the_supported_cli_subcommand() {
        assert_eq!(
            LaunchMode::parse(Vec::<String>::new()).unwrap(),
            LaunchMode::Interactive
        );
        assert_eq!(LaunchMode::parse(["auth"]).unwrap(), LaunchMode::AuthOnly);
        assert!(
            LaunchMode::parse(["unknown"])
                .unwrap_err()
                .to_string()
                .contains("auth")
        );
        assert!(LaunchMode::parse(["auth", "extra"]).is_err());
    }

    #[test]
    fn workspace_discovery_returns_files_from_a_background_worker() {
        let root = std::env::temp_dir().join(format!("funcode-workspace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();

        let receiver = start_workspace_discovery(root.clone());

        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ["src/main.rs"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mouse_click_activates_the_suggestion_under_the_pointer() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/exit");
        let regions = UiRegions {
            suggestions: vec![Rect::new(4, 5, 12, 1)],
            ..UiRegions::default()
        };

        assert_eq!(
            handle_mouse_event(
                &mut app,
                &regions,
                mouse(MouseEventKind::Down(MouseButton::Left), 4, 5)
            ),
            Some(AppAction::Quit)
        );
    }

    #[test]
    fn mouse_hover_and_wheel_choose_suggestions() {
        let mut app = App::with_files(["src/app.rs", "src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src/");
        let regions = UiRegions {
            suggestions: vec![Rect::new(4, 5, 12, 1), Rect::new(4, 6, 12, 1)],
            ..UiRegions::default()
        };

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::Moved, 4, 6));
        assert_eq!(app.selected_suggestion(), 1);

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(app.selected_suggestion(), 0);

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(app.selected_suggestion(), 1);
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_when_no_overlay_owns_it() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        let regions = UiRegions::default();

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(app.scroll_from_bottom, 5);
        assert!(!app.follow_output);

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.follow_output);
    }

    #[test]
    fn copy_action_uses_the_clipboard_and_shows_confirmation() {
        let mut app = App::new();
        let mut auth_runner = AuthTaskRunner::spawn();
        let mut clipboard = ClipboardTaskRunner::spawn_with(RecordingClipboard);

        assert!(!dispatch(
            AppAction::CopyToClipboard {
                text: "copied message".into(),
            },
            &mut app,
            None,
            &auth_runner,
            &clipboard,
        ));
        handle_clipboard_event(
            &mut app,
            clipboard.recv_timeout(Duration::from_secs(1)).unwrap(),
        );
        assert_eq!(app.notice.as_deref(), Some("Message copied"));

        auth_runner.shutdown();
        clipboard.shutdown();
    }
}
