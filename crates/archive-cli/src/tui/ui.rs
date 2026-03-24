use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
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
        AppPhase::Scanning { .. } => {
            let para = Paragraph::new("Scanning archive...")
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Archive Sniper"));
            frame.render_widget(para, area);
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
