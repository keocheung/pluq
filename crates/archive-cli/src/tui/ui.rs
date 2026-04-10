use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
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
                Constraint::Length(1), // breadcrumb header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // file list
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer / details bar
            ])
            .split(area);

            // Build breadcrumb: " archive.zip / photos / vacation /"
            let filename = app
                .source
                .rsplit('/')
                .next()
                .or_else(|| app.source.rsplit('\\').next())
                .unwrap_or(&app.source);
            let filename = filename.split('?').next().unwrap_or(filename);

            let mut breadcrumb = format!(" {}", filename);
            for segment in &app.current_path {
                breadcrumb.push_str(" / ");
                breadcrumb.push_str(segment);
            }
            if !app.current_path.is_empty() {
                breadcrumb.push_str(" /");
            }

            let header_text = if app.is_filtering {
                format!("{}  [Filter: \"{}-\"]", breadcrumb, app.filter_query)
            } else if !app.filter_query.is_empty() {
                format!("{}  [Filter: \"{}\"]", breadcrumb, app.filter_query)
            } else {
                breadcrumb
            };

            let header = Paragraph::new(header_text)
                .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
            frame.render_widget(header, chunks[0]);

            let sep = "─".repeat(area.width as usize);
            let separator_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(
                Paragraph::new(sep.clone()).style(separator_style),
                chunks[1],
            );
            frame.render_widget(
                Paragraph::new(sep).style(separator_style),
                chunks[3],
            );

            let rows: Vec<Row> = app
                .visible_items
                .iter()
                .map(|item| {
                    let is_toggled = app.toggled.contains(&item.full_path);
                    let marker = if is_toggled { "+" } else { " " };

                    let size_str = if item.is_directory {
                        String::new()
                    } else {
                        crate::format_size(item.uncompressed_size)
                    };

                    let display_name = if item.is_directory {
                        format!("{}/", item.name)
                    } else {
                        item.name.clone()
                    };

                    let name_style = if item.is_directory {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    Row::new([
                        Cell::from(format!(" {}", size_str))
                            .style(Style::default().fg(Color::DarkGray)),
                        Cell::from(format!("{} {}", marker, display_name)).style(name_style),
                    ])
                })
                .collect();

            let highlight_style = Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);

            let table = Table::new(
                rows,
                [Constraint::Length(10), Constraint::Min(1)],
            )
            .row_highlight_style(highlight_style);

            frame.render_stateful_widget(table, chunks[2], &mut app.table_state);

            draw_footer(frame, app, chunks[4]);
        }
        AppPhase::ConfirmExtract {
            entry_name,
            dict_size,
            ..
        } => {
            let (entry_name, dict_size) = (entry_name.clone(), *dict_size);

            let outer = Block::default()
                .borders(Borders::ALL)
                .title("Large Dictionary Warning")
                .border_style(Style::default().fg(Color::Yellow));
            let inner = outer.inner(area);
            frame.render_widget(outer, area);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // entry name
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // warning line 1
                    Constraint::Length(1), // warning line 2
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // dict size
                    Constraint::Min(1),    // fill
                    Constraint::Length(1), // action hints
                ])
                .split(inner);

            frame.render_widget(
                Paragraph::new(format!("  {}", entry_name)),
                chunks[1],
            );
            frame.render_widget(
                Paragraph::new("  This file uses a large compression dictionary.")
                    .style(Style::default().fg(Color::Yellow)),
                chunks[3],
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "  The decompressor will allocate ~{} of RAM.",
                    crate::format_size(dict_size)
                ))
                .style(Style::default().fg(Color::Yellow)),
                chunks[4],
            );
            frame.render_widget(
                Paragraph::new(format!("  Dictionary size: {}", crate::format_size(dict_size))),
                chunks[6],
            );
            frame.render_widget(
                Paragraph::new("  Enter to proceed  ·  Esc to cancel"),
                chunks[8],
            );
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
            frame.render_widget(Paragraph::new("  Esc to cancel  ·  q to quit"), chunks[7]);
        }
        AppPhase::ExtractingBatch {
            total_files,
            files_done,
            current_entry,
            total_bytes,
            bytes_extracted,
        } => {
            let (total_files, files_done, total_bytes, bytes_extracted) =
                (*total_files, *files_done, *total_bytes, *bytes_extracted);
            let current_entry = current_entry.clone();

            let title = format!("Extracting ({} of {} files)", files_done, total_files);
            let outer = Block::default().borders(Borders::ALL).title(title);
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
                    Constraint::Length(1), // current entry name
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // byte counter
                    Constraint::Min(1),    // fill
                    Constraint::Length(1), // cancel hint
                ])
                .split(inner);

            let entry_label = if current_entry.is_empty() {
                "  Preparing...".to_string()
            } else {
                format!("  {}", current_entry)
            };
            frame.render_widget(Paragraph::new(entry_label), chunks[1]);

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio);
            frame.render_widget(gauge, chunks[3]);

            let counter_text = if total_bytes > 0 {
                format!(
                    "  Extracted:  {} / {}",
                    crate::format_size(bytes_extracted),
                    crate::format_size(total_bytes),
                )
            } else {
                format!("  Extracted:  {}", crate::format_size(bytes_extracted))
            };
            frame.render_widget(Paragraph::new(counter_text), chunks[5]);
            frame.render_widget(Paragraph::new("  Esc to cancel  ·  q to quit"), chunks[7]);
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

    if app.show_help {
        draw_help_popup(frame, area);
    }
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let text = if let Some(msg) = &app.status_message {
        format!(" {}", msg)
    } else if !app.toggled.is_empty() {
        let n = app.toggled.len();
        let total_size: u64 = app
            .entries
            .iter()
            .filter(|e| app.toggled.contains(&e.name))
            .map(|e| e.uncompressed_size)
            .sum();

        let total_cost: u64 = app
            .entries
            .iter()
            .filter(|e| app.toggled.contains(&e.name))
            .map(|e| {
                app.archive
                    .as_ref()
                    .map(|a| a.extraction_cost(e))
                    .unwrap_or(0)
            })
            .sum();

        format!(
            " [ {} file{} — {} · Cost: {} ]  Enter extract  q quit",
            n,
            if n == 1 { "" } else { "s" },
            crate::format_size(total_size),
            crate::format_size(total_cost),
        )
    } else if let Some(item) = app.visible_items.get(app.selected) {
        if item.is_directory {
            format!(" {}/", item.full_path)
        } else {
            let cost = if let Some(idx) = item.entry_index {
                app.archive
                    .as_ref()
                    .map(|a| a.extraction_cost(&app.entries[idx]))
                    .unwrap_or(0)
            } else {
                0
            };

            format!(
                " {}  {}  Cost: {}",
                item.full_path,
                crate::format_size(item.uncompressed_size),
                crate::format_size(cost)
            )
        }
    } else {
        String::new()
    };

    let footer = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

fn draw_help_popup(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let help_text = "
 l / Right: Dive in
 h / Left:  Back out
 Space:     Toggle selection
 a:         Toggle all in folder
 Enter:     Extract selection
 /:         Filter items
 ?:         Toggle this help
 q:         Quit
";

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(help_text).block(block);

    // Center the popup.
    let popup_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(12),
            Constraint::Min(0),
        ])
        .split(area)[1];

    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(30),
            Constraint::Min(0),
        ])
        .split(popup_area)[1];

    frame.render_widget(Clear, popup_area);
    frame.render_widget(paragraph, popup_area);
}
