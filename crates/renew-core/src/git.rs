use crate::error::Result;
use crate::runner::{CommandOutput, CommandRunner};
use std::path::Path;

pub struct Git<'a> {
    pub runner: &'a dyn CommandRunner,
    pub bin: &'a str,
    pub cwd: &'a Path,
}

impl<'a> Git<'a> {
    async fn run(&self, args: &[&str]) -> Result<CommandOutput> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.runner.run(self.bin, &owned, Some(self.cwd), &[]).await
    }

    pub async fn fetch(&self, remote: &str, branch: &str) -> Result<()> {
        let out = self
            .run(&["fetch", "--prune", "--quiet", remote, branch])
            .await?;
        out.require_success(self.bin)?;
        Ok(())
    }

    pub async fn rev_parse(&self, reference: &str) -> Result<Option<String>> {
        let out = self.run(&["rev-parse", reference]).await?;
        if out.success() {
            Ok(Some(out.stdout_trimmed().to_string()))
        } else {
            Ok(None)
        }
    }

    pub async fn reset_hard(&self, rev: &str) -> Result<()> {
        let out = self.run(&["reset", "--hard", "--quiet", rev]).await?;
        out.require_success(self.bin)?;
        Ok(())
    }
}
