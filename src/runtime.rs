use crate::{
    agent::{AgentEvent, AgentTaskRunner},
    app::{App, AppAction, AuthProvider, FILE_SUGGESTION_LIMIT, PointerEvent},
    auth::{AuthEvent, AuthStore, AuthTaskRunner},
    clipboard::{ClipboardEvent, ClipboardTaskRunner},
    composer::SubmittedContent,
    llm::{LlmClient, LlmConfig},
    model_catalog::ModelCatalogTaskRunner,
    session::SessionMode,
    submission::{DraftId, SubmissionEvent, SubmissionTaskRunner},
    terminal_selection::TerminalSelection,
    theme::{Theme, ThemeConfigEvent, ThemeConfigLoad, ThemeConfigStore, ThemeConfigTaskRunner},
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
use ratatui::{Terminal, backend::CrosstermBackend, buffer::Buffer, layout::Position};
use std::{
    ffi::OsStr,
    io::{Stdout, stdout},
    panic,
    path::PathBuf,
    time::{Duration, Instant},
};

const TICK_RATE: Duration = Duration::from_millis(50);
const WORKSPACE_SUGGESTION_REFRESH: Duration = Duration::from_millis(500);

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug)]
struct RedrawScheduler {
    dirty: bool,
}

impl Default for RedrawScheduler {
    fn default() -> Self {
        Self { dirty: true }
    }
}

impl RedrawScheduler {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchMode {
    Interactive(PathBuf),
    AuthOnly,
}

impl LaunchMode {
    fn parse<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut args = args.into_iter();
        match (args.next(), args.next()) {
            (None, None) => Self::interactive(
                std::env::current_dir()
                    .context("could not determine the current directory for the workspace")?,
            ),
            (Some(command), None) if command.as_ref() == OsStr::new("auth") => Ok(Self::AuthOnly),
            (Some(path), None) => Self::interactive(PathBuf::from(path.as_ref())),
            _ => anyhow::bail!("too many arguments; usage: funcode [PATH] | funcode auth"),
        }
    }

