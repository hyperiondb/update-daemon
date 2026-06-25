use crate::error::{Error, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Default ceiling on a single child process. Generous enough for a slow
/// `docker compose pull`, but bounded so a hung `git fetch` against a dead
/// remote cannot wedge a service's sync forever.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }

    /// Turn a non-zero exit into an `Error::Command`.
    pub fn require_success(&self, program: &str) -> Result<&Self> {
        if self.success() {
            Ok(self)
        } else {
            Err(Error::Command {
                program: program.to_string(),
                code: self.status,
                message: self.stderr.trim().to_string(),
            })
        }
    }
}

/// Everything needed to invoke a child process. Abstracted behind a trait so the
/// sync engine can be driven by a fake in unit tests without real git/docker.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<CommandOutput>;
}

/// Real process execution via tokio, with a hard timeout and kill-on-drop.
#[derive(Debug, Clone, Copy)]
pub struct SystemRunner {
    timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }
}

impl SystemRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<CommandOutput> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = cmd.spawn().map_err(|e| Error::Command {
            program: program.to_string(),
            code: None,
            message: format!("failed to spawn: {e}"),
        })?;

        // On timeout the future is dropped, which drops the Child; kill_on_drop
        // then reaps the process instead of leaking an orphan.
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(res) => res.map_err(|e| Error::Command {
                program: program.to_string(),
                code: None,
                message: format!("failed to wait: {e}"),
            })?,
            Err(_) => {
                return Err(Error::Command {
                    program: program.to_string(),
                    code: None,
                    message: format!("timed out after {:?}", self.timeout),
                })
            }
        };

        Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A recorded invocation and the canned reply a `FakeRunner` returned for it.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl RecordedCall {
    /// Convenience for assertions: `program arg0 arg1 ...`.
    pub fn command_line(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

#[cfg(any(test, feature = "testkit"))]
pub use fake::{FakeRunner, Rule};

#[cfg(any(test, feature = "testkit"))]
mod fake {
    use super::*;
    use std::sync::Mutex;

    /// Matches a call by a substring contained in the joined `program + args`.
    #[derive(Debug, Clone)]
    pub struct Rule {
        pub needle: Vec<String>,
        pub output: CommandOutput,
    }

    /// Scriptable `CommandRunner` for tests. Rules are matched in order; the first
    /// whose every needle token appears in the command line wins.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: Vec<Rule>,
        default: Option<CommandOutput>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_default(mut self, status: i32, stdout: &str, stderr: &str) -> Self {
            self.default = Some(CommandOutput {
                status: Some(status),
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            });
            self
        }

        /// Add a rule: when the command line contains all `needle` tokens, reply.
        pub fn on(mut self, needle: &[&str], status: i32, stdout: &str) -> Self {
            self.rules.push(Rule {
                needle: needle.iter().map(|s| s.to_string()).collect(),
                output: CommandOutput {
                    status: Some(status),
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            });
            self
        }

        pub fn on_fail(mut self, needle: &[&str], status: i32, stderr: &str) -> Self {
            self.rules.push(Rule {
                needle: needle.iter().map(|s| s.to_string()).collect(),
                output: CommandOutput {
                    status: Some(status),
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
            });
            self
        }

        pub fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }

        pub fn command_lines(&self) -> Vec<String> {
            self.calls().iter().map(|c| c.command_line()).collect()
        }

        pub fn ran_matching(&self, tokens: &[&str]) -> bool {
            self.command_lines()
                .iter()
                .any(|line| tokens.iter().all(|t| line.contains(t)))
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            cwd: Option<&Path>,
            _env: &[(String, String)],
        ) -> Result<CommandOutput> {
            let mut line = program.to_string();
            for a in args {
                line.push(' ');
                line.push_str(a);
            }
            self.calls.lock().unwrap().push(RecordedCall {
                program: program.to_string(),
                args: args.to_vec(),
                cwd: cwd.map(|p| p.to_path_buf()),
            });

            for rule in &self.rules {
                if rule.needle.iter().all(|n| line.contains(n)) {
                    return Ok(rule.output.clone());
                }
            }
            if let Some(d) = &self.default {
                return Ok(d.clone());
            }
            Err(Error::other(format!("FakeRunner: no rule for `{line}`")))
        }
    }
}
