use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ssctl", version, about = "Superset session control helper")]
pub struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    pub superset_bin: Option<PathBuf>,

    #[arg(long, global = true, value_name = "PATH")]
    pub superset_home: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Status(OutputArgs),
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    Sessions(OutputArgs),
    Spawn(SpawnArgs),
    Send(SendArgs),
    Handoff(HandoffArgs),
    Report(ReportArgs),
}

#[derive(Debug, Args, Clone, Copy)]
pub struct OutputArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    List(AgentsListArgs),
}

#[derive(Debug, Args, Clone)]
#[command(group(
    ArgGroup::new("target_host")
        .multiple(false)
        .args(["host", "local"])
))]
pub struct AgentsListArgs {
    #[arg(long)]
    pub json: bool,

    #[arg(long, value_name = "HOST_ID")]
    pub host: Option<String>,

    #[arg(long)]
    pub local: bool,
}

#[derive(Debug, Args)]
pub struct SpawnArgs {
    #[arg(long)]
    pub agent: String,

    #[arg(long)]
    pub role: String,

    #[arg(long, value_name = "WORKSPACE_ID")]
    pub workspace: String,

    #[arg(long, value_name = "FILE_OR_TEXT")]
    pub prompt: String,

    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["role", "session"])
))]
#[command(group(
    ArgGroup::new("input")
        .required(true)
        .multiple(false)
        .args(["file", "stdin"])
))]
pub struct SendArgs {
    #[arg(long, group = "target")]
    pub role: Option<String>,

    #[arg(long, group = "target", value_name = "SESSION_ID")]
    pub session: Option<String>,

    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    #[arg(long)]
    pub stdin: bool,

    #[arg(long)]
    pub force_unregistered_session: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, value_name = "WORKSPACE_ID")]
    pub workspace: Option<String>,
}

#[derive(Debug, Args)]
pub struct HandoffArgs {
    #[arg(long)]
    pub to: String,

    #[arg(long, value_name = "PATH")]
    pub file: PathBuf,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[arg(long)]
    pub to: String,

    #[arg(long, value_name = "PATH")]
    pub file: PathBuf,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn parses_phase_one_commands() {
        let cli = Cli::parse_from(["ssctl", "sessions", "--json"]);
        match cli.command {
            Command::Sessions(args) => assert!(args.json),
            _ => panic!("expected sessions command"),
        }

        let cli = Cli::parse_from(["ssctl", "agents", "list"]);
        match cli.command {
            Command::Agents { .. } => {}
            _ => panic!("expected agents command"),
        }
    }

    #[test]
    fn send_requires_a_target() {
        let result = Cli::try_parse_from(["ssctl", "send", "--stdin"]);
        assert!(result.is_err());
    }

    #[test]
    fn send_requires_input() {
        let result = Cli::try_parse_from(["ssctl", "send", "--role", "worker"]);
        assert!(result.is_err());
    }

    #[test]
    fn send_accepts_one_target_and_one_input() {
        let result = Cli::try_parse_from(["ssctl", "send", "--role", "worker", "--stdin"]);
        assert!(result.is_ok());
    }
}
