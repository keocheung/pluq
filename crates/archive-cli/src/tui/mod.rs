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
    reader: Option<archive::Source>,
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
        let result = match reader {
            Some(r) => archive::Archive::open_reader(&source, r).await,
            None => archive::Archive::open(&source).await,
        };
        let _ = tx_open.send(app::AppMessage::ArchiveOpened(result));
    });

    let mut event_stream = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        tokio::select! {
            _ = tick.tick() => {}
            event = event_stream.next() => {
                if let Some(Ok(crossterm::event::Event::Key(key))) = event {
                    app.handle_key(key, &tx);
                }
            }
            msg = rx.recv() => {
                if let Some(msg) = msg {
                    app.handle_message(msg, &tx);
                }
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
