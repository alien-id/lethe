//! TUI event loop. Owns the terminal, the editor, the rendered state, and
//! the background tasks that fan in SSE/HTTP results. Designed so the
//! frontend stays responsive even while the agent is mid-turn.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tui_textarea::{Input, Key, TextArea};

use crate::tui::autocomplete::Autocomplete;
use crate::tui::client::{LetheClient, PreflightError};
use crate::tui::events::UiEvent;
use crate::tui::state::{AppState, Pane};
use crate::tui::view;

const CHAT_ID: i64 = 1;
const USER_ID: i64 = 1;
const ACTOR_REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub base_url: String,
    pub token: String,
    pub workspace: PathBuf,
}

pub async fn run(opts: TuiOptions) -> Result<()> {
    let client = LetheClient::new(opts.base_url.clone(), opts.token.clone())
        .context("build lethe client")?;

    // Fail-fast preflight against an auth-required endpoint. We do this
    // *before* entering the alternate screen so any error renders as a
    // plain stderr line, not as half-painted UI behind a 401 spinner.
    let initial_model = match client.preflight().await {
        Ok(info) => Some(info),
        Err(PreflightError::Unauthorized) => {
            return Err(anyhow!(
                "lethe tui: API at {} rejected the bearer token (401). \
                 Set LETHE_API_TOKEN to match what `lethe api` is using, \
                 or pass --token <value>.",
                opts.base_url
            ));
        }
        Err(PreflightError::Unreachable { url, source }) => {
            return Err(anyhow!(
                "lethe tui: cannot reach API at {url}: {source}\n\
                 Is `lethe api` running? Check `systemctl --user status lethe`."
            ));
        }
        Err(PreflightError::Other(error)) => {
            return Err(anyhow!("lethe tui: API preflight failed: {error}"));
        }
    };

    let mut terminal = enter_terminal()?;
    let result = drive(&mut terminal, client, opts.workspace, initial_model).await;
    leave_terminal(&mut terminal)?;
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn enter_terminal() -> Result<Term> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alt screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("init terminal")
}

fn leave_terminal(terminal: &mut Term) -> Result<()> {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
    Ok(())
}

