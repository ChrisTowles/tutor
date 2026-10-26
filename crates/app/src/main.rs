//! `tutor`: one window to see, hear, talk to and drive the kids' PCs.

mod config;
mod media;
mod settings;
mod station;
mod ui;

fn main() -> anyhow::Result<()> {
    ui::run(config::load()?)
}
