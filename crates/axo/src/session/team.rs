//! Team coordination and the fixed instructions shared by its member sessions.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;

use super::children::SessionTable;
use crate::store::TeamLink;

/// Live coordination state of one team, shared by its member sessions in this process.
pub(crate) struct TeamState {
    id: String,
    /// Member session ids, roster order.
    members: Vec<String>,
    waits: Mutex<Waits>,
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct Waits {
    waiting: HashSet<String>,
    failed: HashSet<String>,
}

/// A member's handle for `wait`.
pub(crate) struct TeamSeat {
    pub(crate) team: Arc<TeamState>,
    pub(crate) member: String,
    pub(crate) sessions: Arc<SessionTable>,
}

pub(crate) struct WaitGuard {
    team: Arc<TeamState>,
    member: String,
}

impl TeamState {
    pub(crate) fn new(link: &TeamLink) -> Self {
        Self {
            id: link.id.clone(),
            members: link
                .roster
                .iter()
                .map(|entry| entry.session_id.clone())
                .collect(),
            waits: Mutex::new(Waits::default()),
            changed: watch::Sender::new(0),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub(crate) fn bump(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub(crate) fn enter_wait(self: &Arc<Self>, member: &str) -> WaitGuard {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waiting
            .insert(member.to_owned());
        self.bump();
        WaitGuard {
            team: self.clone(),
            member: member.to_owned(),
        }
    }

    pub(crate) fn deadlocked(&self, member: &str, sessions: &SessionTable) -> bool {
        let waiting = {
            let mut waits = self.waits.lock().unwrap_or_else(PoisonError::into_inner);
            if waits.failed.remove(member) {
                return true;
            }
            waits.waiting.clone()
        };
        let stuck = self.members.iter().all(|id| {
            let Some(session) = sessions.live(id) else {
                return true;
            };
            let activity = *session.hooks.activity.borrow();
            let jobs = &session.hooks.tools.runner.jobs;
            activity.maintenance.is_none()
                && (!activity.running || waiting.contains(id))
                && !(session.hooks.may_deliver() && jobs.has_pending())
                && jobs.running_admitted().is_empty()
        });
        if !stuck || !waiting.contains(member) {
            return false;
        }
        {
            let mut waits = self.waits.lock().unwrap_or_else(PoisonError::into_inner);
            waits.failed = waits
                .waiting
                .iter()
                .filter(|waiting| waiting.as_str() != member)
                .cloned()
                .collect();
        }
        self.bump();
        true
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        {
            let mut waits = self
                .team
                .waits
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            waits.waiting.remove(&self.member);
            waits.failed.remove(&self.member);
        }
        self.team.bump();
    }
}

pub(crate) fn mutual_wait(team: &str) -> String {
    format!(
        "Mutual wait: every running member of {team} is waiting and nothing is pending, so no one can reply. Send what you need with send, or finish with your result."
    )
}

pub(crate) fn section(link: &TeamLink) -> String {
    let name = link
        .name
        .as_ref()
        .map_or_else(String::new, |name| format!(" ({name})"));
    let mut parts = vec![format!(
        "You are {}, one member of team {}{name}. Your teammates work alongside you at the same time.",
        link.member, link.id
    )];
    if !link.charter.trim().is_empty() {
        parts.push(link.charter.clone());
    }
    if let Some(brief) = &link.brief
        && !brief.trim().is_empty()
    {
        parts.push(format!("Your brief: {brief}"));
    }
    let mut roster = String::from("Roster:");
    for entry in &link.roster {
        roster.push_str("\n- ");
        roster.push_str(&entry.name);
        if entry.name == link.member {
            roster.push_str(" (you)");
        }
        if !entry.description.is_empty() {
            roster.push_str(": ");
            roster.push_str(&entry.description);
        }
    }
    parts.push(roster);
    parts.push("Message a teammate or your parent (\"parent\") with send; there is no broadcast. Messages from teammates arrive between your steps; to wait for one, call wait. Your final message is your result for your parent.".into());
    parts.join("\n\n")
}