    fn interactive(path: PathBuf) -> Result<Self> {
        let display = path.display().to_string();
        let workspace = std::fs::canonicalize(&path)
            .with_context(|| format!("could not open workspace '{display}'"))?;
        anyhow::ensure!(
            workspace.is_dir(),
            "workspace '{}' is not a directory",
            workspace.display()
        );
        Ok(Self::Interactive(workspace))
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
    let workspace_root = match &launch_mode {
        LaunchMode::Interactive(workspace) => Some(workspace.clone()),
        LaunchMode::AuthOnly => None,
    };
    let workspace_runner = workspace_root
        .as_ref()
        .cloned()
        .map(workspace::WorkspaceTaskRunner::spawn);
    let mut app = match &launch_mode {
        LaunchMode::Interactive(_) => App::new(),
        LaunchMode::AuthOnly => App::for_auth(),
    };
    let (theme_load, mut theme_runner) = match ThemeConfigStore::standard() {
        Ok(store) => (
            store.load_or_default(),
            Some(ThemeConfigTaskRunner::spawn(store)),
        ),
        Err(_) => (
            ThemeConfigLoad {
                config: Default::default(),
                warning: Some("Could not locate theme configuration; using terminal".into()),
            },
            None,
        ),
    };
    app.set_active_theme(theme_load.config.theme);
    if let Some(warning) = theme_load.warning {
        app.set_notice(warning);
    }
    let (mut runner, mut model_runner, mut submission_runner) = match &launch_mode {
        LaunchMode::Interactive(_) => {
            let config = LlmConfig::from_env().context("failed to load LLM configuration")?;
            let auth_store =
                AuthStore::standard().context("failed to locate ChatGPT credentials")?;
            let llm = LlmClient::new(config, auth_store).context("failed to configure the LLM")?;
            app.set_current_model(
                llm.current_model()
                    .context("failed to read the selected model")?,
            );
            (
                Some(AgentTaskRunner::spawn_in(
                    llm.clone(),
                    workspace_root
                        .clone()
                        .expect("interactive mode has a workspace"),
                )),
                Some(ModelCatalogTaskRunner::spawn(llm.clone())),
                Some(SubmissionTaskRunner::spawn_with_llm(
                    workspace_root
                        .clone()
                        .expect("interactive mode has a workspace"),
                    llm,
                )),
            )
        }
        LaunchMode::AuthOnly => (None, None, None),
    };
    let mut auth_runner = AuthTaskRunner::spawn();
    let mut clipboard = ClipboardTaskRunner::spawn();
    let mut next_tick = Instant::now() + TICK_RATE;
    let mut should_quit = false;
    let mut regions = ui::UiRegions::default();
    let mut selection = TerminalSelection::default();
    let mut rendered_buffer = Buffer::empty(ratatui::layout::Rect::default());
    let mut last_workspace_search: Option<(crate::composer::QueryId, Instant)> = None;
    let mut workspace_search_ready = false;
    let mut redraw = RedrawScheduler::default();
    let mut preflight_scheduler = PreflightScheduler::default();

    while !should_quit {
        if let Some(workspace_runner) = workspace_runner.as_ref() {
            while let Some(event) = workspace_runner.try_event() {
                redraw.mark_dirty();
                match event {
                    workspace::WorkspaceEvent::Ready { warning } => {
                        app.use_indexed_workspace_search();
                        workspace_search_ready = true;
                        if let Some(warning) = warning {
                            app.set_notice(warning);
                        }
                    }
                    workspace::WorkspaceEvent::Suggestions { query_id, paths } => {
                        app.set_indexed_file_suggestions(query_id, paths);
                    }
                }
            }
        }
        if let Some(runner) = runner.as_ref() {
            while let Some(agent_event) = runner.try_event() {
                app.handle_agent_event(agent_event);
                redraw.mark_dirty();
            }
        }
        if let Some(preflight) =
            preflight_scheduler.take_if_idle(runner.as_ref().is_some_and(AgentTaskRunner::is_idle))
            && let Some(submission_runner) = submission_runner.as_ref()
        {
            start_preflight(&mut app, submission_runner, preflight);
            redraw.mark_dirty();
        }
        if let Some(submission_runner) = submission_runner.as_ref() {
            while let Some(event) = submission_runner.try_event() {
                redraw.mark_dirty();
                if let Some(action) = app.handle_submission_event(event) {
                    should_quit = dispatch(
                        action,
                        DispatchContext {
                            app: &mut app,
                            runner: runner.as_ref(),
                            submission_runner: Some(submission_runner),
                            model_runner: model_runner.as_ref(),
                            auth_runner: &auth_runner,
                            clipboard: &clipboard,
                            theme_runner: theme_runner.as_ref(),
                            preflight_scheduler: &mut preflight_scheduler,
                        },
                    );
                }
            }
        }
        while let Some(auth_event) = auth_runner.try_event() {
            app.handle_auth_event(auth_event);
            redraw.mark_dirty();
        }
        if let Some(model_runner) = model_runner.as_ref() {
            while let Some(event) = model_runner.try_event() {
                app.handle_model_catalog_event(event);
                redraw.mark_dirty();
            }
        }
        while let Some(clipboard_event) = clipboard.try_event() {
            handle_clipboard_event(&mut app, clipboard_event);
            redraw.mark_dirty();
        }
        if let Some(theme_runner) = theme_runner.as_ref() {
            while let Some(event) = theme_runner.try_event() {
                redraw.mark_dirty();
                match event {
                    ThemeConfigEvent::Saved(theme_id) => {
                        app.set_notice(format!("Theme changed to {}", theme_id.display_name()));
                    }
                    ThemeConfigEvent::Failed(error) => {
                        app.set_notice(format!("Could not save theme: {error}"));
                    }
                }
            }
        }

        let workspace_query = app.workspace_file_query();
        if workspace_search_ready
            && let (Some(workspace_runner), Some((query_id, query))) =
                (workspace_runner.as_ref(), workspace_query.as_ref())
        {
            let should_refresh =
                last_workspace_search
                    .as_ref()
                    .is_none_or(|(last_query_id, requested_at)| {
                        last_query_id != query_id
                            || requested_at.elapsed() >= WORKSPACE_SUGGESTION_REFRESH
                    });
            if should_refresh
                && workspace_runner.request_suggestions(
                    *query_id,
                    query.clone(),
                    FILE_SUGGESTION_LIMIT,
                )
            {
                last_workspace_search = Some((*query_id, Instant::now()));
            }
        } else if workspace_query.is_none() {
            last_workspace_search = None;
        }

        if redraw.take_dirty() {
            terminal
                .draw(|frame| {
                    let theme = Theme::resolve(app.effective_theme_id());
                    regions = ui::render(frame, &app, &theme);
                    selection.highlight(frame.buffer_mut());
                    rendered_buffer = frame.buffer_mut().clone();
                })
                .context("failed to draw the terminal UI")?;
            if let Some(area) = regions.composer_input {
                app.set_composer_width(area.width);
            }
        }

        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout).context("failed to poll terminal input")? {
            match event::read().context("failed to read terminal input")? {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press) => {
                    redraw.mark_dirty();
                    if let Some(action) = app.handle_key(key, Instant::now()) {
                        should_quit = dispatch(
                            action,
                            DispatchContext {
                                app: &mut app,
                                runner: runner.as_ref(),
                                submission_runner: submission_runner.as_ref(),
                                model_runner: model_runner.as_ref(),
                                auth_runner: &auth_runner,
                                clipboard: &clipboard,
                                theme_runner: theme_runner.as_ref(),
                                preflight_scheduler: &mut preflight_scheduler,
                            },
                        );
                    }
                }
                Event::Paste(text) => {
                    app.handle_paste(&text);
                    redraw.mark_dirty();
                }
                Event::Mouse(mouse) => {
                    redraw.mark_dirty();
                    match handle_selection_mouse_event(&mut selection, &rendered_buffer, mouse) {
                        Some(SelectionMouseEvent::Copy(text)) => {
                            if let Err(error) = clipboard.copy(text, "Selection copied") {
                                app.set_notice(error.to_string());
                            }
                        }
                        Some(SelectionMouseEvent::Click) | None => {
                            if let Some(action) = handle_mouse_event(&mut app, &regions, mouse) {
                                should_quit = dispatch(
                                    action,
                                    DispatchContext {
                                        app: &mut app,
                                        runner: runner.as_ref(),
                                        submission_runner: submission_runner.as_ref(),
                                        model_runner: model_runner.as_ref(),
                                        auth_runner: &auth_runner,
                                        clipboard: &clipboard,
                                        theme_runner: theme_runner.as_ref(),
                                        preflight_scheduler: &mut preflight_scheduler,
                                    },
                                );
                            }
                        }
                        Some(SelectionMouseEvent::Consumed) => {}
                    }
                }
                Event::Resize(_, _) => redraw.mark_dirty(),
                _ => {}
            }
        }

