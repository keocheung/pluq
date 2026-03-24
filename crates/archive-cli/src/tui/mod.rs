#![allow(dead_code, unused_imports)]

mod app;
mod ui;

pub use app::App;

/// Entry point for the interactive TUI browser.
pub async fn run(_source: String) -> Result<(), Box<dyn std::error::Error>> {
    todo!("TUI will be implemented in subsequent steps")
}
