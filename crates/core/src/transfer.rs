//! Transfer queue of one connection (SPEC §6.3): parallel uploads/downloads,
//! pause/resume by offset, retry, conflict handling.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{remote::Remote, terminal::Waker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

/// What to do when the target already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    Ask,
    Overwrite,
    Skip,
    Rename,
    /// Continue from the existing partial file.
    Resume,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Queued,
    Running,
    Paused,
    Done,
    Skipped,
    Failed(String),
    /// Target exists with this size; waiting for the user's choice.
    Conflict(u64),
}

pub struct Transfer {
    pub id: u64,
    pub dir: Dir,
    pub local: PathBuf,
    pub remote: String,
    pub total: u64,
    pub done: Arc<AtomicU64>,
    pub state: Arc<Mutex<State>>,
    pub on_conflict: OnConflict,
    cancel: Arc<AtomicBool>,
}

impl Transfer {
    pub fn state(&self) -> State {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(State::Failed("poisoned".into()))
    }

    fn set(&self, s: State) {
        if let Ok(mut st) = self.state.lock() {
            *st = s;
        }
    }

    pub fn name(&self) -> String {
        match self.dir {
            Dir::Up => self
                .local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Dir::Down => self.remote.rsplit('/').next().unwrap_or(&self.remote).to_owned(),
        }
    }
}

#[derive(Default)]
pub struct Queue {
    pub items: Vec<Transfer>,
    next_id: u64,
}

/// `name (1).ext`, `name (2).ext`, … until `exists` says no.
pub fn free_name(name: &str, exists: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !exists(c))
        .unwrap_or_default()
}

impl Queue {
    pub fn push(&mut self, dir: Dir, local: PathBuf, remote: String, total: u64, on_conflict: OnConflict) {
        self.next_id += 1;
        self.items.push(Transfer {
            id: self.next_id,
            dir,
            local,
            remote,
            total,
            done: Arc::new(AtomicU64::new(0)),
            state: Arc::new(Mutex::new(State::Queued)),
            on_conflict,
            cancel: Arc::new(AtomicBool::new(false)),
        });
    }

    pub fn running(&self) -> usize {
        self.items.iter().filter(|t| t.state() == State::Running).count()
    }

    /// Starts queued transfers up to `parallel` at a time.
    pub fn pump(&mut self, remote: &Arc<Remote>, parallel: usize, rt: &tokio::runtime::Handle, wake: &Waker) {
        let mut free = parallel.max(1).saturating_sub(self.running());
        for t in self.items.iter_mut().filter(|t| t.state() == State::Queued) {
            if free == 0 {
                break;
            }
            free -= 1;
            t.set(State::Running);
            t.cancel.store(false, Ordering::Relaxed);
            let job = Job {
                dir: t.dir,
                local: t.local.clone(),
                remote: t.remote.clone(),
                total: t.total,
                on_conflict: t.on_conflict,
                done: t.done.clone(),
                state: t.state.clone(),
                cancel: t.cancel.clone(),
            };
            let (remote, wake) = (remote.clone(), wake.clone());
            rt.spawn(async move {
                let result = job.run(&remote).await;
                if let Ok(mut s) = job.state.lock() {
                    *s = match result {
                        Ok(s) => s,
                        Err(_) if job.cancel.load(Ordering::Relaxed) => State::Paused,
                        Err(e) => State::Failed(e),
                    };
                }
                wake();
            });
        }
    }

    pub fn pause(&self, id: u64) {
        if let Some(t) = self.items.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Resume a paused/failed transfer from its partial target.
    pub fn resume(&mut self, id: u64) {
        if let Some(t) = self.items.iter_mut().find(|t| t.id == id) {
            t.on_conflict = OnConflict::Resume;
            t.set(State::Queued);
        }
    }

    pub fn retry(&mut self, id: u64) {
        self.resume(id);
    }

    pub fn cancel(&mut self, id: u64) {
        if let Some(i) = self.items.iter().position(|t| t.id == id) {
            self.items[i].cancel.store(true, Ordering::Relaxed);
            self.items.remove(i);
        }
    }

    /// Answer a conflict; `all` applies it to every waiting conflict.
    pub fn decide(&mut self, id: u64, choice: OnConflict, all: bool) {
        for t in &mut self.items {
            if (t.id == id || all) && matches!(t.state(), State::Conflict(_)) {
                t.on_conflict = choice;
                t.set(if choice == OnConflict::Skip {
                    State::Skipped
                } else {
                    State::Queued
                });
            }
        }
    }

    pub fn clear_done(&mut self) {
        self.items
            .retain(|t| !matches!(t.state(), State::Done | State::Skipped));
    }

    pub fn cancel_all(&mut self) {
        for t in &self.items {
            t.cancel.store(true, Ordering::Relaxed);
        }
        self.items.clear();
    }
}

struct Job {
    dir: Dir,
    local: PathBuf,
    remote: String,
    total: u64,
    on_conflict: OnConflict,
    done: Arc<AtomicU64>,
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    async fn run(&self, remote: &Remote) -> Result<State, String> {
        let existing = match self.dir {
            Dir::Down => tokio::fs::metadata(&self.local).await.ok().map(|m| m.len()),
            Dir::Up => remote.size(&self.remote).await,
        };
        let mut local = self.local.clone();
        let mut target = self.remote.clone();
        let offset = match (existing, self.on_conflict) {
            (None, _) | (Some(_), OnConflict::Overwrite) => 0,
            (Some(size), OnConflict::Ask) => return Ok(State::Conflict(size)),
            (Some(_), OnConflict::Skip) => return Ok(State::Skipped),
            (Some(size), OnConflict::Resume) if size < self.total => size,
            (Some(_), OnConflict::Resume) => return Ok(State::Done),
            (Some(_), OnConflict::Rename) => {
                match self.dir {
                    Dir::Down => {
                        let dir = local.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                        let name = local
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        local = dir.join(free_name(&name, |c| dir.join(c).exists()));
                    }
                    Dir::Up => {
                        let names: Vec<String> = remote
                            .list(&crate::remote::parent(&target))
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|e| e.name)
                            .collect();
                        let name = target.rsplit('/').next().unwrap_or(&target).to_owned();
                        let new = free_name(&name, |c| names.iter().any(|n| n == c));
                        target = crate::remote::join(&crate::remote::parent(&target), &new);
                    }
                }
                0
            }
        };
        match self.dir {
            Dir::Down => {
                remote
                    .download(&target, &local, offset, &self.done, &self.cancel)
                    .await?
            }
            Dir::Up => remote.upload(&local, &target, offset, &self.done, &self.cancel).await?,
        }
        Ok(State::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_names_and_decisions() {
        let taken = ["a.txt", "a (1).txt"];
        assert_eq!(free_name("a.txt", |c| taken.contains(&c)), "a (2).txt");
        assert_eq!(free_name(".bashrc", |_| false), ".bashrc (1)");

        let mut q = Queue::default();
        q.push(Dir::Down, "/tmp/x".into(), "/x".into(), 10, OnConflict::Ask);
        q.push(Dir::Down, "/tmp/y".into(), "/y".into(), 10, OnConflict::Ask);
        for t in &q.items {
            t.set(State::Conflict(5));
        }
        q.decide(1, OnConflict::Resume, false);
        assert_eq!(q.items[0].state(), State::Queued);
        assert_eq!(q.items[1].state(), State::Conflict(5));
        q.decide(2, OnConflict::Skip, true);
        assert_eq!(q.items[1].state(), State::Skipped);
        q.clear_done();
        assert_eq!(q.items.len(), 1);
    }
}
