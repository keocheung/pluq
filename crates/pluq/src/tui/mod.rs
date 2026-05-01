mod app;
mod ui;

#[allow(unused_imports)]
pub use app::App;

use std::io;

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
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

pub async fn run_with_source(
    source: String,
    token_store: Option<std::sync::Arc<archive_auth::TokenStore>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!(source = %source, "TUI starting in Browse mode");
    let app = app::App::new_with_source(source.clone(), token_store.clone());

    let config = crate::source_config(token_store);

    run_app(
        app,
        Some(
            move |tx: tokio::sync::mpsc::UnboundedSender<app::AppMessage>| {
                debug!("spawning Archive::open task");
                tokio::spawn(async move {
                    let result =
                        archive_cache::open(&source, &config, archive::ArchiveConfig::default())
                            .await;
                    let _ = tx.send(app::AppMessage::ArchiveOpened(result));
                });
            },
        ),
    )
    .await
}

pub async fn run_home(
    token_store: Option<std::sync::Arc<archive_auth::TokenStore>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("TUI starting in Home mode");
    let app = app::App::new_home(token_store);
    run_app(
        app,
        None::<fn(tokio::sync::mpsc::UnboundedSender<app::AppMessage>)>,
    )
    .await
}

async fn run_app<F>(mut app: app::App, setup: Option<F>) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<app::AppMessage>),
{
    // Install panic hook that restores terminal before printing panic info.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut terminal = init_terminal()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    if let Some(setup_fn) = setup {
        setup_fn(tx.clone());
    }

    // Dedicated event reader task (ratatui recommended pattern).
    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crossterm::event::KeyEvent>();
    let cancel_flag_reader = app.cancel_flag.clone();
    tokio::spawn(async move {
        let mut event_stream = crossterm::event::EventStream::new();
        while let Some(Ok(event)) = event_stream.next().await {
            if let crossterm::event::Event::Key(key) = event
                && key.kind == crossterm::event::KeyEventKind::Press
            {
                if key.code == crossterm::event::KeyCode::Esc
                    || key.code == crossterm::event::KeyCode::Char('q')
                {
                    cancel_flag_reader.store(true, std::sync::atomic::Ordering::SeqCst);
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

            key = event_rx.recv() => {
                if let Some(key) = key {
                    app.handle_key(key, &tx);
                }
            }
            msg = rx.recv() => {
                if let Some(msg) = msg {
                    app.handle_message(msg, &tx);
                }
            }
            _ = tick.tick() => {
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
