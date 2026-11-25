//! The top-right pane: what the selected operation will send.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use testdet_core::operation::model::Request;

use crate::app::App;

/// Draw the request summary for the selection at `area`.
pub(crate) fn draw(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::bordered().title("request");
    let Some(id) = app.selection() else {
        frame.render_widget(Paragraph::new("no operation selected").block(block), area);
        return;
    };
    let Some(operation) = app.config().operations.get(id) else {
        frame.render_widget(Paragraph::new("unknown operation").block(block), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    match &operation.request {
        Request::Graphql(graphql) => {
            lines.push(Line::from(vec![
                Span::styled("kind: ", Style::default().fg(Color::DarkGray)),
                Span::raw("graphql"),
            ]));
            let path = graphql
                .path
                .clone()
                .unwrap_or_else(|| app.resolved().graphql_path.clone());
            lines.push(Line::from(format!("POST {path}")));
            lines.push(Line::from(""));
            for query_line in graphql.query.trim().lines() {
                lines.push(Line::from(format!("  {query_line}")));
            }
        }
        Request::Http(http) => {
            lines.push(Line::from(vec![
                Span::styled("kind: ", Style::default().fg(Color::DarkGray)),
                Span::raw("http"),
            ]));
            lines.push(Line::from(format!(
                "{} {}",
                http.method.to_uppercase(),
                http.path
            )));
        }
    }

    let headers = app.resolved().headers_for(operation, app.user());
    let mut names: Vec<&String> = headers.0.keys().collect();
    names.sort();
    let mut header_lines: Vec<Line> = Vec::new();
    for name in names {
        // Never a value: `Redactor` owns censoring, so the pane shows the
        // name twice and lets the reader know the value exists.
        header_lines.push(masked_header(name));
    }

    let mut text = lines;
    if !header_lines.is_empty() {
        text.push(Line::from(""));
        text.push(Line::from(Span::styled(
            "headers",
            Style::default().fg(Color::DarkGray),
        )));
        text.extend(header_lines);
    }

    frame.render_widget(Paragraph::new(Text::from(text)).block(block), area);
}

fn masked_header<'a>(name: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {name}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{name}-CENSORED")),
    ])
}
