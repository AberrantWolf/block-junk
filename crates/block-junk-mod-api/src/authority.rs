//! Stable authority identities. Session-local ECS entities never identify an owner.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventCause {
    pub root: u64,
    pub depth: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReservationOwner {
    Player {
        id: u64,
    },
    Npc {
        id: u64,
    },
    Behavior {
        mod_id: String,
        registration_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Accepted,
    Travelling,
    Working,
    Completed,
    Interrupted,
    Rejected,
    Failed,
}
impl JobState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Rejected | Self::Failed
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobTarget {
    Interaction {
        slot: crate::architecture::UseSlotId,
        placement_revision: u64,
        action: String,
        exclusive: bool,
    },
    CraftOrder {
        order_id: u64,
        order_revision: u64,
    },
    Construction {
        project_id: u64,
        project_revision: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRecord {
    pub cause: Option<EventCause>,
    pub id: u64,
    pub revision: u64,
    pub actor: ReservationOwner,
    pub controller: ReservationOwner,
    pub target: JobTarget,
    pub state: JobState,
    pub reason: Option<String>,
    pub observed: bool,
}

impl JobTarget {
    pub fn conflicts_with(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Interaction {
                    slot: a,
                    exclusive: ae,
                    ..
                },
                Self::Interaction {
                    slot: b,
                    exclusive: be,
                    ..
                },
            ) => a == b && (*ae || *be),
            _ => self == other,
        }
    }
}
