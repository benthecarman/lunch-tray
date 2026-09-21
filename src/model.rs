//! Task model, persistence, and the state machine.
//!
//! A task has one `state` field with three values: `Active`, `Settled`, and
//! `Archived`. Deleted tasks are removed from the store. `pinned` is a view
//! flag. Every section sorts by the task's last activity, newest first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const LOCAL_PROJECT: &str = "Local";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Active,
    Settled,
    Archived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    GitHub,
    Forgejo,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::Forgejo => "Forgejo",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::Forgejo => "forgejo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    PullRequest,
    Issue,
}

impl RemoteKind {
    pub fn label(self) -> &'static str {
        match self {
            RemoteKind::PullRequest => "PR",
            RemoteKind::Issue => "Issue",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteState {
    Open,
    Closed,
    Merged,
}

/// Where a task goes when its remote item is closed or merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnClose {
    #[default]
    Settle,
    Archive,
}

/// A pull request or issue on a remote forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub provider: Provider,
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub kind: RemoteKind,
    pub url: String,
    pub author: String,
    pub state: RemoteState,
    /// A draft pull request. Rendered muted, since it is not ready yet.
    #[serde(default)]
    pub draft: bool,
    pub remote_updated_at: DateTime<Utc>,
    /// True while one of the configured queries returns the item. An open
    /// item that leaves the queries no longer needs you.
    #[serde(default = "default_true")]
    pub in_queries: bool,
}

fn default_true() -> bool {
    true
}

impl RemoteRef {
    /// Stable task id for this remote item.
    pub fn key(&self) -> String {
        format!(
            "{}:{}/{}/{}#{}",
            self.provider.slug(),
            self.host,
            self.owner,
            self.repo,
            self.number
        )
    }

