use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::Cli;

pub const TERMINAL_HOST_PROTOCOL_VERSION: u64 = 2;

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub superset_bin: PathBuf,
    pub superset_home: PathBuf,
    pub terminal_host_socket: PathBuf,
    pub terminal_host_token: PathBuf,
    pub registry_dir: PathBuf,
    pub results_dir: PathBuf,
}

impl RuntimeConfig {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let superset_home = match cli.superset_home.clone() {
            Some(path) => path,
            None => default_superset_home()?,
        };
        let superset_bin = cli
            .superset_bin
            .clone()
            .unwrap_or_else(|| superset_home.join("bin").join("superset"));
        let repo_root = env::current_dir().context("failed to resolve current directory")?;

        Ok(Self {
            terminal_host_socket: superset_home.join("terminal-host.sock"),
            terminal_host_token: superset_home.join("terminal-host.token"),
            registry_dir: repo_root.join(".ssctl"),
            results_dir: repo_root.join(".agent-results"),
            superset_home,
            superset_bin,
        })
    }
}

fn default_superset_home() -> Result<PathBuf> {
    Ok(home_dir()?.join(".superset"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
