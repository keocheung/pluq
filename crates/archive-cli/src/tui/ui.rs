use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph},
};

use super::app::{App, AppPhase};

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    match &app.phase {
        AppPhase::Opening => {
            let text = format!("Opening {}...", app.source);
            let para = Paragraph::new(text)
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Archive Sniper"));
            frame.render_widget(para, area);
        }
        AppPhase::Scanning {
            total_bytes,
            bytes_scanned,
            entries_found,
        } => {
            let outer = Block::default().borders(Borders::ALL).title("Archive Sniper");
            let inner = outer.inner(area);
            frame.render_widget(outer, area);

            let ratio = if *total_bytes > 0 {
                (*bytes_scanned as f64 / *total_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // "Scanning archive..."
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // scanned counter
                    Constraint::Length(1), // found counter
                    Constraint::Min(1),    // fill
                    Constraint::Length(1), // cancel hint
                ])
                .split(inner);

            let title = Paragraph::new("Scanning archive...").alignment(Alignment::Center);
            frame.render_widget(title, chunks[1]);

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Green))
                .ratio(ratio);
            frame.render_widget(gauge, chunks[3]);

            let scanned_text = if *total_bytes > 0 {
                format!(
                    "  Scanned:  {} / {}",
                    crate::format_size(*bytes_scanned),
                    crate::format_size(*total_bytes)
                )
            } else {
                format!("  Scanned:  {}", crate::format_size(*bytes_scanned))
            };
            frame.render_widget(Paragraph::new(scanned_text), chunks[5]);

            let found_text = format!(
                "  Found:    {} file{}",
                entries_found,
                if *entries_found == 1 { "" } else { "s" }
            );
            frame.render_widget(Paragraph::new(found_text), chunks[6]);

            frame.render_widget(
                Paragraph::new("  Press q to cancel"),
                chunks[8],
            );
        }
        AppPhase::Browsing => {
            let items: Vec<ListItem> = app
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let marker = if i == app.selected { "> " } else { "  " };
                    let name = if entry.is_directory {
                        format!("{}{}/", marker, entry.name)
                    } else {
                        format!("{}{}", marker, entry.name)
                    };
                    ListItem::new(name)
                })
                .collect();

            let mut list_state = ListState::default();
            list_state.select(Some(app.selected));

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title("Archive Sniper"))
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));

            frame.render_stateful_widget(list, chunks[0], &mut list_state);

            let footer_text = match &app.status_message {
                Some(msg) => msg.clone(),
                None => "up/down navigate  q quit".to_string(),
            };
            let footer = Paragraph::new(footer_text);
            frame.render_widget(footer, chunks[1]);
        }
        AppPhase::Extracting { entry_name, .. } => {
            let text = format!("Extracting {}...", entry_name);
            let para = Paragraph::new(text)
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Extracting"));
            frame.render_widget(para, area);
        }
        AppPhase::Error(msg) => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);

            let para = Paragraph::new(msg.clone())
                .style(Style::default().fg(Color::Red))
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Error"));
            frame.render_widget(para, chunks[0]);

            let hint = Paragraph::new("Press q to quit");
            frame.render_widget(hint, chunks[1]);
        }
    }
}
