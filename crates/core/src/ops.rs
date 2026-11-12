//! Changes to the task list as values. The window, the MCP tools, and the
//! Worker API all go through `Op::apply`, so the desktop app can apply a
//! change at once and send the same value to the Worker, which applies it
//! to its own copy with the same rules.

use serde::{Deserialize, Serialize};

use crate::model::Store;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Add {
        title: String,
        #[serde(default)]
        notes: String,
        #[serde(default)]
        link: Option<String>,
    },
    Rename {
        id: String,
        title: String,
    },
    /// Notes and link of a manual task.
    EditManual {
        id: String,
        notes: String,
        link: Option<String>,
    },
    SetPinned {
        id: String,
        pinned: bool,
    },
    Settle {
        id: String,
    },
    Reopen {
        id: String,
    },
    Archive {
        id: String,
    },
    ArchiveMany {
        ids: Vec<String>,
    },
    Delete {
        id: String,
    },
    MarkSeen {
        id: String,
    },
    MarkAllSeen,
}

/// What an op did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub changed: bool,
    /// The new task's id, for `Add`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// How many tasks changed, for the ops that touch several.
    #[serde(default)]
    pub count: usize,
}

impl Applied {
    fn one(changed: bool) -> Self {
        Applied {
            changed,
            id: None,
            count: usize::from(changed),
        }
    }

    fn many(count: usize) -> Self {
        Applied {
            changed: count > 0,
            id: None,
            count,
        }
    }
}

impl Op {
    /// Apply to `store`. `Err` is a refusal with a message for the user,
    /// such as deleting a task whose item is still open.
    pub fn apply(&self, store: &mut Store) -> Result<Applied, String> {
        Ok(match self {
            Op::Add { title, notes, link } => {
                if title.trim().is_empty() {
                    return Err("A task needs a title.".into());
                }
                let id = store.add_manual(title, notes, link.clone());
                Applied {
                    changed: true,
                    id: Some(id),
                    count: 1,
                }
            }
            Op::Rename { id, title } => Applied::one(store.rename(id, title)?),
            Op::EditManual { id, notes, link } => {
                Applied::one(store.edit_manual(id, notes, link.clone())?)
            }
            Op::SetPinned { id, pinned } => Applied::one(store.set_pinned(id, *pinned)?),
            Op::Settle { id } => Applied::one(store.settle(id)?),
            Op::Reopen { id } => Applied::one(store.reopen(id)?),
            Op::Archive { id } => Applied::one(store.archive(id)?),
            Op::ArchiveMany { ids } => Applied::many(store.archive_many(ids)),
            Op::Delete { id } => Applied::one(store.delete(id)?),
            Op::MarkSeen { id } => Applied::one(store.mark_seen(id)),
            Op::MarkAllSeen => Applied::many(store.mark_all_seen()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState;

    #[test]
    fn ops_round_trip_as_json_and_apply() {
        let mut s = Store::in_memory();
        let add = Op::Add {
            title: "Write".into(),
            notes: String::new(),
            link: None,
        };
        let json = serde_json::to_string(&add).unwrap();
        assert_eq!(
            json,
            r#"{"op":"add","title":"Write","notes":"","link":null}"#
        );
        let back: Op = serde_json::from_str(r#"{"op":"add","title":"Write"}"#).unwrap();
        assert_eq!(back, add);

        let id = add.apply(&mut s).unwrap().id.unwrap();
        let settle = Op::Settle { id: id.clone() };
        assert!(settle.apply(&mut s).unwrap().changed);
        assert!(!settle.apply(&mut s).unwrap().changed);
        assert_eq!(s.get(&id).unwrap().state, TaskState::Settled);
        assert!(Op::SetPinned { id, pinned: true }.apply(&mut s).is_err());
        let empty = Op::Add {
            title: " ".into(),
            notes: String::new(),
            link: None,
        };
        assert!(empty.apply(&mut s).is_err());
    }
}