async fn drive(
    terminal: &mut Term,
    client: LetheClient,
    workspace: PathBuf,
    initial_model: Option<crate::tui::client::ModelInfo>,
) -> Result<()> {
    let mut app = AppState::new();
    let mut editor = make_editor();
    let autocomplete = Autocomplete::new(&workspace);

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<AppCommand>(64);

    // Background: long-lived /events stream (proactive + actor.*).
    let events_client = client.clone();
    let events_tx = ui_tx.clone();
    let _events_task = tokio::spawn(async move {
        let _ = events_client.events_stream(events_tx).await;
    });

    // Use the model info captured by preflight so the footer paints
    // correctly from the first frame, no follow-up GET needed.
    if let Some(info) = initial_model {
        app.set_model(info.model, info.provider);
        app.status = crate::tui::state::Status::Idle;
    }
    refresh_sidebar(&client, &cmd_tx).await;

    // Per-turn /chat handle, so cancel/quit can drop it.
    let mut chat_task: Option<JoinHandle<()>> = None;
    // Refresh actor list a little after every actor.* event to coalesce
    // bursts (an agent spawns 5 hypotheses → one GET instead of five).
    let mut pending_actor_refresh: Option<tokio::time::Instant> = None;

    let mut key_stream = EventStream::new();

    loop {
        terminal.draw(|frame| {
            view::draw(frame, &mut app, &editor);
            draw_autocomplete_popup(frame, &editor, &autocomplete);
        })?;

        tokio::select! {
            // Keyboard / mouse / resize.
            event = key_stream.next() => {
                let Some(Ok(event)) = event else { break; };
                if handle_terminal_event(
                    event,
                    &mut app,
                    &mut editor,
                    &autocomplete,
                    &cmd_tx,
                ).await {
                    break;
                }
            }
            // Streamed UI updates from the server.
            event = ui_rx.recv() => {
                if let Some(event) = event {
                    if let UiEvent::ActorEvent { .. } = &event {
                        pending_actor_refresh = Some(
                            tokio::time::Instant::now() + ACTOR_REFRESH_DEBOUNCE,
                        );
                    }
                    if let UiEvent::ToolEnd { name, .. } = &event {
                        if name.starts_with("todo") {
                            let _ = cmd_tx.try_send(AppCommand::RefreshTodos);
                        }
                    }
                    app.apply_event(event);
                }
            }
            // App-internal commands from the keyboard handler.
            command = cmd_rx.recv() => {
                if let Some(command) = command {
                    match command {
                        AppCommand::SendMessage(message) => {
                            app.push_user(message.clone());
                            app.status = crate::tui::state::Status::Thinking;
                            let chat_client = client.clone();
                            let chat_tx = ui_tx.clone();
                            chat_task = Some(tokio::spawn(async move {
                                if let Err(error) = chat_client
                                    .chat_stream(CHAT_ID, USER_ID, message, chat_tx.clone())
                                    .await
                                {
                                    let _ = chat_tx
                                        .send(UiEvent::Disconnected(format!("chat: {error}")))
                                        .await;
                                }
                            }));
                        }
                        AppCommand::Cancel => {
                            let _ = client.cancel(CHAT_ID).await;
                            if let Some(task) = chat_task.take() {
                                task.abort();
                            }
                            app.push_notice("cancelled");
                            app.status = crate::tui::state::Status::Idle;
                        }
                        AppCommand::RefreshActors => match client.list_actors().await {
                            Ok(actors) => app.replace_actors(actors),
                            Err(error) => tracing::warn!(error = %error, "list_actors failed"),
                        },
                        AppCommand::RefreshTodos => match client.list_todos(false).await {
                            Ok(todos) => app.replace_todos(todos),
                            Err(error) => tracing::warn!(error = %error, "list_todos failed"),
                        },
                        AppCommand::SwitchModel(_) => {
                            // Stub: future /model POST. Refresh display.
                            if let Ok(info) = client.model().await {
                                app.set_model(info.model, info.provider);
                            }
                        }
                        AppCommand::Quit => break,
                    }
                }
            }
            // Debounced sidebar refresh.
            _ = sleep_until(pending_actor_refresh) => {
                pending_actor_refresh = None;
                let _ = cmd_tx.try_send(AppCommand::RefreshActors);
                let _ = cmd_tx.try_send(AppCommand::RefreshTodos);
            }
            // Animation tick. 100ms while thinking (smooth spinner),
            // 1s otherwise (keeps timestamps fresh without waking too
            // often).
            _ = tokio::time::sleep(tick_interval(&app)) => {
                app.tick = app.tick.wrapping_add(1);
            }
        }
    }

    if let Some(task) = chat_task {
        task.abort();
    }
    Ok(())
}

async fn sleep_until(when: Option<tokio::time::Instant>) {
    match when {
        Some(when) => tokio::time::sleep_until(when).await,
        None => std::future::pending::<()>().await,
    }
}

fn tick_interval(app: &AppState) -> Duration {
    if app.status == crate::tui::state::Status::Thinking {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(1)
    }
}

async fn refresh_sidebar(client: &LetheClient, cmd_tx: &mpsc::Sender<AppCommand>) {
    let _ = cmd_tx.try_send(AppCommand::RefreshActors);
    let _ = cmd_tx.try_send(AppCommand::RefreshTodos);
    // Touch the client so we get an early "Connected" via events_stream.
    let _ = client.health().await;
}

use crate::tui::events::AppCommand;

fn make_editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray)),
    );
    editor.set_cursor_line_style(Style::default());
    editor.set_placeholder_text("Ask anything. /help for commands. @ to insert a workspace file.");
    editor
}

async fn handle_terminal_event(
    event: CtEvent,
    app: &mut AppState,
    editor: &mut TextArea<'_>,
    autocomplete: &Autocomplete,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> bool {
    match event {
        CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(key, app, editor, autocomplete, cmd_tx).await
        }
        CtEvent::Mouse(mouse) => {
            handle_mouse(mouse, app);
            false
        }
        CtEvent::Resize(_, _) => false,
        _ => false,
    }
}

/// Mouse wheel always scrolls the transcript; click-to-focus is left to
/// the keyboard so click drag-selects in the user's terminal still work
/// for copy/paste.
fn handle_mouse(mouse: MouseEvent, app: &mut AppState) {
    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_transcript(app, 3, true),
        MouseEventKind::ScrollDown => scroll_transcript(app, 3, false),
        _ => {}
    }
}

