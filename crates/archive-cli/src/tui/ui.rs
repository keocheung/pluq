use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table},
};

use super::app::{App, AppPhase};

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
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
            let (total_bytes, bytes_scanned, entries_found) =
                (*total_bytes, *bytes_scanned, *entries_found);

            let outer = Block::default().borders(Borders::ALL).title("Archive Sniper");
            let inner = outer.inner(area);
            frame.render_widget(outer, area);

            let ratio = if total_bytes > 0 {
                (bytes_scanned as f64 / total_bytes as f64).clamp(0.0, 1.0)
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

            let scanned_text = if total_bytes > 0 {
                format!(
                    "  Scanned:  {} / {}",
                    crate::format_size(bytes_scanned),
                    crate::format_size(total_bytes)
                )
            } else {
                format!("  Scanned:  {}", crate::format_size(bytes_scanned))
            };
            frame.render_widget(Paragraph::new(scanned_text), chunks[5]);

            let found_text = format!(
                "  Found:    {} file{}",
                entries_found,
                if entries_found == 1 { "" } else { "s" }
            );
            frame.render_widget(Paragraph::new(found_text), chunks[6]);

            frame.render_widget(Paragraph::new("  Press q to cancel"), chunks[8]);
        }
        AppPhase::Browsing => {
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(area);

            // --- Header ---
            let filename = app
                .source
                .rsplit('/')
                .next()
                .or_else(|| app.source.rsplit('\\').next())
                .unwrap_or(&app.source);
            let filename = filename.split('?').next().unwrap_or(filename);
            let file_count = app.entries.iter().filter(|e| !e.is_directory).count();
            let total_size: u64 = app.entries.iter().map(|e| e.uncompressed_size).sum();
            let header_title = format!(
                " {} — {} files — {} ",
                filename,
                file_count,
                crate::format_size(total_size)
            );
            frame.render_widget(
                Block::default().borders(Borders::ALL).title(header_title),
                chunks[0],
            );

            // --- Entry Table ---
            let col_header = Row::new(["SIZE", "COMPRESSED", "NAME"])
                .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));

            let selected = app.selected;
            let rows: Vec<Row> = app
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let marker = if i == selected { ">" } else { " " };
                    if entry.is_directory {
                        Row::new([
                            Cell::from(format!("{} -", marker)),
                            Cell::from("-"),
                            Cell::from(format!("{}/", entry.name)),
                        ])
                    } else {
                        Row::new([
                            Cell::from(format!(
                                "{} {}",
                                marker,
                                crate::format_size(entry.uncompressed_size)
                            )),
                            Cell::from(crate::format_size(entry.compressed_size)),
                            Cell::from(entry.name.clone()),
                        ])
                    }
                })
                .collect();

            let highlight_style = Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);

            let table = Table::new(
                rows,
                [
                    Constraint::Length(14),
                    Constraint::Length(13),
                    Constraint::Min(1),
                ],
            )
            .header(col_header)
            .row_highlight_style(highlight_style)
            .block(Block::default().borders(Borders::ALL));

            frame.render_stateful_widget(table, chunks[1], &mut app.table_state);

            // --- Footer ---
            let footer_text = match &app.status_message {
                Some(msg) => msg.clone(),
                None => "  up/down navigate  Enter extract  q quit".to_string(),
            };
            let footer = Paragraph::new(footer_text)
                .block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[2]);
        }
        AppPhase::Extracting {
            entry_name,
            total_bytes,
            bytes_extracted,
        } => {
            let (entry_name, total_bytes, bytes_extracted) =
                (entry_name.clone(), *total_bytes, *bytes_extracted);

            let outer = Block::default().borders(Borders::ALL).title("Extracting");
            let inner = outer.inner(area);
            frame.render_widget(outer, area);

            let ratio = if total_bytes > 0 {
                (bytes_extracted as f64 / total_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // entry name
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // byte counter
                    Constraint::Min(1),    // fill
                    Constraint::Length(1), // cancel hint
                ])
                .split(inner);

            frame.render_widget(Paragraph::new(format!("  {}", entry_name)), chunks[1]);

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio);
            frame.render_widget(gauge, chunks[3]);

            let counter_text = if total_bytes > 0 {
                format!(
                    "  Extracted:  {} / {}",
                    crate::format_size(bytes_extracted),
                    crate::format_size(total_bytes)
                )
            } else {
                format!("  Extracted:  {}", crate::format_size(bytes_extracted))
            };
            frame.render_widget(Paragraph::new(counter_text), chunks[5]);
            frame.render_widget(Paragraph::new("  Press q to cancel"), chunks[7]);
        }
        AppPhase::Error(msg) => {
            let msg = msg.clone();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);

            let para = Paragraph::new(msg)
                .style(Style::default().fg(Color::Red))
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Error"));
            frame.render_widget(para, chunks[0]);

            let hint = Paragraph::new("Press q to quit");
            frame.render_widget(hint, chunks[1]);
        }
    }
}
