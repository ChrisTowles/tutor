//! Is their screen locked, asleep or idle? Pieces arrive from different
//! places (logind, Wayland); Dad gets the whole picture whenever it changes.

use std::{process::Command, sync::Mutex, thread, time::Duration};

use tutor_proto::{FromStation, Presence};

use crate::out::Out;

pub struct Reporter {
    state: Mutex<Presence>,
    out: Out,
}

impl Reporter {
    pub fn new(out: Out) -> Self {
        Self { state: Mutex::new(Presence::default()), out }
    }

    pub fn update(&self, change: impl FnOnce(&mut Presence)) {
        let mut state = self.state.lock().unwrap();
        let before = *state;
        change(&mut state);
        if *state != before {
            self.out.send(FromStation::Presence(*state));
        }
    }

    /// Resend the current state (a new connection hasn't seen it).
    pub fn announce(&self) {
        self.out.send(FromStation::Presence(*self.state.lock().unwrap()));
    }
}

/// Poll logind's LockedHint for their seat session every few seconds.
pub fn watch_lock(reporter: std::sync::Arc<Reporter>) {
    thread::Builder::new()
        .name("lock".into())
        .spawn(move || {
            loop {
                if let Some(locked) = locked() {
                    reporter.update(|p| p.locked = locked);
                }
                thread::sleep(Duration::from_secs(3));
            }
        })
        .expect("spawn lock watcher");
}

fn locked() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let list = Command::new("loginctl").args(["list-sessions", "--no-legend"]).output().ok()?;
    let sessions = String::from_utf8_lossy(&list.stdout).into_owned();
    // Columns: SESSION UID USER SEAT TTY ...
    let id = sessions.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 3 && f[2] == user && f[3].starts_with("seat")).then(|| f[0].to_string())
    })?;
    let out = Command::new("loginctl").args(["show-session", &id, "-p", "LockedHint", "--value"]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "yes")
}
