pub mod app;
pub mod cli;
pub mod config;
pub mod registry;
pub mod resolver;
pub mod superset_cli;
pub mod terminal_host;
pub mod transport;

use anyhow::Result;
use clap::Parser;

pub fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    app::run(cli)
}