    pub fn project(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn label(&self) -> String {
        format!("{}/{} #{}", self.owner, self.repo, self.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Origin {
    Manual,
    Remote(RemoteRef),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    /// Title as last seen on the remote. Used to keep a user rename.
    #[serde(default)]
    pub remote_title: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub link: Option<String>,
    pub project: String,
    pub state: TaskState,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub unseen: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub origin: Origin,
}

impl Task {
    pub fn remote(&self) -> Option<&RemoteRef> {
        match &self.origin {
            Origin::Remote(r) => Some(r),
            Origin::Manual => None,
        }
    }

    /// A task is "running" while its remote item is open. The store refuses
    /// to delete a running task, because the next sync would bring it back.
    /// Archiving is fine: the task comes back to Active on new activity.
    pub fn is_running(&self) -> bool {
        matches!(&self.origin, Origin::Remote(r) if r.state == RemoteState::Open)
    }

    pub fn url(&self) -> Option<&str> {
        match &self.origin {
            Origin::Remote(r) => Some(r.url.as_str()),
            Origin::Manual => self.link.as_deref().filter(|s| !s.is_empty()),
        }
    }

    pub fn rung(&self) -> Rung {
        // Evaluated in order: archived, then pinned, then the state field.
        match self.state {
            TaskState::Archived => Rung::Archived,
            TaskState::Active => Rung::Active,
            TaskState::Settled if self.pinned => Rung::Active,
            TaskState::Settled => Rung::Settled,
        }
    }
}

/// Where a task sits. Derived, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    Active,
    Settled,
    Archived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemePref,
    #[serde(default)]
    pub show_archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct StoreData {
    #[serde(default)]
    tasks: Vec<Task>,
    #[serde(default)]
    next_manual_id: u64,
    #[serde(default)]
    settings: Settings,
}

/// The tasks visible in each section, already sorted.
#[derive(Default)]
pub struct Sections {
    pub pinned: Vec<Task>,
    pub active: Vec<Task>,
    pub settled: Vec<Task>,
    pub archived: Vec<Task>,
}

/// A remote item as returned by a provider sync.
#[derive(Clone, Debug)]
pub struct RemoteItem {
    pub r: RemoteRef,
    pub title: String,
}

/// What changed during a remote sync, for notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    New {
        id: String,
        label: String,
    },
    /// Still open, but no longer in any of your queries.
    Released {
        id: String,
        label: String,
    },
    /// Back in your queries after being out of them.
    Requested {
        id: String,
        label: String,
    },
    Closed {
        id: String,
        label: String,
        merged: bool,
    },
    ReopenedRemotely {
        id: String,
        label: String,
    },
    Woken {
        id: String,
        label: String,
    },
}

pub struct Store {
    data: StoreData,
    path: Option<PathBuf>,
}

impl Store {
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Store {
            data: StoreData::default(),
            path: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        } else {
            StoreData::default()
        };
        Ok(Store {
            data,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.data)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn settings(&self) -> &Settings {
        &self.data.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.data.settings
    }

    pub fn tasks(&self) -> &[Task] {
        &self.data.tasks
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.data.tasks.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.data.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn sections(&self) -> Sections {
        let mut s = Sections::default();
        for t in &self.data.tasks {
            match t.rung() {
                Rung::Archived => s.archived.push(t.clone()),
                Rung::Active if t.pinned => s.pinned.push(t.clone()),
                Rung::Active => s.active.push(t.clone()),
                Rung::Settled => s.settled.push(t.clone()),
            }
        }
        let newest = |a: &Task, b: &Task| Self::last_activity(b).cmp(&Self::last_activity(a));
        s.pinned.sort_by(newest);
        s.active.sort_by(newest);
        s.settled.sort_by(newest);
        s.archived.sort_by(newest);
        s
    }

    pub fn count_unseen(&self) -> usize {
        self.data
            .tasks
            .iter()
            .filter(|t| t.unseen && t.state == TaskState::Active)
            .count()
    }

    /// When the task last moved: the remote item's update time, or the
    /// task's own for manual ones.
    pub fn last_activity(t: &Task) -> DateTime<Utc> {
        match &t.origin {
            Origin::Remote(r) => r.remote_updated_at,
            Origin::Manual => t.updated_at,
        }
    }

    /// Unarchived, unpinned tasks with no activity since `cutoff`.
    pub fn stale_ids(&self, cutoff: DateTime<Utc>) -> Vec<String> {
        self.data
            .tasks
            .iter()
            .filter(|t| {
                t.state != TaskState::Archived && !t.pinned && Self::last_activity(t) < cutoff
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Archive each id. Returns how many changed.
    pub fn archive_many(&mut self, ids: &[String]) -> usize {
        ids.iter()
            .filter(|id| self.archive(id).unwrap_or(false))
            .count()
    }

    /// Archive every unarchived task that `pred` selects. Returns their labels.
    pub fn archive_matching(&mut self, pred: impl Fn(&Task) -> bool) -> Vec<String> {
        let ids: Vec<(String, String)> = self
            .data
            .tasks
            .iter()
            .filter(|t| t.state != TaskState::Archived && pred(t))
            .map(|t| {
                let label = t
                    .remote()
                    .map(|r| r.label())
                    .unwrap_or_else(|| t.title.clone());
                (t.id.clone(), label)
            })
            .collect();
        ids.into_iter()
            .filter(|(id, _)| self.archive(id).unwrap_or(false))
            .map(|(_, label)| label)
            .collect()
    }

    // ----- transitions ------------------------------------------------------
    // Every transition returns Ok(true) when something changed, Ok(false) for
    // a no-op, and Err(message) when the store refuses.

    pub fn add_manual(&mut self, title: &str, notes: &str, link: Option<String>) -> String {
        self.data.next_manual_id += 1;
        let id = format!("manual:{}", self.data.next_manual_id);
        let now = Utc::now();
        self.data.tasks.push(Task {
            id: id.clone(),
            title: title.trim().to_string(),
            remote_title: None,
            notes: notes.trim().to_string(),
            link: link.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()),
            project: LOCAL_PROJECT.to_string(),
            state: TaskState::Active,
            pinned: false,
            unseen: false,
            created_at: now,
            updated_at: now,
            origin: Origin::Manual,
        });
        id
    }

    pub fn rename(&mut self, id: &str, title: &str) -> Result<bool, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("A task needs a title.".into());
        }
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.title == title {
            return Ok(false);
        }
        t.title = title.to_string();
        t.updated_at = Utc::now();
        Ok(true)
    }

    pub fn edit_manual(
        &mut self,
        id: &str,
        notes: &str,
        link: Option<String>,
    ) -> Result<bool, String> {
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.origin != Origin::Manual {
            return Err("Only manual tasks have editable notes.".into());
        }
        let link = link.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
        if t.notes == notes.trim() && t.link == link {
            return Ok(false);
        }
        t.notes = notes.trim().to_string();
        t.link = link;
        t.updated_at = Utc::now();
        Ok(true)
    }

    pub fn mark_seen(&mut self, id: &str) -> bool {
        match self.get_mut(id) {
            Some(t) if t.unseen => {
                t.unseen = false;
                true
            }
            _ => false,
        }
    }

    /// Pin or unpin. Only tasks on the Active rung can be pinned.
    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> Result<bool, String> {
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.state != TaskState::Active {
            return Err("Only active tasks can be pinned.".into());
        }
        if t.pinned == pinned {
            return Ok(false);
        }
        t.pinned = pinned;
        Ok(true)
    }

    /// Active -> Settled. Drops the pin.
    pub fn settle(&mut self, id: &str) -> Result<bool, String> {
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.state != TaskState::Active {
            return Ok(false);
        }
        t.state = TaskState::Settled;
        t.pinned = false;
        t.unseen = false;
        t.updated_at = Utc::now();
        Ok(true)
    }

    /// Settled or Archived -> Active. The task never comes back pinned.
    pub fn reopen(&mut self, id: &str) -> Result<bool, String> {
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.state == TaskState::Active {
            return Ok(false);
        }
        t.state = TaskState::Active;
        t.pinned = false;
        t.updated_at = Utc::now();
        Ok(true)
    }

    /// Any rung -> Archived. Works for open remote items too, so a stale pull
    /// request can be put away until something happens on it.
    pub fn archive(&mut self, id: &str) -> Result<bool, String> {
        let Some(t) = self.get_mut(id) else {
            return Ok(false);
        };
        if t.state == TaskState::Archived {
            return Ok(false);
        }
        t.state = TaskState::Archived;
        t.updated_at = Utc::now();
        Ok(true)
    }

    /// Remove the task. Refused while the remote item is open.
    pub fn delete(&mut self, id: &str) -> Result<bool, String> {
        let Some(t) = self.get(id) else {
            return Ok(false);
        };
        if t.is_running() {
            return Err(running_message(t, "delete"));
        }
        self.data.tasks.retain(|t| t.id != id);
        Ok(true)
    }

    // ----- remote sync ------------------------------------------------------

    /// Upsert remote items. Open items that are not tracked yet become new
    /// Active tasks. Tracked items follow their remote state:
    ///
    /// * open -> closed or merged: an Active task settles, or archives when
    ///   `on_close` says so on its own
    /// * closed -> open: a Settled or Archived task reopens at the top of Active
    /// * open with newer activity: a Settled or Archived task wakes into Active
    ///
    /// Archived tasks wake like settled ones, so archiving a stale open item
    /// puts it away only until something happens on it.
    pub fn apply_remote(&mut self, on_close: OnClose, items: &[RemoteItem]) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        let now = Utc::now();
        for item in items {
            let key = item.r.key();
            let Some(t) = self.get_mut(&key) else {
                if item.r.state == RemoteState::Open {
                    self.data.tasks.push(Task {
                        id: key.clone(),
                        title: item.title.clone(),
                        remote_title: Some(item.title.clone()),
                        notes: String::new(),
                        link: None,
                        project: item.r.project(),
                        state: TaskState::Active,
                        pinned: false,
                        unseen: true,
                        created_at: now,
                        updated_at: now,
                        origin: Origin::Remote(item.r.clone()),
                    });
                    events.push(SyncEvent::New {
                        id: key,
                        label: item.r.label(),
                    });
                }
                continue;
            };
            let Origin::Remote(old) = t.origin.clone() else {
                continue;
            };
            let mut changed = false;
            if t.remote_title.as_deref() != Some(item.title.as_str()) {
                if t.remote_title.as_deref() == Some(t.title.as_str()) || t.remote_title.is_none() {
                    t.title = item.title.clone();
                }
                t.remote_title = Some(item.title.clone());
                changed = true;
            }
            if old != item.r {
                t.origin = Origin::Remote(item.r.clone());
                changed = true;
            }
            let was_open = old.state == RemoteState::Open;
            let is_open = item.r.state == RemoteState::Open;
            let label = item.r.label();
            if !was_open && is_open {
                match t.state {
                    TaskState::Settled | TaskState::Archived => {
                        t.state = TaskState::Active;
                        t.pinned = false;
                        t.unseen = true;
                        changed = true;
                        events.push(SyncEvent::ReopenedRemotely { id: key, label });
                    }
                    TaskState::Active => {
                        // Reopened by hand already: keep its place and pin.
                        t.unseen = true;
                        changed = true;
                        events.push(SyncEvent::ReopenedRemotely { id: key, label });
                    }
                }
            } else if was_open && !is_open {
                let target = match on_close {
                    OnClose::Settle => TaskState::Settled,
                    OnClose::Archive => TaskState::Archived,
                };
                let moves = t.state == TaskState::Active
                    || (on_close == OnClose::Archive && t.state == TaskState::Settled);
                if moves {
                    t.state = target;
                    t.pinned = false;
                    changed = true;
                }
                t.unseen = false;
                events.push(SyncEvent::Closed {
                    id: key,
                    label,
                    merged: item.r.state == RemoteState::Merged,
                });
            } else if is_open
                && old.in_queries
                && !item.r.in_queries
                && t.state == TaskState::Active
                && !t.pinned
            {
                // Left your queries, for example after you submitted the
                // review. Pinned tasks are yours to settle.
                t.state = TaskState::Settled;
                t.unseen = false;
                changed = true;
                events.push(SyncEvent::Released { id: key, label });
            } else if is_open
                && !old.in_queries
                && item.r.in_queries
                && t.state != TaskState::Active
            {
                t.state = TaskState::Active;
                t.unseen = true;
                changed = true;
                events.push(SyncEvent::Requested { id: key, label });
            } else if is_open
                && item.r.remote_updated_at > old.remote_updated_at
                && t.state != TaskState::Active
            {
                t.state = TaskState::Active;
                t.unseen = true;
                changed = true;
                events.push(SyncEvent::Woken { id: key, label });
            }
            if changed {
                t.updated_at = now;
            }
        }
        events
    }
}

fn running_message(t: &Task, verb: &str) -> String {
    let r = t.remote().expect("running tasks have a remote");
    format!(
        "Cannot {verb} {}: the {} is still open on {}. Settle it instead.",
        r.label(),
        r.kind.label(),
        r.provider.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn remote(number: u64, state: RemoteState, updated: DateTime<Utc>) -> RemoteItem {
        RemoteItem {
            r: RemoteRef {
                provider: Provider::GitHub,
                host: "github.com".into(),
                owner: "octo".into(),
                repo: "cat".into(),
                number,
                kind: RemoteKind::PullRequest,
                url: format!("https://github.com/octo/cat/pull/{number}"),
                author: "me".into(),
                state,
                draft: false,
                remote_updated_at: updated,
                in_queries: true,
            },
            title: format!("PR {number}"),
        }
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn manual_task_walks_the_rungs() {
        let mut s = Store::in_memory();
        let id = s.add_manual("Buy milk", "", None);
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
        assert_eq!(s.settle(&id), Ok(true));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Settled);
        assert_eq!(s.archive(&id), Ok(true));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Archived);
        assert_eq!(s.delete(&id), Ok(true));
        assert!(s.get(&id).is_none());
    }

    #[test]
    fn settling_a_settled_task_and_archiving_an_archived_task_are_noops() {
        let mut s = Store::in_memory();
        let id = s.add_manual("x", "", None);
        assert_eq!(s.settle(&id), Ok(true));
        let before = s.get(&id).unwrap().clone();
        assert_eq!(s.settle(&id), Ok(false));
        assert_eq!(s.get(&id).unwrap(), &before);
        assert_eq!(s.archive(&id), Ok(true));
        let before = s.get(&id).unwrap().clone();
        assert_eq!(s.archive(&id), Ok(false));
        assert_eq!(s.get(&id).unwrap(), &before);
    }

    #[test]
    fn settle_never_touches_an_archived_task() {
        let mut s = Store::in_memory();
        let id = s.add_manual("x", "", None);
        s.archive(&id).unwrap();
        assert_eq!(s.settle(&id), Ok(false));
        assert_eq!(s.get(&id).unwrap().state, TaskState::Archived);
        assert_eq!(s.reopen(&id), Ok(true));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn settling_a_pinned_task_drops_the_pin() {
        let mut s = Store::in_memory();
        let id = s.add_manual("x", "", None);
        assert_eq!(s.set_pinned(&id, true), Ok(true));
        assert!(s.sections().pinned.iter().any(|t| t.id == id));
        assert_eq!(s.settle(&id), Ok(true));
        let t = s.get(&id).unwrap();
        assert!(!t.pinned);
        assert_eq!(t.rung(), Rung::Settled);
        assert!(s.sections().settled.iter().any(|t| t.id == id));
    }

    #[test]
    fn reopen_does_not_repin() {
        let mut s = Store::in_memory();
        let id = s.add_manual("x", "", None);
        s.set_pinned(&id, true).unwrap();
        s.settle(&id).unwrap();
        s.reopen(&id).unwrap();
        assert!(!s.get(&id).unwrap().pinned);

        let id2 = s.add_manual("y", "", None);
        s.set_pinned(&id2, true).unwrap();
        s.archive(&id2).unwrap();
        s.reopen(&id2).unwrap();
        assert!(!s.get(&id2).unwrap().pinned);
    }

    #[test]
    fn pinning_is_only_for_active_tasks() {
        let mut s = Store::in_memory();
        let id = s.add_manual("x", "", None);
        s.settle(&id).unwrap();
        assert!(s.set_pinned(&id, true).is_err());
    }

    #[test]
    fn every_section_sorts_by_last_activity() {
        let mut s = Store::in_memory();
        s.apply_remote(
            OnClose::Settle,
            &[
                remote(1, RemoteState::Open, t0() + Duration::hours(1)),
                remote(2, RemoteState::Open, t0() + Duration::hours(3)),
                remote(3, RemoteState::Open, t0() + Duration::hours(2)),
            ],
        );
        let ids: Vec<String> = s.tasks().iter().map(|t| t.id.clone()).collect();
        let order = |s: &Store| -> Vec<String> {
            s.sections().active.iter().map(|t| t.id.clone()).collect()
        };
        assert_eq!(
            order(&s),
            vec![ids[1].clone(), ids[2].clone(), ids[0].clone()]
        );
        // Settling and reopening does not move a task; remote activity does.
        s.settle(&ids[1]).unwrap();
        s.reopen(&ids[1]).unwrap();
        assert_eq!(
            order(&s),
            vec![ids[1].clone(), ids[2].clone(), ids[0].clone()]
        );
        s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::hours(4))],
        );
        assert_eq!(
            order(&s),
            vec![ids[0].clone(), ids[1].clone(), ids[2].clone()]
        );
        // Settled sorts the same way.
        s.settle(&ids[0]).unwrap();
        s.settle(&ids[2]).unwrap();
        let settled: Vec<String> = s.sections().settled.iter().map(|t| t.id.clone()).collect();
        assert_eq!(settled, vec![ids[0].clone(), ids[2].clone()]);
    }

    #[test]
    fn store_refuses_to_delete_an_open_remote_item() {
        let mut s = Store::in_memory();
        let ev = s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        assert!(matches!(ev.as_slice(), [SyncEvent::New { .. }]));
        let id = s.tasks()[0].id.clone();
        assert!(s.delete(&id).is_err());
        assert_eq!(s.settle(&id), Ok(true));
        assert!(s.delete(&id).is_err());
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Merged, t0())]);
        assert_eq!(s.delete(&id), Ok(true));
    }

    #[test]
    fn a_stale_open_item_can_be_archived_until_it_moves_again() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        assert_eq!(s.archive(&id), Ok(true));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Archived);
        // No activity: stays archived, even after leaving the queries.
        let mut quiet = remote(1, RemoteState::Open, t0());
        quiet.r.in_queries = false;
        assert!(s.apply_remote(OnClose::Settle, &[quiet.clone()]).is_empty());
        assert!(s.apply_remote(OnClose::Settle, &[quiet]).is_empty());
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Archived);
        // Still cannot delete while open.
        assert!(s.delete(&id).is_err());
        // Someone pushes: it comes back to the top of Active.
        let mut pushed = remote(1, RemoteState::Open, t0() + Duration::minutes(1));
        pushed.r.in_queries = false;
        let ev = s.apply_remote(OnClose::Settle, &[pushed]);
        assert!(matches!(ev.as_slice(), [SyncEvent::Woken { .. }]));
        let t = s.get(&id).unwrap();
        assert_eq!(t.rung(), Rung::Active);
        assert!(t.unseen);
    }

    #[test]
    fn a_review_requested_again_wakes_an_archived_task() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        let mut out = remote(1, RemoteState::Open, t0());
        out.r.in_queries = false;
        s.apply_remote(OnClose::Settle, &[out]);
        s.archive(&id).unwrap();
        let ev = s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        assert!(matches!(ev.as_slice(), [SyncEvent::Requested { .. }]));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn closed_remote_item_settles_an_active_task() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.set_pinned(&id, true).unwrap();
        let ev = s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Merged, t0())]);
        assert!(matches!(
            ev.as_slice(),
            [SyncEvent::Closed { merged: true, .. }]
        ));
        let t = s.get(&id).unwrap();
        assert_eq!(t.rung(), Rung::Settled);
        assert!(!t.pinned);
    }

    #[test]
    fn remote_activity_wakes_settled_and_archived_tasks() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.settle(&id).unwrap();
        let ev = s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::minutes(1))],
        );
        assert!(matches!(ev.as_slice(), [SyncEvent::Woken { .. }]));
        let t = s.get(&id).unwrap();
        assert_eq!(t.rung(), Rung::Active);

        // Same timestamp: no wake.
        s.settle(&id).unwrap();
        let ev = s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::minutes(1))],
        );
        assert!(ev.is_empty());
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Settled);

        // An archived task comes back when the item is reopened.
        s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Closed, t0() + Duration::minutes(2))],
        );
        s.archive(&id).unwrap();
        let ev = s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::minutes(3))],
        );
        assert!(matches!(
            ev.as_slice(),
            [SyncEvent::ReopenedRemotely { .. }]
        ));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn remotely_reopened_item_becomes_active_again() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Closed, t0())]);
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Settled);
        let ev = s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::minutes(1))],
        );
        assert!(matches!(
            ev.as_slice(),
            [SyncEvent::ReopenedRemotely { .. }]
        ));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn remote_reopen_keeps_the_pin_of_a_task_reopened_by_hand() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Closed, t0())]);
        s.reopen(&id).unwrap();
        s.set_pinned(&id, true).unwrap();
        let ev = s.apply_remote(
            OnClose::Settle,
            &[remote(1, RemoteState::Open, t0() + Duration::minutes(1))],
        );
        assert!(matches!(
            ev.as_slice(),
            [SyncEvent::ReopenedRemotely { .. }]
        ));
        let t = s.get(&id).unwrap();
        assert!(t.pinned);
        assert_eq!(t.rung(), Rung::Active);
        assert!(t.unseen);
    }

    #[test]
    fn unseen_only_counts_active_tasks_and_clears_on_settle() {
        let mut s = Store::in_memory();
        s.apply_remote(
            OnClose::Settle,
            &[
                remote(1, RemoteState::Open, t0()),
                remote(2, RemoteState::Open, t0()),
            ],
        );
        assert_eq!(s.count_unseen(), 2);
        let a = s.tasks()[0].id.clone();
        s.settle(&a).unwrap();
        assert!(!s.get(&a).unwrap().unseen);
        assert_eq!(s.count_unseen(), 1);
        s.apply_remote(OnClose::Settle, &[remote(2, RemoteState::Merged, t0())]);
        assert_eq!(s.count_unseen(), 0);
    }

    fn dropped(number: u64, updated: DateTime<Utc>) -> RemoteItem {
        let mut item = remote(number, RemoteState::Open, updated);
        item.r.in_queries = false;
        item
    }

    #[test]
    fn leaving_the_queries_settles_an_active_task_once() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        // You reviewed it: the review request is gone, the PR is still open.
        let ev = s.apply_remote(OnClose::Settle, &[dropped(1, t0())]);
        assert!(matches!(ev.as_slice(), [SyncEvent::Released { .. }]));
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Settled);
        // Still out of the queries, no activity: nothing happens.
        assert!(
            s.apply_remote(OnClose::Settle, &[dropped(1, t0())])
                .is_empty()
        );
        // The author pushes: it wakes, and stays awake on the next poll.
        let ev = s.apply_remote(OnClose::Settle, &[dropped(1, t0() + Duration::minutes(1))]);
        assert!(matches!(ev.as_slice(), [SyncEvent::Woken { .. }]));
        assert!(
            s.apply_remote(OnClose::Settle, &[dropped(1, t0() + Duration::minutes(1))])
                .is_empty()
        );
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn re_entering_the_queries_wakes_a_settled_task() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.apply_remote(OnClose::Settle, &[dropped(1, t0())]);
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Settled);
        // Review requested again.
        let ev = s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        assert!(matches!(ev.as_slice(), [SyncEvent::Requested { .. }]));
        let t = s.get(&id).unwrap();
        assert_eq!(t.rung(), Rung::Active);
        assert!(t.unseen);
    }

    #[test]
    fn leaving_the_queries_leaves_pinned_tasks_alone() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.set_pinned(&id, true).unwrap();
        assert!(
            s.apply_remote(OnClose::Settle, &[dropped(1, t0())])
                .is_empty()
        );
        assert_eq!(s.get(&id).unwrap().rung(), Rung::Active);
        assert!(s.get(&id).unwrap().pinned);
    }

    #[test]
    fn stale_tasks_are_the_unpinned_ones_without_recent_activity() {
        let mut s = Store::in_memory();
        let now = Utc::now();
        let cutoff = now - Duration::days(365);
        let old = now - Duration::days(400);
        s.apply_remote(
            OnClose::Settle,
            &[
                remote(1, RemoteState::Open, old),
                remote(2, RemoteState::Open, now - Duration::days(1)),
                remote(3, RemoteState::Open, old),
                remote(4, RemoteState::Open, old),
            ],
        );
        let ids: Vec<String> = s.tasks().iter().map(|t| t.id.clone()).collect();
        s.set_pinned(&ids[2], true).unwrap();
        s.archive(&ids[3]).unwrap();
        let fresh_manual = s.add_manual("today", "", None);
        let stale = s.stale_ids(cutoff);
        assert_eq!(stale, vec![ids[0].clone()]);
        assert_eq!(s.archive_many(&stale), 1);
        assert_eq!(s.get(&ids[0]).unwrap().rung(), Rung::Archived);
        assert_eq!(s.get(&ids[1]).unwrap().rung(), Rung::Active);
        assert!(s.get(&ids[2]).unwrap().pinned);
        assert_eq!(s.get(&fresh_manual).unwrap().rung(), Rung::Active);
        // Archiving the same set again changes nothing.
        assert_eq!(s.archive_many(&stale), 0);
    }

    #[test]
    fn on_close_archive_sends_closed_items_to_the_archive() {
        let mut s = Store::in_memory();
        s.apply_remote(
            OnClose::Archive,
            &[
                remote(1, RemoteState::Open, t0()),
                remote(2, RemoteState::Open, t0()),
            ],
        );
        let ids: Vec<String> = s.tasks().iter().map(|t| t.id.clone()).collect();
        s.set_pinned(&ids[0], true).unwrap();
        s.settle(&ids[1]).unwrap();
        let ev = s.apply_remote(
            OnClose::Archive,
            &[
                remote(1, RemoteState::Merged, t0()),
                remote(2, RemoteState::Closed, t0()),
            ],
        );
        assert_eq!(ev.len(), 2);
        for id in &ids {
            let t = s.get(id).unwrap();
            assert_eq!(t.rung(), Rung::Archived);
            assert!(!t.pinned);
        }
        // Reopened remotely: back to Active, like any archived task.
        s.apply_remote(
            OnClose::Archive,
            &[remote(2, RemoteState::Open, t0() + Duration::minutes(1))],
        );
        assert_eq!(s.get(&ids[1]).unwrap().rung(), Rung::Active);
    }

    #[test]
    fn untracked_closed_items_are_ignored() {
        let mut s = Store::in_memory();
        let ev = s.apply_remote(OnClose::Settle, &[remote(9, RemoteState::Closed, t0())]);
        assert!(ev.is_empty());
        assert!(s.tasks().is_empty());
    }

    #[test]
    fn user_rename_survives_sync_until_remote_title_changes() {
        let mut s = Store::in_memory();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let id = s.tasks()[0].id.clone();
        s.rename(&id, "My name").unwrap();
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        assert_eq!(s.get(&id).unwrap().title, "My name");
        let mut item = remote(1, RemoteState::Open, t0());
        item.title = "Renamed upstream".into();
        s.apply_remote(OnClose::Settle, &[item]);
        assert_eq!(s.get(&id).unwrap().title, "My name");
        assert_eq!(
            s.get(&id).unwrap().remote_title.as_deref(),
            Some("Renamed upstream")
        );
    }

    #[test]
    fn roundtrips_through_json() {
        let mut s = Store::in_memory();
        s.add_manual("m", "notes", Some("https://example.com".into()));
        s.apply_remote(OnClose::Settle, &[remote(1, RemoteState::Open, t0())]);
        let text = serde_json::to_string(&s.data).unwrap();
        let back: StoreData = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tasks, s.data.tasks);
    }
}
