//! Transcript pane: user messages, assistant markdown, and inline tool
//! cards. Scroll position lives in `AppState`. Renders bottom-aligned so
//! freshest content is always visible.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::markdown;
use crate::tui::state::{AppState, Pane, Status, ToolCall, ToolStatus, TranscriptItem};
use crate::tui::view::{focus_border, inner_area};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    let block = focus_border(app, Pane::Transcript).title(Span::styled(
        " transcript ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = inner_area(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &app.transcript {
        render_item(item, inner.width, &mut lines);
    }
    if app.status == Status::Thinking {
        lines.push(thinking_line(app.tick));
    }

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    // Wrapped line count, not raw `lines.len()` — a 200-char paragraph
    // in an 80-col pane occupies 3 visual rows but is one entry in
    // `lines`. Without this the scroll calc undercounted by however
    // many lines wrapped, and the transcript appeared to "not scroll"
    // when in fact it just hadn't realised it had overflowed.
    let total = paragraph.line_count(inner.width) as u16;
    let visible = inner.height;
    let max_scroll = total.saturating_sub(visible);
    // Clamp the user's requested scroll so they can't scroll past the
    // top of the buffer.
    app.transcript_scroll = app.transcript_scroll.min(max_scroll);
    let scroll = max_scroll.saturating_sub(app.transcript_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), inner);
}

fn thinking_line(tick: u64) -> Line<'static> {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let frame = FRAMES[(tick as usize) % FRAMES.len()];
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            frame.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("thinking…", Style::default().fg(Color::Yellow)),
    ])
}

fn render_item(item: &TranscriptItem, width: u16, lines: &mut Vec<Line<'static>>) {
    match item {
        TranscriptItem::User { content, .. } => {
            let label = Span::styled(
                "user ▸ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
            push_prefixed(
                lines,
                label,
                content
                    .lines()
                    .map(|line| Line::from(Span::raw(line.to_string()))),
            );
        }
        TranscriptItem::Assistant { content, .. } => {
            let label = Span::styled(
                "lethe ▸ ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            );
            let rendered = markdown::render(content, width.saturating_sub(8));
            push_prefixed(lines, label, rendered.lines.into_iter());
        }
        TranscriptItem::Tool(call) => render_tool(call, lines),
        TranscriptItem::Notice { content, .. } => {
            lines.push(Line::from(Span::styled(
                format!("· {content}"),
                Style::default().fg(Color::Gray),
            )));
        }
    }
}

/// Push a sequence of body lines under a single role label. The label
/// rides on the first body line; continuations get an indent equal to the
/// label's printable width so paragraphs align under the body text.
fn push_prefixed<I>(lines: &mut Vec<Line<'static>>, label: Span<'static>, body: I)
where
    I: Iterator<Item = Line<'static>>,
{
    let indent: String = " ".repeat(label.content.chars().count());
    let mut first = true;
    let mut emitted = false;
    for body_line in body {
        emitted = true;
        if first {
            first = false;
            let mut spans = vec![label.clone()];
            spans.extend(body_line.spans);
            lines.push(Line::from(spans));
        } else {
            let mut spans = vec![Span::raw(indent.clone())];
            spans.extend(body_line.spans);
            lines.push(Line::from(spans));
        }
    }
    if !emitted {
        // Empty body — still render the label so the message isn't
        // invisible (mostly happens for in-flight streaming bubbles
        // before the first delta lands).
        lines.push(Line::from(label));
    }
}

fn render_tool(call: &ToolCall, lines: &mut Vec<Line<'static>>) {
    let (marker, marker_style) = match call.status {
        ToolStatus::Running => (
            "▸",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        ToolStatus::Done { success: true, .. } => (
            "✓",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ToolStatus::Done { success: false, .. } => (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let trailer = match call.status {
        ToolStatus::Done { duration_ms, .. } => format!(" {}ms", duration_ms),
        ToolStatus::Running => String::new(),
    };
    let mut spans = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
        Span::styled(
            call.name.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !call.args_preview.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            call.args_preview.clone(),
            Style::default().fg(Color::Gray),
        ));
    }
    if !trailer.is_empty() {
        spans.push(Span::styled(trailer, Style::default().fg(Color::Gray)));
    }
    lines.push(Line::from(spans));

    if !call.output_preview.is_empty() {
        for line in call.output_preview.lines().take(2) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
            ]));
        }
    }
}

#[allow(dead_code)]
pub fn min_block() -> Block<'static> {
    Block::default().borders(Borders::ALL)
}
