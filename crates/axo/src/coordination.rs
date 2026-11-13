//! Typed receipts shared by host operations and model coordination tools.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillReceipt {
    pub id: String,
    pub stopping: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SendKind {
    Note,
    Steer,
    FollowUp,
    Mail,
}

/// The originating session and actual delivery route, independent of roster names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionProvenance {
    pub session_id: String,
    pub route: SendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "camelCase")]
pub enum SendIdentity {
    Submission(String),
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReceipt {
    pub to: String,
    pub kind: SendKind,
    pub session_id: String,
    pub receipt: SendIdentity,
}

impl SendReceipt {
    pub(crate) fn text(&self) -> String {
        match self.kind {
            SendKind::Note => "Sent to your parent.".into(),
            SendKind::Steer => format!(
                "Steered {}: it reads your message after its current tool calls.",
                self.to
            ),
            SendKind::FollowUp => {
                format!("Sent to {}. Its reply arrives as a new result.", self.to)
            }
            SendKind::Mail => format!("Sent to {}.", self.to),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaitReason {
    Results,
    Mail,
    Interrupted,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitReceipt {
    pub reason: WaitReason,
    /// Result message references, e.g. `agent_1/2`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SpawnReceipt {
    Child {
        job: String,
    },
    Team {
        team: String,
        jobs: IndexMap<String, String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receipts_preserve_tool_keys_and_round_trip() {
        let spawn = SpawnReceipt::Team {
            team: "team_1".into(),
            jobs: IndexMap::from([("worker".into(), "agent_1".into())]),
        };
        let value = serde_json::to_value(&spawn).unwrap();
        assert_eq!(value, json!({"team":"team_1", "jobs":{"worker":"agent_1"}}));
        assert_eq!(
            serde_json::from_value::<SpawnReceipt>(value).unwrap(),
            spawn
        );
        for reason in [
            WaitReason::Results,
            WaitReason::Mail,
            WaitReason::Interrupted,
            WaitReason::Idle,
        ] {
            let receipt = WaitReceipt {
                reason,
                jobs: vec![],
            };
            let value = serde_json::to_value(&receipt).unwrap();
            assert!(value.get("reason").is_some());
            assert_eq!(
                serde_json::from_value::<WaitReceipt>(value).unwrap(),
                receipt
            );
        }
        let sent = SendReceipt {
            to: "agent_1".into(),
            kind: SendKind::FollowUp,
            session_id: "child-session".into(),
            receipt: SendIdentity::Submission("submission-id".into()),
        };
        let value = serde_json::to_value(&sent).unwrap();
        assert_eq!(value["kind"], "followUp");
        assert_eq!(value["sessionId"], "child-session");
        assert_eq!(
            value["receipt"],
            json!({"type":"submission","id":"submission-id"})
        );
        assert_eq!(serde_json::from_value::<SendReceipt>(value).unwrap(), sent);
    }
}
