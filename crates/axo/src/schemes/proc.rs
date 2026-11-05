//! `proc://`: lists, inspects and stops the session's background jobs.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target};
use crate::jobs::{JobRegistry, headline, job_name, parse_job_id};

pub struct ProcScheme {
    pub jobs: Arc<JobRegistry>,
}

/// The paths `proc://` serves; anything else is rejected.
enum ProcPath {
    List,
    Job(u64),
    Kill(u64),
}

fn parse(path: &str) -> Option<ProcPath> {
    if path.is_empty() {
        return Some(ProcPath::List);
    }
    let (job, action) = match path.split_once('/') {
        Some((job, action)) => (job, Some(action)),
        None => (path, None),
    };
    let id = parse_job_id(job)?;
    match action {
        None => Some(ProcPath::Job(id)),
        Some("kill") => Some(ProcPath::Kill(id)),
        Some(_) => None,
    }
}

fn unsupported(url: &SchemeUrl) -> ToolError {
    ToolError(format!(
        "Unsupported proc:// path: {}. Use proc://, proc://bg_N or proc://bg_N/kill.",
        url.path
    ))
}

#[async_trait]
impl Scheme for ProcScheme {
    fn name(&self) -> &str {
        "proc"
    }

    fn prompt_doc(&self) -> &str {
        "proc:// background jobs: read proc:// lists them, read proc://bg_N shows one, write proc://bg_N/kill (empty content) stops it."
    }

    async fn resolve(&self, _url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        Ok(Target::Virtual)
    }

    async fn read(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<String, ToolError> {
        match parse(&url.path).ok_or_else(|| unsupported(url))? {
            ProcPath::List => {
                let jobs = self.jobs.list();
                if jobs.is_empty() {
                    return Ok("No background jobs.".into());
                }
                Ok(jobs
                    .iter()
                    .map(|job| {
                        format!(
                            "{}  {}  {}",
                            job_name(job.id),
                            job.status.label(),
                            headline(&job.command)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            ProcPath::Job(id) => {
                let (job, result) = self
                    .jobs
                    .get(id)
                    .ok_or_else(|| ToolError(format!("No job {}", job_name(id))))?;
                let mut text = format!(
                    "Job: {}\nCommand: {}\nStatus: {}\nOutput: artifact://{}",
                    job_name(job.id),
                    job.command,
                    job.status.label(),
                    job.artifact
                );
                if let Some(result) = &result {
                    text.push_str("\n\nResult:\n");
                    text.push_str(result);
                }
                Ok(text)
            }
            ProcPath::Kill(_) => Err(ToolError(format!("{url} cannot be read"))),
        }
    }

    async fn write(
        &self,
        url: &SchemeUrl,
        _content: &str,
        _cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        match parse(&url.path).ok_or_else(|| unsupported(url))? {
            ProcPath::Kill(id) if self.jobs.kill(id) => Ok(format!("Stopping {}.", job_name(id))),
            ProcPath::Kill(id) => Ok(format!("{} is not running.", job_name(id))),
            ProcPath::List | ProcPath::Job(_) => Err(ToolError(format!("{url} is read-only"))),
        }
    }
}