        if Instant::now() >= next_tick {
            if app.tick() {
                redraw.mark_dirty();
            }
            next_tick = Instant::now() + TICK_RATE;
        }
    }

    if let Some(runner) = runner.as_mut() {
        runner.shutdown();
    }
    if let Some(model_runner) = model_runner.as_mut() {
        model_runner.shutdown();
    }
    if let Some(submission_runner) = submission_runner.as_mut() {
        submission_runner.shutdown();
    }
    auth_runner.shutdown();
    clipboard.shutdown();
    if let Some(theme_runner) = theme_runner.as_mut() {
        theme_runner.shutdown();
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionMouseEvent {
    Consumed,
    Click,
    Copy(String),
}

fn handle_selection_mouse_event(
    selection: &mut TerminalSelection,
    buffer: &Buffer,
    mouse: crossterm::event::MouseEvent,
) -> Option<SelectionMouseEvent> {
    let position = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            selection.start(position);
            Some(SelectionMouseEvent::Consumed)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            selection.extend(position);
            Some(SelectionMouseEvent::Consumed)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            selection.extend(position);
            let dragged = selection.has_range();
            Some(match selection.finish(buffer) {
                Some(text) => SelectionMouseEvent::Copy(text),
                None if dragged => SelectionMouseEvent::Consumed,
                None => SelectionMouseEvent::Click,
            })
        }
        _ => None,
    }
}