fn scroll_transcript(app: &mut AppState, lines: u16, up: bool) {
    if up {
        app.transcript_scroll = app.transcript_scroll.saturating_add(lines);
    } else {
        app.transcript_scroll = app.transcript_scroll.saturating_sub(lines);
    }
}

async fn handle_key(
    key: KeyEvent,
    app: &mut AppState,
    editor: &mut TextArea<'_>,
    autocomplete: &Autocomplete,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Global hotkeys (work regardless of focus).
    match key.code {
        KeyCode::Char('c') if ctrl => return true,
        KeyCode::Char('q') if ctrl => return true,
        KeyCode::Char('b') if ctrl => {
            app.toggle_sidebar();
            return false;
        }
        KeyCode::Tab if !ctrl => {
            app.cycle_focus();
            return false;
        }
        KeyCode::Esc => {
            if app.status == crate::tui::state::Status::Thinking {
                let _ = cmd_tx.send(AppCommand::Cancel).await;
                return false;
            }
        }
        // Scroll the transcript regardless of focus — these don't
        // overlap with anything in the editor or text pane.
        KeyCode::PageUp => {
            scroll_transcript(app, 10, true);
            return false;
        }
        KeyCode::PageDown => {
            scroll_transcript(app, 10, false);
            return false;
        }
        KeyCode::Up if ctrl => {
            scroll_transcript(app, 2, true);
            return false;
        }
        KeyCode::Down if ctrl => {
            scroll_transcript(app, 2, false);
            return false;
        }
        KeyCode::Home if ctrl => {
            app.transcript_scroll = u16::MAX;
            return false;
        }
        KeyCode::End if ctrl => {
            app.transcript_scroll = 0;
            return false;
        }
        _ => {}
    }

    if app.focused_pane != Pane::Editor {
        match key.code {
            KeyCode::Up => scroll_transcript(app, 2, true),
            KeyCode::Down => scroll_transcript(app, 2, false),
            KeyCode::Home => app.transcript_scroll = u16::MAX,
            KeyCode::End => app.transcript_scroll = 0,
            _ => {}
        }
        return false;
    }

    // Editor-focused keys.
    match key.code {
        KeyCode::Enter if shift => {
            editor.insert_newline();
            return false;
        }
        KeyCode::Enter => {
            let text = editor.lines().join("\n").trim().to_string();
            if text.is_empty() {
                return false;
            }
            clear_editor(editor);
            // Take effect of any tab-accepted completion already in `text`.
            if let Some(command) = parse_slash_command(&text) {
                handle_slash_command(command, app, cmd_tx).await;
            } else {
                let _ = cmd_tx.send(AppCommand::SendMessage(text)).await;
            }
            return false;
        }
        KeyCode::Tab => {
            if let Some(replacement) = active_completion(editor, autocomplete) {
                apply_completion(editor, replacement);
                return false;
            }
        }
        _ => {}
    }

    let input: Input = key.into();
    if input.key == Key::Null {
        return false;
    }
    editor.input(input);
    false
}

fn clear_editor(editor: &mut TextArea<'_>) {
    editor.select_all();
    editor.cut();
}

fn active_completion(
    editor: &TextArea<'_>,
    autocomplete: &Autocomplete,
) -> Option<CompletionApply> {
    let (row, col) = editor.cursor();
    let line = editor.lines().get(row).cloned().unwrap_or_default();
    let context = autocomplete.context_at(&line, col)?;
    let pick = context.matches.first()?.clone();
    Some(CompletionApply {
        row,
        start_col: context.start_col,
        cursor_col: col,
        replacement: pick,
    })
}

struct CompletionApply {
    row: usize,
    start_col: usize,
    cursor_col: usize,
    replacement: String,
}

