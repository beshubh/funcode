use crate::{
    agent::{AgentEvent, AgentTaskRunner},
    app::{App, AppAction},
    theme::Theme,
    ui,
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
    time::{Duration, Instant},
};

const TICK_RATE: Duration = Duration::from_millis(50);

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub fn run() -> Result<()> {
    let session = TerminalSession::enter()?;
    install_restoring_panic_hook(session.keyboard_enhancement);

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("failed to create the terminal backend")?;
    let result = run_event_loop(&mut terminal);

    drop(terminal);
    drop(session);
    result
}

fn run_event_loop(terminal: &mut AppTerminal) -> Result<()> {
    let mut app = App::new();
    let theme = Theme::default();
    let mut runner = AgentTaskRunner::spawn();
    let mut next_tick = Instant::now() + TICK_RATE;
    let mut should_quit = false;
    let mut regions = ui::UiRegions::default();

    while !should_quit {
        while let Some(agent_event) = runner.try_event() {
            app.handle_agent_event(agent_event);
        }

        terminal
            .draw(|frame| regions = ui::render(frame, &app, &theme))
            .context("failed to draw the terminal UI")?;

        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout).context("failed to poll terminal input")? {
            match event::read().context("failed to read terminal input")? {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press) => {
                    if let Some(action) = app.handle_key(key, Instant::now()) {
                        should_quit = dispatch(action, &mut app, &runner);
                    }
                }
                Event::Paste(text) => app.handle_paste(&text),
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    match regions.target_at(mouse.column, mouse.row) {
                        Some(ui::UiTarget::Thinking) => app.toggle_thinking(),
                        Some(ui::UiTarget::Tools) => app.toggle_tools(),
                        None => {}
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

    runner.shutdown();
    Ok(())
}

fn dispatch(action: AppAction, app: &mut App, runner: &AgentTaskRunner) -> bool {
    match action {
        AppAction::Submit { request_id, prompt } => {
            if let Err(error) = runner.submit(request_id, prompt) {
                app.handle_agent_event(AgentEvent::Failed {
                    request_id,
                    message: error.to_string(),
                });
            }
            false
        }
        AppAction::Cancel { request_id } => {
            if let Err(error) = runner.cancel(request_id) {
                app.handle_agent_event(AgentEvent::Failed {
                    request_id,
                    message: error.to_string(),
                });
            }
            false
        }
        AppAction::Quit => true,
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
