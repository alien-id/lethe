//! Right sidebar with Actors, Activity, and Todos panels. Each panel is a
//! vertical list; focus dictates the border color.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::state::{ActivityUiRow, ActorRow, AppState, Pane, TodoRow};
use crate::tui::view::{focus_border, inner_area};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(area);

    draw_actors(frame, layout[0], app);
    draw_activity(frame, layout[1], app);
    draw_todos(frame, layout[2], app);
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

fn draw_activity(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    // The green dot: unseen-insight count in the pane title, hidden at 0.
    let mut title_spans = vec![Span::styled(
        format!(" activity ({}) ", app.activity.len()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if app.unseen_insights > 0 {
        title_spans.push(Span::styled(
            format!("●{} ", app.unseen_insights),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let block = focus_border(app, Pane::Activity).title(Line::from(title_spans));
    let inner = inner_area(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.activity.is_empty() {
        lines.push(Line::from(Span::styled(
            "no background activity yet",
            Style::default().fg(Color::Gray),
        )));
    } else {
        let focused = app.focused_pane == Pane::Activity;
        // Keep the selection visible in the pane's viewport.
        let visible = inner.height as usize;
        let start = if visible == 0 {
            0
        } else {
            app.activity_selected.saturating_sub(visible.saturating_sub(1))
        };
        let mut last_day: Option<String> = None;
        for (index, row) in app.activity.iter().enumerate().skip(start) {
            // Group by day: a dim date line whenever the day changes
            // (rows are newest-first, so days appear in reverse order).
            let day: String = row.produced_at.chars().take(10).collect();
            if last_day.as_deref() != Some(day.as_str()) && !day.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("— {day}"),
                    Style::default().fg(Color::DarkGray),
                )));
                last_day = Some(day);
            }
            lines.push(activity_line(row, focused && index == app.activity_selected));
        }
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn activity_line(row: &ActivityUiRow, selected: bool) -> Line<'static> {
    // Insight vs completed-task icon; unseen rows get the green dot + bold.
    let icon = if row.kind == "insight" { "✦" } else { "▸" };
    let icon_color = if row.kind == "insight" {
        Color::Green
    } else {
        Color::Blue
    };
    let mut title_style = Style::default();
    if !row.seen {
        title_style = title_style.add_modifier(Modifier::BOLD);
    }
    if selected {
        title_style = title_style.bg(Color::DarkGray);
    }
    let time: String = row
        .produced_at
        .chars()
        .skip(11)
        .take(5)
        .collect::<String>();
    let mut spans = vec![
        Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
        Span::styled(row.title.clone(), title_style),
    ];
    if !row.seen {
        spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
    }
    if !time.is_empty() {
        spans.push(Span::styled(
            format!(" {time}"),
            Style::default().fg(Color::Gray),
        ));
    }
    Line::from(spans)
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
