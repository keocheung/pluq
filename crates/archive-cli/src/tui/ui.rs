use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
};

use super::app::{App, AppPhase, HomeFocus};
use crate::home::complete::looks_like_url;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();

    if let AppPhase::AuthPrompt { provider, .. } = &app.phase {
        let p = *provider;
        draw_home(frame, app, area);
        let buffer = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buffer[(x, y)];
                cell.set_style(cell.style().fg(Color::DarkGray));
            }
        }
        draw_auth_prompt(frame, area, &p);

        if app.show_help {
            draw_help_popup(frame, area);
        }
        return;
    }

    match &app.phase {
        AppPhase::Home => {
            draw_home(frame, app, area);
        }
        AppPhase::Opening => {
            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let header = Paragraph::new(" Opening archive...").style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let content_chunks = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(chunks[2]);

            let source_text = Paragraph::new(format!(" {}", app.source))
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center);
            frame.render_widget(source_text, content_chunks[1]);

            // Footer
            let footer =
                Paragraph::new(" Please wait...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::Scanning {
            total_bytes,
            bytes_scanned,
            entries_found,
        } => {
            let (total_bytes, bytes_scanned, entries_found) =
                (*total_bytes, *bytes_scanned, *entries_found);

            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let header = Paragraph::new(" Scanning archive...").style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let ratio = if total_bytes > 0 {
                (bytes_scanned as f64 / total_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let content_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // scanned counter
                    Constraint::Length(1), // found counter
                    Constraint::Min(1),    // fill
                ])
                .split(chunks[2]);

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio);
            frame.render_widget(gauge, content_chunks[2]);

            let scanned_text = if total_bytes > 0 {
                format!(
                    "  Scanned:  {} / {}",
                    crate::format_size(bytes_scanned),
                    crate::format_size(total_bytes)
                )
            } else {
                format!("  Scanned:  {}", crate::format_size(bytes_scanned))
            };
            frame.render_widget(Paragraph::new(scanned_text), content_chunks[4]);

            let found_text = format!(
                "  Found:    {} file{}",
                entries_found,
                if entries_found == 1 { "" } else { "s" }
            );
            frame.render_widget(Paragraph::new(found_text), content_chunks[5]);

            // Footer
            let footer =
                Paragraph::new("  Press q to cancel").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::Browsing => {
            if app.in_review_mode {
                draw_review(frame, app, area);
            } else {
                draw_browsing(frame, app, area);
            }
        }
        AppPhase::ConfirmExtract {
            entry_name,
            dict_size,
            ..
        } => {
            let (entry_name, dict_size) = (entry_name.clone(), *dict_size);

            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let header = Paragraph::new(" Large Dictionary Warning").style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let content_chunks = Layout::default()
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
                ])
                .split(chunks[2]);

            frame.render_widget(
                Paragraph::new(format!("  {}", entry_name)),
                content_chunks[1],
            );
            frame.render_widget(
                Paragraph::new("  This file uses a large compression dictionary.")
                    .style(Style::default().fg(Color::Yellow)),
                content_chunks[3],
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "  The decompressor will allocate ~{} of RAM.",
                    crate::format_size(dict_size)
                ))
                .style(Style::default().fg(Color::Yellow)),
                content_chunks[4],
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "  Dictionary size: {}",
                    crate::format_size(dict_size)
                )),
                content_chunks[6],
            );

            // Footer
            let footer = Paragraph::new("  Enter to proceed  ·  Esc to cancel")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::PasswordPrompt { .. } => {
            let chunks = Layout::vertical([
                Constraint::Min(1),    // top fill
                Constraint::Length(1), // error message (if any)
                Constraint::Length(1), // spacing
                Constraint::Length(1), // prompt label
                Constraint::Length(1), // input field
                Constraint::Length(1), // spacing
                Constraint::Length(1), // hint
                Constraint::Min(1),    // bottom fill
            ])
            .split(area);

            // Error message (e.g., "Wrong password")
            if let Some(ref err) = app.password_error {
                let err_text = Paragraph::new(format!("  {}", err))
                    .style(Style::default().fg(Color::Red))
                    .alignment(Alignment::Center);
                frame.render_widget(err_text, chunks[1]);
            }

            // Prompt label
            let label = Paragraph::new("  This archive is password protected.")
                .alignment(Alignment::Center);
            frame.render_widget(label, chunks[3]);

            // Input field: show dots for each character (mask the password)
            let masked: String = "*".repeat(app.password_input.len());
            let input_text = format!("  Password: {}_", masked); // Underscore as cursor
            let input = Paragraph::new(input_text)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Cyan));
            frame.render_widget(input, chunks[4]);

            // Hint
            let hint = Paragraph::new("  Enter to confirm  ·  Esc to cancel")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(hint, chunks[6]);
        }
        AppPhase::ExtractionStrategy {
            selected,
            editing_custom,
            custom_input,
            ..
        } => {
            let selected = *selected;
            let editing_custom = *editing_custom;
            let custom_input = custom_input.clone();
            let archive_stem = app.archive_stem();

            // Render the browse screen as backdrop.
            draw_browsing(frame, app, area);

            // Polish: Dim the backdrop to make the modal pop.
            let buffer = frame.buffer_mut();
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let cell = &mut buffer[(x, y)];
                    cell.set_style(cell.style().fg(Color::DarkGray));
                }
            }

            // Draw the modal overlay.
            draw_extraction_strategy_modal(
                frame,
                area,
                selected,
                editing_custom,
                &custom_input,
                &archive_stem,
            );
        }
        AppPhase::Extracting {
            entry_name,
            total_bytes,
            bytes_extracted,
        } => {
            let (entry_name, total_bytes, bytes_extracted) =
                (entry_name.clone(), *total_bytes, *bytes_extracted);

            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let header = Paragraph::new(" Extracting").style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let ratio = if total_bytes > 0 {
                (bytes_extracted as f64 / total_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let content_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // entry name
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // byte counter
                    Constraint::Min(1),    // fill
                ])
                .split(chunks[2]);

            frame.render_widget(
                Paragraph::new(format!("  {}", entry_name)),
                content_chunks[1],
            );

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio);
            frame.render_widget(gauge, content_chunks[3]);

            let speed_str = app.speed_tracker.format_speed().unwrap_or_default();
            let eta_str = if total_bytes > 0 {
                app.speed_tracker
                    .format_eta(total_bytes, bytes_extracted)
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let counter_text = if total_bytes > 0 {
                let mut text = format!(
                    "  Extracted:  {} / {}",
                    crate::format_size(bytes_extracted),
                    crate::format_size(total_bytes),
                );
                if !speed_str.is_empty() {
                    text.push_str(&format!("  —  {}  {}", speed_str, eta_str));
                }
                text
            } else {
                let mut text = format!("  Extracted:  {}", crate::format_size(bytes_extracted));
                if !speed_str.is_empty() {
                    text.push_str(&format!("  —  {}", speed_str));
                }
                text
            };
            frame.render_widget(Paragraph::new(counter_text), content_chunks[5]);

            // Footer
            let footer = Paragraph::new("  Esc to cancel  ·  q to quit")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
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

            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let title = format!(" Extracting ({} of {} files)", files_done, total_files);
            let header = Paragraph::new(title).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let ratio = if total_bytes > 0 {
                (bytes_extracted as f64 / total_bytes as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let content_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top padding
                    Constraint::Length(1), // current entry name
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // gauge
                    Constraint::Length(1), // spacing
                    Constraint::Length(1), // byte counter
                    Constraint::Min(1),    // fill
                ])
                .split(chunks[2]);

            let entry_label = if current_entry.is_empty() {
                "  Preparing...".to_string()
            } else {
                format!("  {}", current_entry)
            };
            frame.render_widget(Paragraph::new(entry_label), content_chunks[1]);

            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(ratio);
            frame.render_widget(gauge, content_chunks[3]);

            let speed_str = app.speed_tracker.format_speed().unwrap_or_default();
            let eta_str = if total_bytes > 0 {
                app.speed_tracker
                    .format_eta(total_bytes, bytes_extracted)
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let counter_text = if total_bytes > 0 {
                let mut text = format!(
                    "  Extracted:  {} / {}",
                    crate::format_size(bytes_extracted),
                    crate::format_size(total_bytes),
                );
                if !speed_str.is_empty() {
                    text.push_str(&format!("  —  {}  {}", speed_str, eta_str));
                }
                text
            } else {
                let mut text = format!("  Extracted:  {}", crate::format_size(bytes_extracted));
                if !speed_str.is_empty() {
                    text.push_str(&format!("  —  {}", speed_str));
                }
                text
            };
            frame.render_widget(Paragraph::new(counter_text), content_chunks[5]);

            // Footer
            let footer = Paragraph::new("  Esc to cancel  ·  q to quit")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::Error(msg) => {
            let msg = msg.clone();
            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            // Header
            let header = Paragraph::new(" Error")
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
            frame.render_widget(header, chunks[0]);

            // Separators
            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            // Content
            let is_auth = msg.contains("private") || msg.contains("credentials invalid");
            let hint = if is_auth {
                Some("Tip: run `snipe auth add gdrive|dropbox` to authenticate.")
            } else {
                None
            };

            let mut paras = vec![ratatui::text::Line::from(ratatui::text::Span::styled(
                msg,
                Style::default().fg(Color::Red),
            ))];
            if let Some(h) = hint {
                paras.push(ratatui::text::Line::from(""));
                paras.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                    h,
                    Style::default().fg(Color::DarkGray),
                )));
            }

            let para = Paragraph::new(paras).alignment(Alignment::Center);
            frame.render_widget(para, chunks[2]);

            // Footer
            let footer =
                Paragraph::new(" Press q to quit").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::Authenticating { provider, .. } => {
            let provider_name = match provider {
                crate::auth_cmd::AuthProvider::Gdrive => "Google Drive",
                crate::auth_cmd::AuthProvider::Dropbox => "Dropbox",
            };
            let chunks = Layout::vertical([
                Constraint::Length(1), // header
                Constraint::Length(1), // separator
                Constraint::Min(1),    // content
                Constraint::Length(1), // separator
                Constraint::Length(1), // footer
            ])
            .split(area);

            let header = Paragraph::new(format!(" Authenticating with {provider_name}")).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_widget(header, chunks[0]);

            let sep = "─".repeat(area.width as usize);
            let sep_style = Style::default().fg(Color::DarkGray);
            frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
            frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

            let content =
                "  Browser opened. Waiting for approval...\n  Token will be saved on success.";
            let para = Paragraph::new(content)
                .style(Style::default().fg(Color::White))
                .alignment(Alignment::Left);
            frame.render_widget(para, chunks[2]);

            let footer = Paragraph::new("  Esc to cancel  ·  q to quit")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, chunks[4]);
        }
        AppPhase::AuthPrompt { .. } => unreachable!(), // Handled above
    }

    if app.show_help {
        draw_help_popup(frame, area);
    }

    if app.bookmark_overlay.is_some() {
        // Polish: Dim the backdrop to make the modal pop.
        let buffer = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buffer[(x, y)];
                cell.set_style(cell.style().fg(Color::DarkGray));
            }
        }
        draw_bookmark_overlay(frame, app, area);
    }
}

