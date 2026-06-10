//! Right sidebar with Actors and Todos panels. Each panel is a vertical
//! list; focus dictates the border color.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::state::{ActorRow, AppState, Pane, TodoRow};
use crate::tui::view::{focus_border, inner_area};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    draw_actors(frame, layout[0], app);
    draw_todos(frame, layout[1], app);
}

fn draw_actors(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let block = focus_border(app, Pane::Actors).title(Span::styled(
        format!(" actors ({}) ", app.actors.len()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = inner_area(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.actors.is_empty() {
        lines.push(Line::from(Span::styled(
            "no actors",
            Style::default().fg(Color::Gray),
        )));
    } else {
        // Group parents first, then children under them.
        let roots: Vec<&ActorRow> = app
            .actors
            .iter()
            .filter(|actor| actor.spawned_by.is_empty())
            .collect();
        for root in &roots {
            lines.push(actor_line(root, false));
            for child in app
                .actors
                .iter()
                .filter(|actor| actor.spawned_by == root.id)
            {
                lines.push(actor_line(child, true));
            }
        }
        let orphans: Vec<&ActorRow> = app
            .actors
            .iter()
            .filter(|actor| {
                !actor.spawned_by.is_empty()
                    && !roots.iter().any(|root| root.id == actor.spawned_by)
            })
            .collect();
        for orphan in orphans {
            lines.push(actor_line(orphan, true));
        }
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn actor_line(actor: &ActorRow, indent: bool) -> Line<'static> {
    let prefix = if indent { "  ↳ " } else { "▸ " };
    let state_color = match actor.state.as_str() {
        "running" => Color::Yellow,
        "waiting" => Color::Blue,
        "terminated" => match actor.outcome.as_deref() {
            Some("success") => Color::Green,
            Some("failure" | "killed" | "max_turns") => Color::Red,
            _ => Color::Gray,
        },
        _ => Color::Gray,
    };
    let badge = format!(
        "[{}{}]",
        actor.state,
        actor
            .outcome
            .as_deref()
            .map(|outcome| format!("/{outcome}"))
            .unwrap_or_default()
    );
    Line::from(vec![
        Span::raw(prefix.to_string()),
        Span::styled(
            actor.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(badge, Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(short_id(&actor.id), Style::default().fg(Color::Gray)),
    ])
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn draw_todos(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let block = focus_border(app, Pane::Todos).title(Span::styled(
        format!(" todos ({}) ", app.todos.len()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = inner_area(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "no todos",
            Style::default().fg(Color::Gray),
        )));
    } else {
        for todo in &app.todos {
            lines.push(todo_line(todo));
        }
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn todo_line(todo: &TodoRow) -> Line<'static> {
    let (marker, marker_style) = match todo.status.as_str() {
        "completed" => ("▣", Style::default().fg(Color::Green)),
        "cancelled" => (
            "▢",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        _ => ("▢", Style::default().fg(Color::Yellow)),
    };
    let priority_style = match todo.priority.as_str() {
        "high" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "low" => Style::default().fg(Color::Gray),
        _ => Style::default().fg(Color::Gray),
    };
    let due = todo
        .due_date
        .as_deref()
        .map(|due| format!(" @{}", &due[..due.len().min(10)]))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
        Span::styled(format!("{:>4} ", todo.priority), priority_style),
        Span::raw(todo.title.clone()),
        Span::styled(due, Style::default().fg(Color::Gray)),
    ])
}
