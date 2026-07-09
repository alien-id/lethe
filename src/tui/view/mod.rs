//! Renderer for the TUI. One `draw(frame, app, editor)` entry point;
//! layout decisions live here, content rendering in the submodules.

pub mod editor;
pub mod footer;
pub mod sidebar;
pub mod transcript;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_textarea::TextArea;

use crate::tui::state::{AppState, Pane};

pub fn draw(frame: &mut Frame<'_>, app: &mut AppState, editor: &TextArea<'_>) {
    let size = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(editor_height(editor, size.width)),
            Constraint::Length(1),
        ])
        .split(size);

    let main_area = outer[0];
    let editor_area = outer[1];
    let footer_area = outer[2];

    if app.sidebar_visible {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(38)])
            .split(main_area);
        transcript::draw(frame, split[0], app);
        sidebar::draw(frame, split[1], app);
    } else {
        transcript::draw(frame, main_area, app);
    }

    editor::draw(frame, editor_area, app, editor);
    footer::draw(frame, footer_area, app);
    draw_activity_detail(frame, size, app);
}

/// Centered popup with the selected activity's "what I did / what I found".
/// Opened with Enter on an activity row; closed with Esc/Enter.
fn draw_activity_detail(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(row) = app.activity_detail.and_then(|index| app.activity.get(index)) else {
        return;
    };
    let width = area.width.saturating_sub(8).min(80).max(30);
    let height = area.height.saturating_sub(6).min(20).max(8);
    let popup = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    let kind_label = if row.kind == "insight" {
        ("✦ insight", Color::Green)
    } else {
        ("▸ activity", Color::Blue)
    };
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(
                kind_label.0.to_string(),
                Style::default().fg(kind_label.1).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} · {}", row.source_name, row.produced_at),
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::default(),
        Line::from(Span::styled(
            row.summary.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    if let Some(detail) = row
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && *detail != row.summary)
    {
        lines.push(Line::default());
        for text_line in detail.lines() {
            lines.push(Line::from(Span::raw(text_line.to_string())));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Esc/Enter to close",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Clear, popup);
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green))
                .title(Span::styled(
                    format!(" {} ", row.title),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
        );
    frame.render_widget(paragraph, popup);
}

fn editor_height(editor: &TextArea<'_>, width: u16) -> u16 {
    let body_width = width.saturating_sub(4).max(20) as usize;
    let mut lines: u16 = 0;
    for line in editor.lines() {
        let len = line.chars().count();
        let visible = if line.is_empty() {
            1
        } else {
            (len + body_width - 1) / body_width
        };
        lines = lines.saturating_add(visible.max(1) as u16);
    }
    lines.clamp(3, 10) + 2
}

pub fn focus_border(app: &AppState, pane: Pane) -> Block<'_> {
    let style = if app.focused_pane == pane {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default().borders(Borders::ALL).border_style(style)
}

pub fn inner_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}