fn draw_browsing(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

    let header_text = if app.is_filtering || !app.filter_query.is_empty() {
        format!("{}  [Filter: \"{}\"]", breadcrumb, app.filter_query)
    } else {
        breadcrumb
    };

    let header = Paragraph::new(header_text).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, chunks[0]);

    let sep = "─".repeat(area.width as usize);
    let separator_style = Style::default().fg(Color::DarkGray);
    frame.render_widget(
        Paragraph::new(sep.clone()).style(separator_style),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(sep).style(separator_style), chunks[3]);

    let rows: Vec<Row> = app
        .visible_items
        .iter()
        .map(|item| {
            let is_toggled = if item.is_directory {
                let prefix = format!("{}/", item.full_path);
                let mut has_children = false;
                let mut all_toggled = true;

                for e in app.entries.iter() {
                    if !e.is_directory && e.name.starts_with(&prefix) {
                        has_children = true;
                        if !app.toggled.contains(&e.name) {
                            all_toggled = false;
                            break; // Short-circuit, no need to check the rest
                        }
                    }
                }
                has_children && all_toggled
            } else {
                app.toggled.contains(&item.full_path)
            };
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
                Cell::from(format!(" {}", size_str)).style(Style::default().fg(Color::DarkGray)),
                Cell::from(format!("{} {}", marker, display_name)).style(name_style),
            ])
        })
        .collect();

    let highlight_style = Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(1)])
        .row_highlight_style(highlight_style);

    frame.render_stateful_widget(table, chunks[2], &mut app.table_state);

    draw_footer(frame, app, chunks[4]);
}

