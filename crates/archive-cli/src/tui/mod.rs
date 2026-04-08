mod app;
mod ui;

#[allow(unused_imports)]
pub use app::App;

use std::io;

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tracing::{debug, info};

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub async fn run(
    source: String,
    reader: archive_source::ResolvedSource,
) -> Result<(), Box<dyn std::error::Error>> {
    info!(source = %source, "TUI starting");
    // Install panic hook that restores terminal before printing panic info.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut terminal = init_terminal()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut app = app::App::new(source.clone());

    // Spawn the initial archive open task.
    debug!("spawning Archive::open task");
    let tx_open = tx.clone();
    tokio::spawn(async move {
        let result = archive::Archive::open_reader(&source, reader).await;
        let _ = tx_open.send(app::AppMessage::ArchiveOpened(result));
    });

    // Dedicated event reader task (ratatui recommended pattern).
    // Runs independently so key events are captured immediately,
    // even when the main loop is busy drawing or processing messages.
    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crossterm::event::KeyEvent>();
    let cancel_flag_reader = app.cancel_flag.clone();
    tokio::spawn(async move {
        let mut event_stream = crossterm::event::EventStream::new();
        while let Some(Ok(event)) = event_stream.next().await {
            if let crossterm::event::Event::Key(key) = event
                && key.kind == crossterm::event::KeyEventKind::Press
            {
                // Fast-path: set the cancel flag directly from this
                // task so spawn_blocking sees it without waiting for
                // the main loop to process the key event.
                if key.code == crossterm::event::KeyCode::Esc
                    || key.code == crossterm::event::KeyCode::Char('q')
                {
                    cancel_flag_reader.store(
                        true,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                }
                if event_tx.send(key).is_err() {
                    break;
                }
            }
        }
    });

    const FPS: u64 = 20;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1000 / FPS));

    loop {
        tokio::select! {
            biased;

            // 1. Process Key Events instantly (State update only)
            key = event_rx.recv() => {
                if let Some(key) = key {
                    app.handle_key(key, &tx);
                }
            }
            // 2. Process System Messages instantly (State update only)
            msg = rx.recv() => {
                if let Some(msg) = msg {
                    app.handle_message(msg, &tx);
                }
            }
            // 3. ONLY DRAW when the tick fires (Rendering)
            _ = tick.tick() => {
                // Read latest progress from shared atomics for the draw.
                app.poll_progress();
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
            }
        }

        if app.should_quit {
            break;
        }
    }

    info!("TUI shutting down");
    restore_terminal(&mut terminal)?;
    Ok(())
}
