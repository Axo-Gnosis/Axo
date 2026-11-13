//! One routing operation for host and model sends.

use super::children::SessionTable;
use super::{AgentRoutes, Session};
use crate::coordination::{SendIdentity, SendKind, SendReceipt, SubmissionProvenance};
use crate::error::Error;
use crate::jobs::child::ChildMessage;
use crate::jobs::{JobId, JobKind, JobRegistry, Namespace};
use crate::store::SubmissionKind;

impl Session {
    /// Routes exactly like the model's `send`: owned child id, teammate name, or `parent`.
    /// The receipt identifies the saved submission or parent result message.
    pub async fn send(&self, to: &str, message: &str, steer: bool) -> Result<SendReceipt, Error> {
        let link = self.spawn_link();
        let routes = AgentRoutes {
            session_id: self.id().to_owned(),
            parent: link.map(|link| link.parent_session_id.clone()),
            team: link.and_then(|link| link.team.clone()),
        };
        send(
            &self.hooks.tools.runner.jobs,
            &self.hooks.inherited.sessions,
            &routes,
            to,
            message,
            steer,
        )
        .await
    }
}

pub(crate) async fn send(
    jobs: &JobRegistry,
    sessions: &SessionTable,
    routes: &AgentRoutes,
    to: &str,
    content: &str,
    steer: bool,
) -> Result<SendReceipt, Error> {
    if content.trim().is_empty() {
        return Err(Error::InvalidInput("message must not be blank".into()));
    }
    let child_id = JobId::parse(to).filter(|id| id.ns == Namespace::Agent);
    if steer && child_id.is_none() {
        return Err(Error::InvalidInput(format!(
            "Only your subagents can be steered; send to {to} without steer."
        )));
    }
    let receipt = |kind, session_id: String, receipt| SendReceipt {
        to: to.to_owned(),
        kind,
        session_id,
        receipt,
    };
    let provenance = |route| SubmissionProvenance {
        session_id: routes.session_id.clone(),
        route,
        name: routes.team.as_ref().map(|team| team.member.clone()),
    };
    if to == "parent" {
        let parent = routes
            .parent
            .as_ref()
            .ok_or_else(|| Error::InvalidInput("This session has no parent.".into()))?;
        let registry = sessions.registry(parent).ok_or_else(|| {
            Error::InvalidInput("Your parent session is not open; nothing was sent.".into())
        })?;
        let message = registry
            .child_message(
                &routes.session_id,
                ChildMessage {
                    run_id: None,
                    text: content.to_owned(),
                    is_error: false,
                    partial: false,
                    note: true,
                    usage: None,
                    value: None,
                },
                true,
            )
            .await?
            .ok_or_else(|| {
                Error::InvalidInput(
                    "Your parent no longer tracks this session; nothing was sent.".into(),
                )
            })?;
        return Ok(receipt(
            SendKind::Note,
            parent.clone(),
            SendIdentity::Message(message.to_string()),
        ));
    }
    if let Some(id) = child_id {
        let (job, _) = jobs
            .child(id)
            .ok_or_else(|| Error::InvalidInput(format!("No agent {id}")))?;
        let JobKind::Child { session_id, .. } = &job.kind else {
            return Err(Error::InvalidInput(format!("No agent {id}")));
        };
        let child = sessions.session(session_id).await?;
        let (kind, submission_kind) = if steer {
            (SendKind::Steer, SubmissionKind::Steer)
        } else {
            (SendKind::FollowUp, SubmissionKind::FollowUp)
        };
        let submission_id = child
            .queue_routed(submission_kind, content.to_owned().into(), provenance(kind))
            .await?;
        return Ok(receipt(
            kind,
            session_id.clone(),
            SendIdentity::Submission(submission_id),
        ));
    }
    if let Some(team) = &routes.team {
        if to == team.member {
            return Err(Error::InvalidInput(format!(
                "You are {to}; send to a teammate or to parent."
            )));
        }
        if let Some(entry) = team.roster.iter().find(|entry| entry.name == to) {
            let submission_id = sessions
                .session(&entry.session_id)
                .await?
                .post_mail(&team.member, content, provenance(SendKind::Mail))
                .await?;
            return Ok(receipt(
                SendKind::Mail,
                entry.session_id.clone(),
                SendIdentity::Submission(submission_id),
            ));
        }
    }
    let mut message = format!("No agent named {to}. Your subagents are agent_N");
    if routes.parent.is_some() {
        message.push_str("; your parent is parent");
    }
    if let Some(team) = &routes.team {
        message.push_str("; your teammates are ");
        message.push_str(
            &team
                .roster
                .iter()
                .filter(|entry| entry.name != team.member)
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    message.push('.');
    Err(Error::InvalidInput(message))
}