fn draw_review(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Build the review list from toggled entries.
    let mut review_items: Vec<&archive::ArchiveEntry> = app
        .entries
        .iter()
        .filter(|e| app.toggled.contains(&e.name))
        .collect();
    review_items.sort_by_key(|a| a.name.to_lowercase());

    // Use the same layout as browsing.
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // separator
        Constraint::Min(1),    // file list
        Constraint::Length(1), // separator
        Constraint::Length(1), // footer
    ])
    .split(area);

    // Header
    let header = Paragraph::new(" Review Selection (Tab/Esc to return)").style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, chunks[0]);

    // Separators
    let sep = "─".repeat(area.width as usize);
    let sep_style = Style::default().fg(Color::DarkGray);
    frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
    frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

    // Table rows
    let rows: Vec<Row> = review_items
        .iter()
        .map(|entry| {
            Row::new([
                Cell::from(format!(" {}", crate::format_size(entry.uncompressed_size)))
                    .style(Style::default().fg(Color::DarkGray)),
                Cell::from(format!(" {}", entry.name)).style(Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let highlight_style = Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(1)])
        .row_highlight_style(highlight_style);

    // Use a temporary TableState for the review view.
    let mut review_state = ratatui::widgets::TableState::default();
    review_state.select(if review_items.is_empty() {
        None
    } else {
        Some(app.review_selected)
    });
    frame.render_stateful_widget(table, chunks[2], &mut review_state);

    // Footer
    let total_size: u64 = review_items.iter().map(|e| e.uncompressed_size).sum();
    let n = review_items.len();
    let footer_text = format!(
        " [ {} file{} — {} ]  Space deselect  Enter choice  Tab return",
        n,
        if n == 1 { "" } else { "s" },
        crate::format_size(total_size),
    );
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[4]);
}

fn draw_extraction_strategy_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    selected: usize,
    editing_custom: bool,
    custom_input: &str,
    archive_stem: &str,
) {
    // Modal dimensions
    let modal_width: u16 = 40;
    let modal_height: u16 = 9; // title + blank + 3 options + blank + hint + border

    // Center vertically
    let popup_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(modal_height),
            Constraint::Min(0),
        ])
        .split(area)[1];

    // Center horizontally
    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(modal_width),
            Constraint::Min(0),
        ])
        .split(popup_area)[1];

    // Clear the area behind the modal
    frame.render_widget(Clear, popup_area);

    // Border block
    let block = Block::default()
        .title(" Extract to ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    // Inner area (inside the border)
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Layout the inner content
    let inner_chunks = Layout::vertical([
        Constraint::Length(1), // top padding
        Constraint::Length(1), // option 1: archive name
        Constraint::Length(1), // option 2: here
        Constraint::Length(1), // option 3: custom
        Constraint::Length(1), // bottom padding
        Constraint::Length(1), // hint line
    ])
    .split(inner);

    // Styles
    let highlight = Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::White);
    let hint_style = Style::default().fg(Color::DarkGray);

    // Option 1: Archive name folder
    let opt1_label = format!(" {}/", archive_stem);
    let padded1 = format!("{:<width$}", opt1_label, width = inner.width as usize);
    let style1 = if selected == 0 { highlight } else { normal };
    frame.render_widget(Paragraph::new(padded1).style(style1), inner_chunks[1]);

    // Option 2: Here (CWD)
    let opt2_label = " ./";
    let padded2 = format!("{:<width$}", opt2_label, width = inner.width as usize);
    let style2 = if selected == 1 { highlight } else { normal };
    frame.render_widget(Paragraph::new(padded2).style(style2), inner_chunks[2]);

    // Option 3: Custom
    if editing_custom {
        let input_text = format!(" {}/_", custom_input); // Trailing underscore as cursor
        let padded = format!("{:<width$}", input_text, width = inner.width as usize);
        frame.render_widget(Paragraph::new(padded).style(highlight), inner_chunks[3]);
    } else {
        let label = " [type a name]/";
        let padded = format!("{:<width$}", label, width = inner.width as usize);
        let style3 = if selected == 2 { highlight } else { hint_style };
        frame.render_widget(Paragraph::new(padded).style(style3), inner_chunks[3]);
    }

    // Hint line
    let hint = Paragraph::new(" Esc cancel · Enter confirm")
        .style(hint_style)
        .alignment(Alignment::Center);
    frame.render_widget(hint, inner_chunks[5]);
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
            " [ {} file{} — {} · Cost: {} ]  Enter choice  q quit",
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
 g / Home:  Jump to top
 G / End:   Jump to bottom
 Space:     Toggle selection
 a:         Toggle all in folder
 Tab:       Review selection
 Enter:     Choose destination
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
            Constraint::Length(15),
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

