//! Namespace markers exist only for executing turns, never for parked harnesses.
//! The filename carries process identity so crash recovery cannot mistake a reused
//! PID, a reboot, or another live Crew engine for our abandoned work.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use uuid::Uuid;

const PREFIX: &str = "crew-turn-v1-";

pub(crate) struct NamespaceTasks {
    directory: Option<PathBuf>,
    owner: io::Result<Owner>,
    engine: Uuid,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stopped: bool,
    turns: HashMap<String, PathBuf>,
}

struct Owner {
    boot: Uuid,
    pid: u32,
    start: u64,
}

impl NamespaceTasks {
    pub(crate) fn detect() -> Self {
        let directory = Path::new("/.namespace/tasks");
        Self::new(crate::devbox::namespace_host().then(|| directory.to_path_buf()))
    }

    pub(crate) fn required(&self) -> bool {
        self.directory.is_some()
    }

    fn new(directory: Option<PathBuf>) -> Self {
        let owner = if directory.is_some() {
            current_owner()
        } else {
            Err(io::Error::other("not a Namespace Devbox"))
        };
        if let (Some(directory), Ok(owner)) = (&directory, &owner) {
            if let Err(error) = sweep_stale(directory, owner.boot, process_start) {
                tracing::warn!(%error, "could not inspect stale Crew Namespace task markers");
            }
        }
        Self {
            directory,
            owner,
            engine: Uuid::new_v4(),
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn start_turn(&self, chat: &str, run: &str) -> io::Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let mut state = lock(&self.state);
        if state.stopped {
            return Err(io::Error::other("Crew is shutting down"));
        }
        if state.turns.contains_key(run) {
            return Ok(());
        }
        let owner = self
            .owner
            .as_ref()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let path = directory.join(format!(
            "{PREFIX}{}-{}-{}-{}-{}",
            owner.boot.simple(),
            owner.pid,
            owner.start,
            self.engine.simple(),
            Uuid::new_v4().simple(),
        ));
        // create_new never truncates somebody else's task; identity is complete
        // before creation, so even a crash during the write can be swept safely.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        state.turns.insert(run.to_owned(), path);
        if let Err(error) = writeln!(
            file,
            "{}",
            serde_json::json!({ "session": chat, "run": run })
        ) {
            // Retain failed removals for shutdown's bounded second attempt.
            remove_turn(&mut state, run);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn finish_turn(&self, run: &str) {
        remove_turn(&mut lock(&self.state), run);
    }

    pub(crate) fn shutdown(&self) {
        let mut state = lock(&self.state);
        state.stopped = true;
        state.turns.retain(|_, path| !remove_marker(path));
    }
}

impl Drop for NamespaceTasks {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn remove_turn(state: &mut State, run: &str) {
    if let Some(path) = state.turns.get(run)
        && remove_marker(path)
    {
        state.turns.remove(run);
    }
}

fn remove_marker(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "could not remove Crew Namespace task marker; Devbox may remain active");
            false
        }
    }
}

fn current_owner() -> io::Result<Owner> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Owner {
        boot: Uuid::parse_str(boot.trim()).map_err(io::Error::other)?,
        pid: std::process::id(),
        start: process_start(std::process::id())?,
    })
}

fn process_start(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // comm can contain spaces and closing parentheses. Field 22 is starttime;
    // the first field after the final ')' is field 3 (state).
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|start| start.parse().ok())
        .ok_or_else(|| io::Error::other("invalid process start time"))
}

fn marker_owner(name: &str) -> Option<Owner> {
    let mut fields = name.strip_prefix(PREFIX)?.split('-');
    let boot = fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    let start = fields.next()?.parse().ok()?;
    let engine = fields.next()?;
    let turn = fields.next()?;
    if fields.next().is_some() || [boot, engine, turn].iter().any(|value| value.len() != 32) {
        return None;
    }
    Uuid::parse_str(engine).ok()?;
    Uuid::parse_str(turn).ok()?;
    Some(Owner {
        boot: Uuid::parse_str(boot).ok()?,
        pid,
        start,
    })
}

fn sweep_stale(
    directory: &Path,
    boot: Uuid,
    process_start: impl Fn(u32) -> io::Result<u64>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(owner) = name.to_str().and_then(marker_owner) else {
            continue;
        };
        // Never follow links or recursively delete directories in this shared
        // directory. Unknown ownership/permission errors are not proof of death.
        if !entry.file_type()?.is_file() {
            continue;
        }
        let dead = owner.boot != boot
            || match process_start(owner.pid) {
                Ok(start) => start != owner.start,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
        if dead {
            remove_marker(&entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tasks_in(directory: &Path) -> NamespaceTasks {
        NamespaceTasks {
            directory: Some(directory.to_path_buf()),
            owner: Ok(Owner {
                boot: Uuid::nil(),
                pid: 123,
                start: 456,
            }),
            engine: Uuid::new_v4(),
            state: Mutex::new(State::default()),
        }
    }

    #[test]
    fn turns_and_engines_remove_only_their_own_markers() {
        let directory = tempfile::tempdir().unwrap();
        let unrelated = directory.path().join("build-in-progress");
        fs::write(&unrelated, "").unwrap();
        let first = tasks_in(directory.path());
        let second = tasks_in(directory.path());
        first.start_turn("chat-a", "run-a").unwrap();
        first.start_turn("chat-a", "run-a").unwrap();
        first.start_turn("chat-b", "run-b").unwrap();
        second.start_turn("chat-a", "run-a").unwrap();
        let first_a = lock(&first.state).turns["run-a"].clone();
        let first_b = lock(&first.state).turns["run-b"].clone();
        let second_a = lock(&second.state).turns["run-a"].clone();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 4);
        first.finish_turn("run-a");
        assert!(!first_a.exists());
        assert!(first_b.exists() && second_a.exists() && unrelated.exists());
        first.start_turn("chat-a", "run-a").unwrap();
        assert_ne!(lock(&first.state).turns["run-a"], first_a);
        first.shutdown();
        assert!(!first_b.exists());
        assert!(first.start_turn("chat-c", "run-c").is_err());
        assert!(second_a.exists() && unrelated.exists());
        drop(second);
        assert!(!second_a.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn stale_sweep_preserves_live_unknown_and_unrelated_tasks() {
        let directory = tempfile::tempdir().unwrap();
        let boot = Uuid::new_v4();
        let marker = |owner_boot: Uuid, pid, start| {
            let path = directory.path().join(format!(
                "{PREFIX}{}-{pid}-{start}-{}-{}",
                owner_boot.simple(),
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple(),
            ));
            fs::write(&path, "").unwrap();
            path
        };
        let live = marker(boot, 1, 100);
        let reused_pid = marker(boot, 1, 99);
        let dead = marker(boot, 2, 100);
        let unknown = marker(boot, 3, 100);
        let old_boot = marker(Uuid::new_v4(), 1, 100);
        let unrelated = directory.path().join("crew-turn-v1-not-owned");
        fs::write(&unrelated, "").unwrap();
        sweep_stale(directory.path(), boot, |pid| match pid {
            1 => Ok(100),
            2 => Err(io::ErrorKind::NotFound.into()),
            _ => Err(io::ErrorKind::PermissionDenied.into()),
        })
        .unwrap();
        assert!(live.exists() && unknown.exists() && unrelated.exists());
        assert!(!reused_pid.exists() && !dead.exists() && !old_boot.exists());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
