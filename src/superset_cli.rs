use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct SupersetCliAdapter {
    bin: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl SupersetCliAdapter {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub fn version(&self) -> Result<CommandOutput> {
        self.run(["--version"])
    }

    pub fn status(&self, json: bool) -> Result<CommandOutput> {
        if json {
            self.run(["--json", "status"])
        } else {
            self.run(["status"])
        }
    }

    pub fn agents_list(&self, json: bool, host: Option<&str>) -> Result<CommandOutput> {
        let mut args = Vec::new();
        if json {
            args.push("--json");
        }
        args.extend(["agents", "list"]);
        if let Some(host) = host {
            args.extend(["--host", host]);
        } else {
            args.push("--local");
        }
        self.run(args)
    }

    pub fn agents_create(
        &self,
        workspace: &str,
        agent: &str,
        prompt: &str,
    ) -> Result<CommandOutput> {
        self.run([
            OsString::from("--json"),
            OsString::from("agents"),
            OsString::from("create"),
            OsString::from("--workspace"),
            OsString::from(workspace),
            OsString::from("--agent"),
            OsString::from(agent),
            OsString::from("--prompt"),
            OsString::from(prompt),
        ])
    }

    /// Adapter for public `superset terminals create`.
    ///
    /// Kept for the architecture's future terminal-spawn path. The current
    /// `ssctl spawn` command only exposes agent-backed session creation.
    pub fn terminals_create(
        &self,
        workspace: &str,
        command: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<CommandOutput> {
        let mut args = vec![
            OsString::from("--json"),
            OsString::from("terminals"),
            OsString::from("create"),
            OsString::from("--workspace"),
            OsString::from(workspace),
        ];
        if let Some(command) = command {
            args.push(OsString::from("--command"));
            args.push(OsString::from(command));
        }
        if let Some(cwd) = cwd {
            args.push(OsString::from("--cwd"));
            args.push(cwd.as_os_str().to_owned());
        }
        self.run(args)
    }

    fn run<I, S>(&self, args: I) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if !self.bin.exists() {
            bail!("superset CLI not found at {}", self.bin.display());
        }

        let output = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to run {}", self.bin.display()))?;

        Ok(CommandOutput {
            status_code: status_code(output.status),
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn status_code(status: ExitStatus) -> Option<i32> {
    status.code()
}