fn draw_home(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // separator
        Constraint::Min(1),    // content
        Constraint::Length(1), // separator
        Constraint::Length(1), // footer
    ])
    .split(area);

    // Header: Solid cyan bar with black bold text
    let header = Paragraph::new(" Archive Sniper — Home ").style(
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, chunks[0]);

    // Separators
    let sep = "─".repeat(area.width as usize);
    let sep_style = Style::default().fg(Color::DarkGray);
    frame.render_widget(Paragraph::new(sep.clone()).style(sep_style), chunks[1]);
    frame.render_widget(Paragraph::new(sep).style(sep_style), chunks[3]);

    // Content
    let bookmark_count = app.home_bookmarks.len();
    // Header + rows (or 1-line placeholder if empty), cap at 6 visible rows.
    let bookmarks_body_rows: u16 = if bookmark_count == 0 {
        1
    } else {
        bookmark_count.min(6) as u16
    };
    let bookmarks_height: u16 = 1 /* header */ + bookmarks_body_rows;

    let content_chunks = Layout::vertical([
        Constraint::Length(2),                // Snipe Input + optional inline error
        Constraint::Length(1),                // blank spacer
        Constraint::Length(bookmarks_height), // Bookmarks (header + body)
        Constraint::Length(1),                // blank spacer
        Constraint::Min(1),                   // Recent (header + list)
    ])
    .split(chunks[2]);

    let section_style = |focused: bool| {
        if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };

    // 1. Snipe Input
    let snipe_chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(content_chunks[0]);

    let snipe_focused = app.home_focus == HomeFocus::SnipeInput;
    let label_style = if snipe_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut snipe_text = vec![ratatui::text::Span::styled("[ Snipe: ] ", label_style)];

    if app.snipe_input.is_empty() && !snipe_focused {
        snipe_text.push(ratatui::text::Span::styled(
            " Paste cloud link or local path...",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        snipe_text.push(ratatui::text::Span::styled(
            &app.snipe_input,
            Style::default().fg(Color::White),
        ));
        if snipe_focused {
            snipe_text.push(ratatui::text::Span::styled(
                "_",
                Style::default().fg(Color::White),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(ratatui::text::Line::from(snipe_text)),
        snipe_chunks[0],
    );

    if let Some(ref err) = app.home_input_error {
        frame.render_widget(
            Paragraph::new(format!(" {}", err)).style(Style::default().fg(Color::Red)),
            snipe_chunks[1],
        );
    }

    // 2. Bookmarks
    let bookmarks_focused = app.home_focus == HomeFocus::Bookmarks;
    let bookmark_chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(content_chunks[2]);

    frame.render_widget(
        Paragraph::new("─── BOOKMARKS ───").style(section_style(bookmarks_focused)),
        bookmark_chunks[0],
    );

    if app.home_bookmarks.is_empty() {
        frame.render_widget(
            Paragraph::new("  — no bookmarks —").style(Style::default().fg(Color::DarkGray)),
            bookmark_chunks[1],
        );
    } else {
        let rows: Vec<Row> = app
            .home_bookmarks
            .entries()
            .iter()
            .map(|item| {
                let size_str = crate::format_size(item.size);
                let name_str = item.name.clone();
                let provider_tag = item.provider.tag();

                let name_style = if bookmarks_focused {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                let name_line = ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!(" {}  ", name_str), name_style),
                    ratatui::text::Span::styled(provider_tag, Style::default().fg(Color::DarkGray)),
                ]);

                Row::new([
                    Cell::from(format!(" {}", size_str))
                        .style(Style::default().fg(Color::DarkGray)),
                    Cell::from(name_line),
                ])
            })
            .collect();

        let mut table_state = ratatui::widgets::TableState::default();
        table_state.select(Some(app.home_bookmarks_selected));

        let highlight_style = if bookmarks_focused {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(1)])
            .row_highlight_style(highlight_style);

        frame.render_stateful_widget(table, bookmark_chunks[1], &mut table_state);
    }

    // 3. Recent Archives
    let recent_focused = app.home_focus == HomeFocus::Recent;
    let recent_chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(content_chunks[4]);

    frame.render_widget(
        Paragraph::new("─── RECENT ───").style(section_style(recent_focused)),
        recent_chunks[0],
    );

    let visible_recent = app.recent_visible();
    if visible_recent.is_empty() {
        frame.render_widget(
            Paragraph::new("  — no recent archives —").style(Style::default().fg(Color::DarkGray)),
            recent_chunks[1],
        );
    } else {
        let rows: Vec<Row> = visible_recent
            .iter()
            .map(|item| {
                let size_str = crate::format_size(item.size);
                let name_str = item.name.clone();
                let provider_tag = item.provider.tag();

                let name_style = if recent_focused {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                let name_line = ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!(" {}  ", name_str), name_style),
                    ratatui::text::Span::styled(provider_tag, Style::default().fg(Color::DarkGray)),
                ]);

                Row::new([
                    Cell::from(format!(" {}", size_str))
                        .style(Style::default().fg(Color::DarkGray)),
                    Cell::from(name_line),
                ])
            })
            .collect();

        let mut table_state = ratatui::widgets::TableState::default();
        table_state.select(Some(app.home_recent_selected));

        let highlight_style = if recent_focused {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(1)])
            .row_highlight_style(highlight_style);

        frame.render_stateful_widget(table, recent_chunks[1], &mut table_state);
    }

    // Footer
    let footer_text = if let Some(ref msg) = app.status_message {
        format!("  {}", msg)
    } else {
        match app.home_focus {
            HomeFocus::SnipeInput if app.snipe_input.is_empty() => {
                "  Paste a link or path. Tab cycle  Ctrl+q quit".into()
            }
            HomeFocus::SnipeInput if looks_like_url(&app.snipe_input) => {
                "  Enter open  Tab cycle  Ctrl+q quit".into()
            }
            HomeFocus::SnipeInput => "  Enter open  Tab complete  Ctrl+q quit".into(),
            HomeFocus::Recent if !visible_recent.is_empty() => {
                "  Enter open  b bookmark  d delete  Tab cycle  q quit".into()
            }
            HomeFocus::Bookmarks if !app.home_bookmarks.is_empty() => {
                "  Enter open  r rename  d delete  Tab cycle  q quit".into()
            }
            _ => "  Tab cycle  q quit".into(),
        }
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[4]);
}

