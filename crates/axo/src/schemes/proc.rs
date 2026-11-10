//! `proc://`: lists and inspects the session's background jobs.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target};
use crate::jobs::{JobId, JobKind, JobRegistry, headline};

pub struct ProcScheme {
    pub jobs: Arc<JobRegistry>,
}

/// The paths `proc://` serves; anything else is rejected.
enum ProcPath {
    List,
    Job(JobId),
}

fn parse(path: &str) -> Option<ProcPath> {
    if path.is_empty() {
        return Some(ProcPath::List);
    }
    JobId::parse(path).map(ProcPath::Job)
}

fn unsupported(url: &SchemeUrl) -> ToolError {
    ToolError(format!(
        "Unsupported proc:// path: {}. Use proc:// or proc://<id>.",
        url.path
    ))
}

#[async_trait]
impl Scheme for ProcScheme {
    fn name(&self) -> &str {
        "proc"
    }

    fn prompt_doc(&self) -> &str {
        "proc:// background jobs and subagents: read proc:// lists them, read proc://bg_N or proc://agent_N shows one; stop one with kill."
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
                    .map(|job| match &job.kind {
                        JobKind::Command { command, .. } => {
                            format!("{}  {}  {}", job.id, job.status.label(), headline(command))
                        }
                        JobKind::Child {
                            agent, model, team, ..
                        } => {
                            let name = team
                                .as_ref()
                                .map(|team| format!("{} of {}", team.member, team.id));
                            format!(
                                "{}  {}  {}  {}",
                                job.id,
                                job.status.label(),
                                name.as_deref()
                                    .unwrap_or_else(|| agent.as_deref().unwrap_or("subagent")),
                                model
                            )
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            ProcPath::Job(id) => {
                let (job, result) = self
                    .jobs
                    .get(id)
                    .ok_or_else(|| ToolError(format!("No job {id}")))?;
                let mut text = match &job.kind {
                    JobKind::Command {
                        command, artifact, ..
                    } => format!(
                        "Job: {}\nCommand: {}\nStatus: {}\nOutput: artifact://{}",
                        job.id,
                        command,
                        job.status.label(),
                        artifact
                    ),
                    JobKind::Child {
                        session_id,
                        agent,
                        model,
                        team,
                        ..
                    } => {
                        let mut text = format!(
                            "Job: {}\nAgent: {}",
                            job.id,
                            agent.as_deref().unwrap_or("subagent"),
                        );
                        if let Some(team) = team {
                            text.push_str(&format!("\nTeam: {} ({})", team.id, team.member));
                        }
                        text.push_str(&format!(
                            "\nModel: {model}\nSession: {session_id}\nStatus: {}\nDetails: agent://{id}",
                            job.status.label()
                        ));
                        text
                    }
                };
                if let Some(result) = &result {
                    text.push_str("\n\nResult:\n");
                    text.push_str(result);
                }
                Ok(text)
            }
        }
    }
}
