//! Footer line: status indicator, model name, token usage, hotkey hints.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::state::{AppState, Status};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let (status_label, status_style) = match app.status {
        Status::Idle => (
            "● idle",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Status::Thinking => (
            "◐ thinking",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Status::Disconnected => (
            "○ offline",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };

    let tokens = match app.prompt_tokens {
        Some(value) => format!("{}/{}k", format_tokens(value), app.max_context / 1000,),
        None => "—".to_string(),
    };

    let model = if app.provider.is_empty() {
        app.model.clone()
    } else {
        format!("{} · {}", app.provider, app.model)
    };

    let mut spans = vec![
        Span::styled(status_label.to_string(), status_style),
        Span::raw("  "),
        Span::styled(model, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(tokens, Style::default().fg(Color::Gray)),
    ];
    if let Some(message) = &app.status_message {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            message.clone(),
            Style::default().fg(Color::Gray),
        ));
    }
    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        "PgUp/PgDn scroll · Ctrl-Home/End jump · Tab pane · Ctrl-B sidebar · Ctrl-C quit · /help",
        Style::default().fg(Color::Gray),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn format_tokens(value: u64) -> String {
    if value >= 1000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        value.to_string()
    }
}