fn handle_mouse_event(
    app: &mut App,
    regions: &ui::UiRegions,
    mouse: crossterm::event::MouseEvent,
) -> Option<AppAction> {
    let target = || regions.target_at(mouse.column, mouse.row);
    let event = match mouse.kind {
        MouseEventKind::Up(MouseButton::Left) => PointerEvent::Activate(target()),
        MouseEventKind::Moved => PointerEvent::Hover(target()),
        MouseEventKind::ScrollUp => PointerEvent::ScrollUp,
        MouseEventKind::ScrollDown => PointerEvent::ScrollDown,
        _ => return None,
    };
    app.handle_pointer(event)
}

struct DeferredPreflight {
    draft_id: DraftId,
    content: SubmittedContent,
    mode: SessionMode,
}

#[derive(Default)]
struct PreflightScheduler {
    deferred: Option<DeferredPreflight>,
}

impl PreflightScheduler {
    fn schedule(
        &mut self,
        agent_idle: bool,
        preflight: DeferredPreflight,
    ) -> Option<DeferredPreflight> {
        if agent_idle {
            Some(preflight)
        } else {
            self.deferred = Some(preflight);
            None
        }
    }

    fn take_if_idle(&mut self, agent_idle: bool) -> Option<DeferredPreflight> {
        agent_idle.then(|| self.deferred.take()).flatten()
    }

    fn cancel(&mut self, draft_id: DraftId) -> bool {
        if self
            .deferred
            .as_ref()
            .is_some_and(|preflight| preflight.draft_id == draft_id)
        {
            self.deferred = None;
            true
        } else {
            false
        }
    }
}

fn start_preflight(
    app: &mut App,
    submission_runner: &SubmissionTaskRunner,
    preflight: DeferredPreflight,
) {
    if let Err(error) =
        submission_runner.request(preflight.draft_id, preflight.content, preflight.mode)
    {
        let _ = app.handle_submission_event(SubmissionEvent::Failed {
            draft_id: preflight.draft_id,
            message: error.to_string(),
        });
    }
}

struct DispatchContext<'a> {
    app: &'a mut App,
    runner: Option<&'a AgentTaskRunner>,
    submission_runner: Option<&'a SubmissionTaskRunner>,
    model_runner: Option<&'a ModelCatalogTaskRunner>,
    auth_runner: &'a AuthTaskRunner,
    clipboard: &'a ClipboardTaskRunner,
    theme_runner: Option<&'a ThemeConfigTaskRunner>,
    preflight_scheduler: &'a mut PreflightScheduler,
}

