use crate::{
    agent::{AgentEvent, AgentTaskRunner},
    app::{App, AppAction, AppInputOutcome, AuthProvider, FILE_SUGGESTION_LIMIT, PointerEvent},
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
    Command,
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event,
        KeyEventKind, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
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
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const WORKSPACE_SUGGESTION_REFRESH: Duration = Duration::from_millis(500);

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableButtonMouseCapture;

impl Command for EnableButtonMouseCapture {
    fn write_ansi(&self, output: &mut impl std::fmt::Write) -> std::fmt::Result {
        output.write_str(concat!(
            "\x1b[?1000h",
            "\x1b[?1002h",
            "\x1b[?1015h",
            "\x1b[?1006h",
        ))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::EnableMouseCapture.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SetAnyMouseMotion(bool);

impl Command for SetAnyMouseMotion {
    fn write_ansi(&self, output: &mut impl std::fmt::Write) -> std::fmt::Result {
        output.write_str(if self.0 { "\x1b[?1003h" } else { "\x1b[?1003l" })
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct RedrawScheduler {
    dirty: bool,
    urgent: bool,
    next_frame: Option<Instant>,
}

impl Default for RedrawScheduler {
    fn default() -> Self {
        Self {
            dirty: true,
            urgent: true,
            next_frame: None,
        }
    }
}

impl RedrawScheduler {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_urgent(&mut self) {
        self.dirty = true;
        self.urgent = true;
    }

    fn should_draw(&self, now: Instant) -> bool {
        self.dirty && (self.urgent || self.next_frame.is_none_or(|deadline| now >= deadline))
    }

    fn frame_rendered(&mut self, now: Instant) {
        self.dirty = false;
        self.urgent = false;
        self.next_frame = Some(now + FRAME_INTERVAL);
    }

    fn deadline(&self) -> Option<Instant> {
        self.dirty.then(|| {
            if self.urgent {
                Instant::now()
            } else {
                self.next_frame.unwrap_or_else(Instant::now)
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchedEvent {
    Key(crossterm::event::KeyEvent),
    Paste(String),
    Mouse(crossterm::event::MouseEvent),
    Scroll { delta: isize },
    Resize(u16, u16),
}

#[derive(Debug, Default)]
struct TerminalEventBatch {
    events: Vec<BatchedEvent>,
}

impl TerminalEventBatch {
    fn from_events(events: impl IntoIterator<Item = Event>) -> Self {
        let mut batch = Self::default();
        for event in events {
            match event {
                Event::Key(key) => batch.events.push(BatchedEvent::Key(key)),
                Event::Paste(text) => batch.events.push(BatchedEvent::Paste(text)),
                Event::Resize(width, height) => {
                    batch.events.push(BatchedEvent::Resize(width, height));
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    let direction = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        1
                    } else {
                        -1
                    };
                    if let Some(BatchedEvent::Scroll { delta }) = batch.events.last_mut() {
                        *delta = delta.saturating_add(direction);
                    } else {
                        batch.events.push(BatchedEvent::Scroll { delta: direction });
                    }
                }
                Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Moved) => {
                    if let Some(BatchedEvent::Mouse(previous)) = batch.events.last_mut()
                        && matches!(previous.kind, MouseEventKind::Moved)
                    {
                        *previous = mouse;
                    } else {
                        batch.events.push(BatchedEvent::Mouse(mouse));
                    }
                }
                Event::Mouse(mouse) => batch.events.push(BatchedEvent::Mouse(mouse)),
                _ => {}
            }
        }
        batch
    }

    fn events(&self) -> &[BatchedEvent] {
        &self.events
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.events.len()
    }
}

#[derive(Debug, Default)]
struct InputOutcome {
    action: Option<AppAction>,
    changed: bool,
    urgent: bool,
    rendered_at: Option<Instant>,
}

impl From<AppInputOutcome> for InputOutcome {
    fn from(outcome: AppInputOutcome) -> Self {
        Self {
            action: outcome.action,
            changed: outcome.changed,
            urgent: outcome.urgent,
            rendered_at: None,
        }
    }
}

fn apply_app_input(
    app: &mut App,
    regions: &ui::UiRegions,
    event: &BatchedEvent,
    now: Instant,
) -> InputOutcome {
    match event {
        BatchedEvent::Key(key) if matches!(key.kind, KeyEventKind::Press) => {
            app.handle_key_with_outcome(*key, now).into()
        }
        BatchedEvent::Paste(text) => app.handle_paste_with_outcome(text).into(),
        BatchedEvent::Mouse(mouse) => {
            let outcome = handle_mouse_event_outcome(app, regions, *mouse);
            InputOutcome {
                action: outcome.action,
                changed: outcome.changed,
                urgent: false,
                rendered_at: None,
            }
        }
        BatchedEvent::Scroll { delta, .. } => {
            let outcome = app.handle_pointer(PointerEvent::Scroll(*delta));
            InputOutcome {
                action: outcome.action,
                changed: outcome.changed,
                urgent: false,
                rendered_at: None,
            }
        }
        BatchedEvent::Resize(_, _) => InputOutcome {
            changed: true,
            urgent: true,
            ..InputOutcome::default()
        },
        _ => InputOutcome::default(),
    }
}

fn process_event_batch(
    events: &[BatchedEvent],
    redraw: &mut RedrawScheduler,
    mut process: impl FnMut(&BatchedEvent) -> Result<(InputOutcome, bool)>,
) -> Result<bool> {
    for event in events {
        let (outcome, should_quit) = process(event)?;
        if let Some(rendered_at) = outcome.rendered_at {
            redraw.frame_rendered(rendered_at);
        }
        if outcome.urgent {
            redraw.mark_urgent();
        } else if outcome.changed {
            redraw.mark_dirty();
        }
        if should_quit {
            return Ok(true);
        }
    }
    Ok(false)
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
    if let Some(model_runner) = model_runner.as_ref()
        && let Err(error) = model_runner.load()
    {
        app.set_notice(format!("Could not load model capabilities: {error}"));
    }
    let mut auth_runner = AuthTaskRunner::spawn();
    let mut clipboard = ClipboardTaskRunner::spawn();
    let mut next_tick = Instant::now() + TICK_RATE;
    let mut should_quit = false;
    let mut regions = ui::UiRegions::default();
    let ui_renderer = ui::UiRenderer::default();
    let mut selection = TerminalSelection::default();
    let mut selection_buffer: Option<Buffer> = None;
    let mut any_mouse_motion = false;
    let mut last_workspace_search: Option<(crate::composer::QueryId, Instant)> = None;
    let mut workspace_search_ready = false;
    let mut redraw = RedrawScheduler::default();
    let mut preflight_scheduler = PreflightScheduler::default();

    while !should_quit {
        if let Some(workspace_runner) = workspace_runner.as_ref() {
            while let Some(event) = workspace_runner.try_event() {
                let urgent = matches!(
                    &event,
                    workspace::WorkspaceEvent::Ready { warning: Some(_) }
                );
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
                if urgent {
                    redraw.mark_urgent();
                } else {
                    redraw.mark_dirty();
                }
            }
        }
        if let Some(runner) = runner.as_ref() {
            while let Some(agent_event) = runner.try_event() {
                let urgent = matches!(
                    &agent_event,
                    AgentEvent::ToolFailed { .. } | AgentEvent::Failed { .. }
                );
                app.handle_agent_event(agent_event);
                if urgent {
                    redraw.mark_urgent();
                } else {
                    redraw.mark_dirty();
                }
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
                let urgent = matches!(&event, SubmissionEvent::Failed { .. });
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
                if urgent {
                    redraw.mark_urgent();
                } else {
                    redraw.mark_dirty();
                }
            }
        }
        while let Some(auth_event) = auth_runner.try_event() {
            let urgent = matches!(&auth_event, AuthEvent::Failed { .. });
            app.handle_auth_event(auth_event);
            if urgent {
                redraw.mark_urgent();
            } else {
                redraw.mark_dirty();
            }
        }
        if let Some(model_runner) = model_runner.as_ref() {
            while let Some(event) = model_runner.try_event() {
                let urgent = matches!(&event, crate::model_catalog::ModelCatalogEvent::Failed(_));
                app.handle_model_catalog_event(event);
                if urgent {
                    redraw.mark_urgent();
                } else {
                    redraw.mark_dirty();
                }
            }
        }
        while let Some(clipboard_event) = clipboard.try_event() {
            let urgent = matches!(&clipboard_event, ClipboardEvent::Failed(_));
            handle_clipboard_event(&mut app, clipboard_event);
            if urgent {
                redraw.mark_urgent();
            } else {
                redraw.mark_dirty();
            }
        }
        if let Some(theme_runner) = theme_runner.as_ref() {
            while let Some(event) = theme_runner.try_event() {
                let urgent = matches!(&event, ThemeConfigEvent::Failed(_));
                match event {
                    ThemeConfigEvent::Saved(theme_id) => {
                        app.set_notice(format!("Theme changed to {}", theme_id.display_name()));
                    }
                    ThemeConfigEvent::Failed(error) => {
                        app.set_notice(format!("Could not save theme: {error}"));
                    }
                }
                if urgent {
                    redraw.mark_urgent();
                } else {
                    redraw.mark_dirty();
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

        let render_started = Instant::now();
        if redraw.should_draw(render_started) {
            render_frame(
                terminal,
                &mut app,
                &ui_renderer,
                &selection,
                &mut regions,
                &mut any_mouse_motion,
                false,
            )?;
            redraw.frame_rendered(render_started);
        }

        let now = Instant::now();
        let background_wake = app
            .active_request
            .map(|_| now + FRAME_INTERVAL)
            .unwrap_or(next_tick);
        let wake_at = redraw
            .deadline()
            .map_or(background_wake.min(next_tick), |frame_deadline| {
                frame_deadline.min(background_wake).min(next_tick)
            });
        let timeout = wake_at.saturating_duration_since(now);
        if event::poll(timeout).context("failed to poll terminal input")? {
            let mut events = vec![event::read().context("failed to read terminal input")?];
            while event::poll(Duration::ZERO).context("failed to drain terminal input")? {
                events.push(event::read().context("failed to read queued terminal input")?);
            }
            let batch = TerminalEventBatch::from_events(events);
            should_quit = process_event_batch(batch.events(), &mut redraw, |event| {
                let mut rendered_at = None;
                let mut outcome = match event {
                    BatchedEvent::Mouse(mouse) => {
                        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                            && selection_buffer.is_none()
                        {
                            selection_buffer = render_frame(
                                terminal,
                                &mut app,
                                &ui_renderer,
                                &selection,
                                &mut regions,
                                &mut any_mouse_motion,
                                true,
                            )?;
                            rendered_at = Some(Instant::now());
                        }
                        match handle_selection_mouse_event(
                            &mut selection,
                            selection_buffer.as_ref(),
                            *mouse,
                        ) {
                            Some(SelectionMouseEvent::Copy(text)) => {
                                if let Err(error) = clipboard.copy(text, "Selection copied") {
                                    app.set_notice(error.to_string());
                                    InputOutcome {
                                        changed: true,
                                        urgent: true,
                                        ..InputOutcome::default()
                                    }
                                } else {
                                    InputOutcome {
                                        changed: true,
                                        ..InputOutcome::default()
                                    }
                                }
                            }
                            Some(SelectionMouseEvent::Click) | None => {
                                apply_app_input(&mut app, &regions, event, Instant::now())
                            }
                            Some(SelectionMouseEvent::Consumed) => InputOutcome {
                                changed: true,
                                ..InputOutcome::default()
                            },
                        }
                    }
                    _ => apply_app_input(&mut app, &regions, event, Instant::now()),
                };
                outcome.rendered_at = rendered_at;
                let notice_before_dispatch = app.notice.clone();
                let event_should_quit = outcome.action.take().is_some_and(|action| {
                    dispatch(
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
                    )
                });
                if app.notice != notice_before_dispatch && app.notice.is_some() {
                    outcome.changed = true;
                    outcome.urgent = true;
                }
                if matches!(
                    event,
                    BatchedEvent::Mouse(crossterm::event::MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        ..
                    })
                ) {
                    selection_buffer = None;
                }
                Ok::<_, anyhow::Error>((outcome, event_should_quit))
            })?;
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

fn render_frame(
    terminal: &mut AppTerminal,
    app: &mut App,
    ui_renderer: &ui::UiRenderer,
    selection: &TerminalSelection,
    regions: &mut ui::UiRegions,
    any_mouse_motion: &mut bool,
    capture_buffer: bool,
) -> Result<Option<Buffer>> {
    let mut snapshot = None;
    terminal
        .draw(|frame| {
            let theme = Theme::resolve(app.effective_theme_id());
            *regions = ui_renderer.render(frame, app, &theme);
            selection.highlight(frame.buffer_mut());
            if capture_buffer {
                snapshot = Some(frame.buffer_mut().clone());
            }
        })
        .context("failed to draw the terminal UI")?;
    if let Some(area) = regions.composer_input {
        app.set_composer_width(area.width);
    }
    app.update_transcript_scroll_maximum(regions.transcript_scroll_maximum);
    let wants_pointer_motion = regions.wants_pointer_motion();
    if wants_pointer_motion != *any_mouse_motion {
        execute!(stdout(), SetAnyMouseMotion(wants_pointer_motion))
            .context("failed to update terminal mouse motion reporting")?;
        *any_mouse_motion = wants_pointer_motion;
    }
    Ok(snapshot)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionMouseEvent {
    Consumed,
    Click,
    Copy(String),
}

fn handle_selection_mouse_event(
    selection: &mut TerminalSelection,
    buffer: Option<&Buffer>,
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
            let copied = buffer.and_then(|buffer| selection.finish(buffer));
            if buffer.is_none() {
                selection.clear();
            }
            Some(match copied {
                Some(text) => SelectionMouseEvent::Copy(text),
                None if dragged => SelectionMouseEvent::Consumed,
                None => SelectionMouseEvent::Click,
            })
        }
        _ => None,
    }
}

fn handle_mouse_event_outcome(
    app: &mut App,
    regions: &ui::UiRegions,
    mouse: crossterm::event::MouseEvent,
) -> crate::app::PointerOutcome {
    let target = || regions.target_at(mouse.column, mouse.row);
    let event = match mouse.kind {
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(area) = regions.composer_input
                && area.contains(Position::new(mouse.column, mouse.row))
            {
                PointerEvent::PlaceComposerCursor {
                    column: mouse.column.saturating_sub(area.x),
                    row: mouse.row.saturating_sub(area.y),
                    height: area.height,
                }
            } else {
                PointerEvent::Activate(target())
            }
        }
        MouseEventKind::Moved => PointerEvent::Hover(target()),
        MouseEventKind::ScrollUp => PointerEvent::Scroll(1),
        MouseEventKind::ScrollDown => PointerEvent::Scroll(-1),
        _ => return crate::app::PointerOutcome::default(),
    };
    app.handle_pointer(event)
}

#[cfg(test)]
fn handle_mouse_event(
    app: &mut App,
    regions: &ui::UiRegions,
    mouse: crossterm::event::MouseEvent,
) -> Option<AppAction> {
    handle_mouse_event_outcome(app, regions, mouse).action
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
                EnableButtonMouseCapture,
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
        BatchedEvent, DeferredPreflight, DispatchContext, EnableButtonMouseCapture, LaunchMode,
        PreflightScheduler, RedrawScheduler, SelectionMouseEvent, SetAnyMouseMotion,
        TerminalEventBatch, apply_app_input, dispatch, handle_clipboard_event, handle_mouse_event,
        handle_selection_mouse_event, process_event_batch,
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
    use crossterm::Command;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
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
        let start = std::time::Instant::now();
        let mut redraw = RedrawScheduler::default();

        assert!(redraw.should_draw(start));
        redraw.frame_rendered(start);
        for _ in 0..100 {
            assert!(!redraw.should_draw(start));
        }

        for _ in 0..1_000 {
            redraw.mark_dirty();
        }
        assert!(!redraw.should_draw(start + Duration::from_millis(1)));
        assert!(redraw.should_draw(start + super::FRAME_INTERVAL));
        redraw.frame_rendered(start + super::FRAME_INTERVAL);
        assert!(!redraw.should_draw(start + super::FRAME_INTERVAL));
    }

    #[test]
    fn event_batch_coalesces_wheel_and_motion_bursts_without_starving_keyboard_input() {
        let mut events = Vec::new();
        events.extend((0..10_000).map(|_| Event::Mouse(mouse(MouseEventKind::ScrollUp, 0, 0))));
        events.extend(
            (0..10_000)
                .map(|column| Event::Mouse(mouse(MouseEventKind::Moved, (column % 80) as u16, 4))),
        );
        events.push(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));

        let batch = TerminalEventBatch::from_events(events);
        assert_eq!(batch.len(), 3);
        assert!(matches!(
            batch.events()[0],
            BatchedEvent::Scroll { delta: 10_000, .. }
        ));

        let mut app = App::new();
        app.screen = Screen::Chat;
        app.update_transcript_scroll_maximum(25);
        let regions = UiRegions::default();
        let start = std::time::Instant::now();
        let mut redraw = RedrawScheduler::default();
        redraw.frame_rendered(start);
        let should_quit = process_event_batch(batch.events(), &mut redraw, |event| {
            Ok((apply_app_input(&mut app, &regions, event, start), false))
        })
        .unwrap();

        assert!(!should_quit);
        assert_eq!(app.transcript_scroll_offset(25), 25);
        assert_eq!(app.composer.submission_text(), "x");
        assert!(!redraw.should_draw(start + Duration::from_millis(1)));

        let frame_time = start + super::FRAME_INTERVAL;
        let mut renders_after_batch = 0;
        if redraw.should_draw(frame_time) {
            renders_after_batch += 1;
            redraw.frame_rendered(frame_time);
        }
        assert_eq!(renders_after_batch, 1);
        assert!(!redraw.should_draw(frame_time));
    }

    #[test]
    fn ignored_keyboard_and_paste_input_do_not_dirty_the_frame() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        let regions = UiRegions::default();
        let ignored = BatchedEvent::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));

        assert!(!apply_app_input(&mut app, &regions, &ignored, std::time::Instant::now()).changed);

        app.open_theme_dialog();
        let ignored_paste = BatchedEvent::Paste("not accepted by a dialog".into());
        assert!(
            !apply_app_input(
                &mut app,
                &regions,
                &ignored_paste,
                std::time::Instant::now()
            )
            .changed
        );
    }

    #[test]
    fn opening_a_modal_requests_an_urgent_frame() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.composer.insert_text("/theme");
        let enter = BatchedEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let outcome = apply_app_input(
            &mut app,
            &UiRegions::default(),
            &enter,
            std::time::Instant::now(),
        );

        assert!(outcome.changed);
        assert!(outcome.urgent);
        assert!(app.theme_dialog.is_some());
    }

    #[test]
    fn redundant_hover_and_boundary_scroll_events_do_not_schedule_redraws() {
        let mut app = App::with_files(["src/app.rs", "src/main.rs"]);
        app.screen = Screen::Chat;
        app.composer.insert_text("@src/");
        let regions = UiRegions {
            suggestions: vec![Rect::new(4, 5, 12, 1), Rect::new(4, 6, 12, 1)],
            ..UiRegions::default()
        };
        let hover = BatchedEvent::Mouse(mouse(MouseEventKind::Moved, 4, 6));

        assert!(apply_app_input(&mut app, &regions, &hover, std::time::Instant::now()).changed);
        assert!(!apply_app_input(&mut app, &regions, &hover, std::time::Instant::now()).changed);

        let empty_click = BatchedEvent::Mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 0));
        assert!(
            !apply_app_input(
                &mut app,
                &UiRegions::default(),
                &empty_click,
                std::time::Instant::now()
            )
            .changed
        );

        let mut scroll_app = App::new();
        scroll_app.screen = Screen::Chat;
        scroll_app.update_transcript_scroll_maximum(5);
        let scroll = BatchedEvent::Scroll { delta: 10_000 };
        assert!(
            apply_app_input(
                &mut scroll_app,
                &UiRegions::default(),
                &scroll,
                std::time::Instant::now()
            )
            .changed
        );
        assert!(
            !apply_app_input(
                &mut scroll_app,
                &UiRegions::default(),
                &scroll,
                std::time::Instant::now()
            )
            .changed
        );
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
    fn mouse_capture_reports_click_wheel_and_drag_without_all_motion_mode() {
        let mut ansi = String::new();
        EnableButtonMouseCapture.write_ansi(&mut ansi).unwrap();

        assert!(ansi.contains("?1000h"));
        assert!(ansi.contains("?1002h"));
        assert!(ansi.contains("?1006h"));
        assert!(!ansi.contains("?1003h"));

        ansi.clear();
        SetAnyMouseMotion(true).write_ansi(&mut ansi).unwrap();
        assert_eq!(ansi, "\x1b[?1003h");

        ansi.clear();
        SetAnyMouseMotion(false).write_ansi(&mut ansi).unwrap();
        assert_eq!(ansi, "\x1b[?1003l");
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
    fn mouse_click_on_context_usage_starts_the_pop_feedback() {
        let mut app = App::new();
        let regions = UiRegions {
            context_usage: Some(Rect::new(80, 1, 14, 6)),
            ..UiRegions::default()
        };

        assert_eq!(
            handle_mouse_event(
                &mut app,
                &regions,
                mouse(MouseEventKind::Up(MouseButton::Left), 82, 3)
            ),
            None
        );
        assert_eq!(app.context_usage_pop_frames(), 5);
        assert_eq!(app.context_usage_pop_origin(), Some((2, 2)));
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
                    context_window: None,
                },
                ModelInfo {
                    id: "model-b".into(),
                    display_name: "Model B".into(),
                    context_window: None,
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
                Some(&buffer),
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ),
            Some(SelectionMouseEvent::Consumed)
        );
        assert_eq!(
            handle_selection_mouse_event(
                &mut selection,
                Some(&buffer),
                mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            ),
            Some(SelectionMouseEvent::Consumed)
        );
        assert_eq!(
            handle_selection_mouse_event(
                &mut selection,
                Some(&buffer),
                mouse(MouseEventKind::Up(MouseButton::Left), 3, 0),
            ),
            Some(SelectionMouseEvent::Copy("copy".into()))
        );
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_when_no_overlay_owns_it() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.update_transcript_scroll_maximum(20);
        let regions = UiRegions::default();

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(app.transcript_scroll_offset(20), 5);
        assert!(!app.transcript_is_following());

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(app.transcript_scroll_offset(20), 0);
        assert!(app.transcript_is_following());
    }

    #[test]
    fn clicking_inside_the_composer_places_the_editing_cursor() {
        let mut app = App::new();
        app.set_composer_width(10);
        app.composer.insert_text("abcdefghij");
        let regions = UiRegions {
            composer_input: Some(Rect::new(2, 4, 10, 2)),
            ..UiRegions::default()
        };

        handle_mouse_event(
            &mut app,
            &regions,
            mouse(MouseEventKind::Up(MouseButton::Left), 5, 4),
        );
        app.composer.insert_text("X");

        assert_eq!(app.composer.submission_text(), "abcXdefghij");
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
                    context_window: None,
                },
                ModelInfo {
                    id: "model-b".into(),
                    display_name: "Model B".into(),
                    context_window: None,
                },
            ],
        }]));
        let regions = UiRegions::default();

        handle_mouse_event(&mut app, &regions, mouse(MouseEventKind::ScrollDown, 0, 0));

        assert_eq!(app.selected_model_index(), 1);
        assert!(app.transcript_is_following());
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
        assert!(app.transcript_is_following());
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
