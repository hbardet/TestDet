//! The left column: the environment and user banner, the operation tree and
//! the filter prompt.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::app::App;

/// Draw the whole left column at `area`.
pub(crate) fn draw(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let [banner, operations, prompt] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(area);

    draw_banner(app, frame, banner);
    draw_operations(app, frame, operations);
    draw_filter(app, frame, prompt);
}

/// `name · environment`, `user: …`, and the last status line.
fn draw_banner(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let name = app
        .config()
        .manifest
        .name
        .clone()
        .unwrap_or_else(|| "testdet".to_string());
    let user = app.user().unwrap_or("anonymous");
    let status = app.status().unwrap_or_default();
    let text = Text::from(vec![
        Line::from(format!("{name} · {}", app.environment())),
        Line::from(format!("user: {user}")),
        Line::from(Span::styled(
            status.to_string(),
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title("testdet")),
        area,
    );
}

/// Group headers plus one row per operation, tags dimmed after the id.
fn draw_operations(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row = None;

    for group in &app.groups {
        let mut header_added = false;
        for id in &group.operations {
            if !app.visible.contains(id) {
                continue;
            }
            if !header_added {
                let label = if group.name.is_empty() {
                    "(root)".to_string()
                } else {
                    group.name.clone()
                };
                items.push(ListItem::new(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))));
                header_added = true;
            }
            if app.selection() == Some(id.as_str()) {
                selected_row = Some(items.len());
            }
            items.push(ListItem::new(operation_row(app, id)));
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            "no matches",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let list = List::new(items)
        .block(Block::bordered().title("operations"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
}

/// One operation row: indented id, then its tags in dim grey.
fn operation_row<'a>(app: &App, id: &'a str) -> Line<'a> {
    let tags = app
        .config()
        .operations
        .get(id)
        .map(|operation| operation.tags.join(","))
        .unwrap_or_default();
    let mut spans = vec![Span::raw(format!("  {id}"))];
    if !tags.is_empty() {
        spans.push(Span::styled(
            format!(" [{tags}]"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// The filter prompt at the bottom of the column.
fn draw_filter(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let text = if app.filtering() || !app.filter().is_empty() {
        format!("/{}", app.filter())
    } else {
        "[/] filter".to_string()
    };
    frame.render_widget(Paragraph::new(text).block(Block::bordered()), area);
}