fn draw_bookmark_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(overlay) = app.bookmark_overlay.as_ref() else {
        return;
    };

    use crate::tui::app::{OverlayMode, OverlayState};
    use ratatui::widgets::Clear;

    let modal_w = 60.min(area.width.saturating_sub(4));
    let modal_h = match overlay.state {
        OverlayState::Editing => 5,   // border + title + input + spacer + hint
        OverlayState::Collision => 6, // border + title + message + hint + spacer
    };
    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect {
        x: modal_x,
        y: modal_y,
        width: modal_w,
        height: modal_h,
    };

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(match overlay.state {
            OverlayState::Editing => match &overlay.mode {
                OverlayMode::Save { .. } => " Bookmark ",
                OverlayMode::Rename { .. } => " Rename Bookmark ",
            },
            OverlayState::Collision => " Name Conflict ",
        })
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    match overlay.state {
        OverlayState::Editing => {
            let chunks = Layout::vertical([
                Constraint::Length(1), // "Name:" + input
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
            ])
            .split(inner);

            let line = ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(" Name: ", Style::default().fg(Color::White)),
                ratatui::text::Span::styled(
                    overlay.input.as_str(),
                    Style::default().fg(Color::Cyan),
                ),
                ratatui::text::Span::styled("_", Style::default().fg(Color::White)),
            ]);
            frame.render_widget(Paragraph::new(line), chunks[0]);

            frame.render_widget(
                Paragraph::new(" Enter save  Esc cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[2],
            );
        }
        OverlayState::Collision => {
            let chunks = Layout::vertical([
                Constraint::Length(1), // "[!] Bookmark \"X\" already exists."
                Constraint::Length(1), // spacer
                Constraint::Length(1), // "(o) Overwrite (r) Rename (c) Cancel"
            ])
            .split(inner);
            let msg = format!(" [!] Bookmark \"{}\" already exists.", overlay.input);
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(Color::Yellow)),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new(" (o) Overwrite  (r) Rename  (c) Cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[2],
            );
        }
    }
}