fn apply_completion(editor: &mut TextArea<'_>, apply: CompletionApply) {
    let (cur_row, _) = editor.cursor();
    if cur_row != apply.row {
        return;
    }
    let line = editor
        .lines()
        .get(apply.row)
        .cloned()
        .unwrap_or_default();
    let before: String = line.chars().take(apply.start_col).collect();
    let after: String = line.chars().skip(apply.cursor_col).collect();
    let mut new_line = before;
    new_line.push('@');
    new_line.push_str(&apply.replacement);
    new_line.push(' ');
    let new_cursor = new_line.chars().count();
    new_line.push_str(&after);
    // Replace the current line in place by clearing and re-inserting.
    editor.move_cursor(tui_textarea::CursorMove::Head);
    for _ in 0..line.chars().count() {
        editor.delete_next_char();
    }
    editor.insert_str(&new_line);
    editor.move_cursor(tui_textarea::CursorMove::Head);
    for _ in 0..new_cursor {
        editor.move_cursor(tui_textarea::CursorMove::Forward);
    }
}

fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let args = parts.next().unwrap_or_default().trim().to_string();
    Some(match head {
        "help" | "?" => SlashCommand::Help,
        "clear" => SlashCommand::Clear,
        "cancel" => SlashCommand::Cancel,
        "todos" => SlashCommand::RefreshTodos,
        "actors" => SlashCommand::RefreshActors,
        "model" if !args.is_empty() => SlashCommand::Model(args),
        "model" => SlashCommand::ModelShow,
        "quit" | "exit" => SlashCommand::Quit,
        unknown => SlashCommand::Unknown(unknown.to_string()),
    })
}

enum SlashCommand {
    Help,
    Clear,
    Cancel,
    RefreshTodos,
    RefreshActors,
    ModelShow,
    Model(String),
    Quit,
    Unknown(String),
}

async fn handle_slash_command(
    command: SlashCommand,
    app: &mut AppState,
    cmd_tx: &mpsc::Sender<AppCommand>,
) {
    match command {
        SlashCommand::Help => {
            app.push_notice(
                "commands: /help · /clear · /cancel · /todos · /actors · /model [name] · /quit",
            );
            app.push_notice(
                "keys: Enter send · Shift+Enter newline · @ file complete · Tab cycle pane · Esc cancel turn",
            );
            app.push_notice(
                "scroll: PgUp/PgDn (10 lines) · Ctrl-Up/Down (2 lines) · Ctrl-Home/End (top/bottom) · mouse wheel",
            );
        }
        SlashCommand::Clear => {
            app.transcript.clear();
            app.tool_index.clear();
        }
        SlashCommand::Cancel => {
            let _ = cmd_tx.send(AppCommand::Cancel).await;
        }
        SlashCommand::RefreshTodos => {
            let _ = cmd_tx.send(AppCommand::RefreshTodos).await;
        }
        SlashCommand::RefreshActors => {
            let _ = cmd_tx.send(AppCommand::RefreshActors).await;
        }
        SlashCommand::ModelShow => {
            app.push_notice(format!("model: {} / {}", app.provider, app.model));
        }
        SlashCommand::Model(name) => {
            let _ = cmd_tx.send(AppCommand::SwitchModel(name)).await;
        }
        SlashCommand::Quit => {
            let _ = cmd_tx.send(AppCommand::Quit).await;
        }
        SlashCommand::Unknown(name) => {
            app.push_notice(format!("unknown command: /{name}"));
        }
    }
}

fn draw_autocomplete_popup(
    frame: &mut ratatui::Frame<'_>,
    editor: &TextArea<'_>,
    autocomplete: &Autocomplete,
) {
    let (row, col) = editor.cursor();
    let line = editor.lines().get(row).cloned().unwrap_or_default();
    let Some(context) = autocomplete.context_at(&line, col) else {
        return;
    };
    if context.matches.is_empty() {
        return;
    }
    let area = frame.area();
    let height = (context.matches.len() as u16 + 2).min(10);
    let popup_width = context
        .matches
        .iter()
        .map(|path| path.chars().count() as u16)
        .max()
        .unwrap_or(20)
        .min(80)
        + 4;
    let popup = Rect {
        x: area.width.saturating_sub(popup_width).saturating_sub(2),
        y: area.height.saturating_sub(height).saturating_sub(4),
        width: popup_width,
        height,
    };
    frame.render_widget(Clear, popup);
    let lines: Vec<Line<'static>> = context
        .matches
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let style = if index == 0 {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(path.clone(), style))
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                format!(" @{} ", context.query),
                Style::default().fg(Color::Cyan),
            )),
    );
    frame.render_widget(paragraph, popup);
}