fn dispatch(action: AppAction, context: DispatchContext<'_>) -> bool {
    let DispatchContext {
        app,
        runner,
        submission_runner,
        model_runner,
        auth_runner,
        clipboard,
        theme_runner,
        preflight_scheduler,
    } = context;
    match action {
        AppAction::Preflight {
            draft_id,
            content,
            mode,
        } => {
            match (runner, submission_runner) {
                (_, None) => {
                    let _ = app.handle_submission_event(SubmissionEvent::Failed {
                        draft_id,
                        message: "submission preflight is unavailable in authentication-only mode"
                            .into(),
                    });
                }
                (runner, Some(submission_runner)) => {
                    let preflight = DeferredPreflight {
                        draft_id,
                        content,
                        mode,
                    };
                    if let Some(preflight) = preflight_scheduler
                        .schedule(runner.is_none_or(AgentTaskRunner::is_idle), preflight)
                    {
                        start_preflight(app, submission_runner, preflight);
                    }
                }
            }
            false
        }
        AppAction::CancelPreflight { draft_id } => {
            if preflight_scheduler.cancel(draft_id) {
                let _ = app.handle_submission_event(SubmissionEvent::Cancelled { draft_id });
                return false;
            }
            if let Some(runner) = submission_runner
                && let Err(error) = runner.cancel(draft_id)
            {
                let _ = app.handle_submission_event(SubmissionEvent::Failed {
                    draft_id,
                    message: error.to_string(),
                });
            }
            false
        }
        AppAction::Submit {
            request_id,
            request,
        } => {
            match runner {
                Some(runner) => {
                    if let Err(error) = runner.submit_prepared(request_id, request) {
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
            if let Err(error) = clipboard.copy(text, "Message copied") {
                app.set_notice(error.to_string());
            }
            false
        }
        AppAction::ListModels => {
            match model_runner {
                Some(model_runner) => {
                    if let Err(error) = model_runner.load() {
                        app.handle_model_catalog_event(
                            crate::model_catalog::ModelCatalogEvent::Failed(error.to_string()),
                        );
                    }
                }
                None => {
                    app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Failed(
                        "model discovery is unavailable in authentication-only mode".into(),
                    ))
                }
            }
            false
        }
        AppAction::SaveTheme { theme_id } => {
            match theme_runner {
                Some(runner) => {
                    if let Err(error) = runner.save(theme_id) {
                        app.set_notice(format!("Could not save theme: {error}"));
                    }
                }
                None => app.set_notice("Could not locate theme configuration"),
            }
            false
        }
        AppAction::RefreshModels => {
            match model_runner {
                Some(model_runner) => {
                    if let Err(error) = model_runner.refresh() {
                        app.handle_model_catalog_event(
                            crate::model_catalog::ModelCatalogEvent::Failed(error.to_string()),
                        );
                    }
                }
                None => {
                    app.handle_model_catalog_event(crate::model_catalog::ModelCatalogEvent::Failed(
                        "model discovery is unavailable".into(),
                    ))
                }
            }
            false
        }
        AppAction::SelectModel { model } => {
            match model_runner {
                Some(model_runner) => {
                    if let Err(error) = model_runner.select_model(model.clone()) {
                        app.set_notice(error.to_string());
                    } else {
                        app.set_current_model(model.clone());
                        app.set_notice(format!("Model changed to {model}"));
                    }
                }
                None => app.set_notice("model selection is unavailable"),
            }
            false
        }
        AppAction::Quit => true,
    }
}

fn handle_clipboard_event(app: &mut App, event: ClipboardEvent) {
    match event {
        ClipboardEvent::Copied(message) => app.set_notice(message),
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
        DeferredPreflight, DispatchContext, LaunchMode, PreflightScheduler, RedrawScheduler,
        SelectionMouseEvent, dispatch, handle_clipboard_event, handle_mouse_event,
        handle_selection_mouse_event,
    };
    use crate::{
        app::{App, AppAction, Screen},
        auth::AuthTaskRunner,
        clipboard::{Clipboard, ClipboardTaskRunner},
        llm::{ModelInfo, ProviderModels},
        model_catalog::ModelCatalogEvent,
        terminal_selection::TerminalSelection,
        theme::ThemeId,
        ui::{ModelRegion, UiRegions},
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};
    use std::time::Duration;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn preflight(draft_id: u64) -> DeferredPreflight {
        DeferredPreflight {
            draft_id,
            content: crate::composer::SubmittedContent::plain("draft"),
            mode: crate::session::SessionMode::Build,
        }
    }

    #[test]
    fn deferred_preflight_starts_once_after_the_agent_becomes_idle() {
        let mut scheduler = PreflightScheduler::default();

        assert!(scheduler.schedule(false, preflight(7)).is_none());
        assert!(scheduler.take_if_idle(false).is_none());
        assert_eq!(scheduler.take_if_idle(true).unwrap().draft_id, 7);
        assert!(scheduler.take_if_idle(true).is_none());
    }

    #[test]
    fn cancelling_a_deferred_preflight_prevents_a_late_start() {
        let mut scheduler = PreflightScheduler::default();
        assert!(scheduler.schedule(false, preflight(9)).is_none());

        assert!(scheduler.cancel(9));
        assert!(!scheduler.cancel(9));
        assert!(scheduler.take_if_idle(true).is_none());
    }

    #[test]
    fn redraw_scheduler_stays_clean_across_idle_wakeups() {
        let mut redraw = RedrawScheduler::default();

        assert!(redraw.take_dirty());
        for _ in 0..100 {
            assert!(!redraw.take_dirty());
        }

        redraw.mark_dirty();
        assert!(redraw.take_dirty());
        assert!(!redraw.take_dirty());
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
    fn launch_mode_accepts_the_current_workspace_and_auth_subcommand() {
        assert!(matches!(
            LaunchMode::parse(Vec::<String>::new()).unwrap(),
            LaunchMode::Interactive(_)
        ));
        assert_eq!(LaunchMode::parse(["auth"]).unwrap(), LaunchMode::AuthOnly);
        assert!(LaunchMode::parse(["auth", "extra"]).is_err());
    }

    #[test]
    fn a_project_path_selects_that_workspace_and_invalid_paths_fail_early() {
        let workspace = tempfile::tempdir().unwrap();

        assert_eq!(
            LaunchMode::parse([workspace.path().as_os_str()]).unwrap(),
            LaunchMode::Interactive(workspace.path().canonicalize().unwrap())
        );
        assert!(LaunchMode::parse([workspace.path().join("missing")]).is_err());
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
                mouse(MouseEventKind::Up(MouseButton::Left), 4, 5)
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
    fn mouse_click_places_the_composer_cursor_at_the_clicked_column() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("alpha beta");
        let regions = UiRegions {
            composer_input: Some(Rect::new(10, 20, 40, 2)),
            ..UiRegions::default()
        };
        app.set_composer_width(40);

        handle_mouse_event(
            &mut app,
            &regions,
            mouse(MouseEventKind::Up(MouseButton::Left), 13, 20),
        );
        app.composer.insert_text("X");

        assert_eq!(app.composer.submission_text(), "alpXha beta");
    }

    #[test]
    fn mouse_previews_then_commits_themes() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.open_theme_dialog();
        let theme_regions = UiRegions {
            theme_options: (0..4).map(|index| Rect::new(4, 5 + index, 20, 1)).collect(),
            ..UiRegions::default()
        };
        handle_mouse_event(&mut app, &theme_regions, mouse(MouseEventKind::Moved, 4, 7));
        assert_eq!(app.effective_theme_id(), ThemeId::Midnight);
        assert_eq!(
            handle_mouse_event(
                &mut app,
                &theme_regions,
                mouse(MouseEventKind::Up(MouseButton::Left), 4, 7),
            ),
            Some(AppAction::SaveTheme {
                theme_id: ThemeId::Midnight,
            })
        );
    }

    #[test]
    fn mouse_hover_highlights_and_click_selects_a_model() {
        let mut app = App::new();
        app.open_models_dialog();
        app.handle_model_catalog_event(ModelCatalogEvent::Loaded(vec![ProviderModels {
            provider: "Test".into(),
            source: "built-in catalog".into(),
            models: vec![
                ModelInfo {
                    id: "model-a".into(),
                    display_name: "Model A".into(),
                },
                ModelInfo {
                    id: "model-b".into(),
                    display_name: "Model B".into(),
                },
            ],
        }]));
        let regions = UiRegions {
            models: vec![
                ModelRegion {
                    index: 0,
                    area: Rect::new(4, 5, 20, 1),
                },
                ModelRegion {
                    index: 1,
                    area: Rect::new(4, 6, 20, 1),
                },
            ],
            ..UiRegions::default()
        };

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::Moved, 4, 6));
        assert_eq!(app.selected_model_index(), 1);
        assert_eq!(
            handle_mouse_event(
                &mut app,
                &regions,
                mouse(MouseEventKind::Up(MouseButton::Left), 4, 6),
            ),
            Some(AppAction::SelectModel {
                model: "model-b".into(),
            })
        );
        assert_eq!(app.current_model(), "model-b");
    }

    #[test]
    fn clicking_model_refresh_bypasses_the_cached_catalog() {
        let mut app = App::new();
        app.open_models_dialog();
        app.handle_model_catalog_event(ModelCatalogEvent::Loaded(Vec::new()));
        let regions = UiRegions {
            model_refresh: Some(Rect::new(4, 8, 9, 1)),
            ..UiRegions::default()
        };

        assert_eq!(
            handle_mouse_event(
                &mut app,
                &regions,
                mouse(MouseEventKind::Up(MouseButton::Left), 4, 8),
            ),
            Some(AppAction::RefreshModels)
        );
        assert!(matches!(
            app.models_dialog,
            Some(crate::app::ModelsDialogPhase::Loading)
        ));
    }

    #[test]
    fn releasing_a_mouse_drag_copies_the_selected_screen_text() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(0, 0, "copy this", Style::default());
        let mut selection = TerminalSelection::default();

        assert_eq!(
            handle_selection_mouse_event(
                &mut selection,
                &buffer,
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ),
            Some(SelectionMouseEvent::Consumed)
        );
        assert_eq!(
            handle_selection_mouse_event(
                &mut selection,
                &buffer,
                mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            ),
            Some(SelectionMouseEvent::Consumed)
        );
        assert_eq!(
            handle_selection_mouse_event(
                &mut selection,
                &buffer,
                mouse(MouseEventKind::Up(MouseButton::Left), 3, 0),
            ),
            Some(SelectionMouseEvent::Copy("copy".into()))
        );
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
    fn mouse_wheel_moves_the_models_selection_instead_of_the_hidden_transcript() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.open_models_dialog();
        app.handle_model_catalog_event(ModelCatalogEvent::Loaded(vec![ProviderModels {
            provider: "Test".into(),
            source: "built-in catalog".into(),
            models: vec![
                ModelInfo {
                    id: "model-a".into(),
                    display_name: "Model A".into(),
                },
                ModelInfo {
                    id: "model-b".into(),
                    display_name: "Model B".into(),
                },
            ],
        }]));
        let regions = UiRegions::default();

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollDown, 0, 0));

        assert_eq!(app.selected_model_index(), 1);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn message_owner_blocks_hidden_suggestion_wheel_input() {
        let mut app = App::with_files(["src/app.rs", "src/runtime.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src");
        app.set_suggestion_selection(0);
        app.transcript.submit(1, "sent".into(), Vec::new());
        app.open_message_dialog(app.transcript.entries()[0].id);

        handle_mouse_event(
            &mut app,
            &UiRegions::default(),
            mouse(MouseEventKind::ScrollDown, 0, 0),
        );

        assert_eq!(app.selected_suggestion(), 0);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn modal_owner_blocks_clicks_on_background_transcript_regions() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.transcript.submit(1, "sent".into(), Vec::new());
        let entry_id = app.transcript.entries()[0].id;
        app.open_auth_dialog();
        let regions = UiRegions {
            transcript_entries: vec![crate::ui::transcript::EntryRegion {
                id: entry_id,
                area: Rect::new(2, 2, 20, 3),
            }],
            ..UiRegions::default()
        };

        handle_mouse_event(
            &mut app,
            &regions,
            mouse(MouseEventKind::Up(MouseButton::Left), 3, 3),
        );

        assert!(app.message_dialog.is_none());
        assert!(app.auth_dialog.is_some());
    }

    #[test]
    fn copy_action_uses_the_clipboard_and_shows_confirmation() {
        let mut app = App::new();
        let mut auth_runner = AuthTaskRunner::spawn();
        let mut clipboard = ClipboardTaskRunner::spawn_with(RecordingClipboard);
        let mut preflight_scheduler = PreflightScheduler::default();

        assert!(!dispatch(
            AppAction::CopyToClipboard {
                text: "copied message".into(),
            },
            DispatchContext {
                app: &mut app,
                runner: None,
                submission_runner: None,
                model_runner: None,
                auth_runner: &auth_runner,
                clipboard: &clipboard,
                theme_runner: None,
                preflight_scheduler: &mut preflight_scheduler,
            },
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