fn draw_auth_prompt(frame: &mut Frame<'_>, area: Rect, provider: &crate::auth_cmd::AuthProvider) {
    let provider_name = match provider {
        crate::auth_cmd::AuthProvider::Gdrive => "Google Drive",
        crate::auth_cmd::AuthProvider::Dropbox => "Dropbox",
    };

    let modal_width: u16 = 40;
    let modal_height: u16 = 6;

    let popup_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(modal_height),
            Constraint::Min(0),
        ])
        .split(area)[1];

    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(modal_width),
            Constraint::Min(0),
        ])
        .split(popup_area)[1];

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Authentication required ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let inner_chunks = Layout::vertical([
        Constraint::Length(1), // line 1 padding
        Constraint::Length(1), // line 2 (provider is private)
        Constraint::Length(1), // line 3 (blank)
        Constraint::Length(1), // line 4 (question + y/n)
    ])
    .split(inner);

    let line2 = ratatui::text::Line::from(vec![
        ratatui::text::Span::styled(
            format!(" {}", provider_name),
            Style::default().fg(Color::Cyan),
        ),
        ratatui::text::Span::raw(" file is private."),
    ]);
    frame.render_widget(Paragraph::new(line2), inner_chunks[1]);

    let line4 = ratatui::text::Line::from(vec![
        ratatui::text::Span::raw(format!(" Authenticate with {}? ", provider_name)),
        ratatui::text::Span::styled("[Y/n]", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line4), inner_chunks[3]);
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn home_renders_empty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new_home(None);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn home_renders_with_bookmarks() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new_home(None);

        use crate::home::bookmarks::{BookmarkEntry, BookmarkList};
        use crate::home::provider::Provider;
        use chrono::Utc;

        let b1 = BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);

        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn home_renders_with_overlay_editing() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new_home(None);

        use crate::home::provider::Provider;
        use crate::tui::app::{BookmarkOverlay, OverlayMode, OverlayState};

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s1".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "New".into(),
            state: OverlayState::Editing,
        });

        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn home_renders_with_overlay_collision() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new_home(None);

        use crate::home::provider::Provider;
        use crate::tui::app::{BookmarkOverlay, OverlayMode, OverlayState};

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s1".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "Existing".into(),
            state: OverlayState::Collision,
        });

        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }
}
