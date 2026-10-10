//! Running game sessions.
//!
//! Every launch goes through a supervisor: either the foreground `gameshelf
//! run`, or a detached `gameshelf supervise` process started by the TUI or
//! `run --detach`. The supervisor registers the session in a small state file,
//! runs umu-run, records playtime when it exits, and removes the file. Any
//! gameshelf process can therefore tell which games are running, regardless of
//! which process started them, and stop them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::launch;
use crate::library::Library;
use crate::util::{format_duration, now};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub library: PathBuf,
    pub dir: String,
    /// The supervisor's pid.
    pub pid: u32,
    /// The supervisor's start time from `/proc/<pid>/stat`, so a recycled
    /// pid isn't mistaken for the session.
    pub proc_start: u64,
    /// Unix time the game was launched.
    pub started: u64,
}

fn sessions_dir() -> PathBuf {
    // The runtime dir is cleared on reboot, which is exactly the lifetime we want.
    dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("gameshelf")
        .join("sessions")
}

fn library_key(lib: &Library) -> String {
    let root = fs::canonicalize(&lib.root).unwrap_or_else(|_| lib.root.clone());
    hex::encode(&Sha256::digest(root.as_os_str().as_encoded_bytes())[..6])
}

fn session_path(lib: &Library, dir: &str) -> PathBuf {
    let safe: String =
        dir.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    // The hash keeps games with similar sanitized names apart.
    let dir_hash = hex::encode(&Sha256::digest(dir.as_bytes())[..3]);
    sessions_dir().join(format!("{}-{safe}-{dir_hash}.json", library_key(lib)))
}

/// `(ppid, starttime)` from `/proc/<pid>/stat`.
fn proc_stat(pid: u32) -> Option<(u32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume
    // after the last ')'. Field 3 (state) is index 0 here.
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start))
}

fn is_alive(pid: u32, proc_start: u64) -> bool {
    proc_stat(pid).is_some_and(|(_, start)| start == proc_start)
}

impl Session {
    pub fn is_alive(&self) -> bool {
        is_alive(self.pid, self.proc_start)
    }
}

/// Running sessions in this library, keyed by game directory. Stale files
/// (the supervisor died without cleaning up) are removed.
pub fn active(lib: &Library) -> HashMap<String, Session> {
    let prefix = format!("{}-", library_key(lib));
    let mut out = HashMap::new();
    for entry in fs::read_dir(sessions_dir()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let session: Option<Session> =
            fs::read_to_string(entry.path()).ok().and_then(|t| serde_json::from_str(&t).ok());
        match session {
            Some(s) if s.is_alive() => {
                out.insert(s.dir.clone(), s);
            }
            _ => {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    out
}

pub fn find(lib: &Library, dir: &str) -> Option<Session> {
    let path = session_path(lib, dir);
    let s: Session = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    if s.is_alive() {
        Some(s)
    } else {
        let _ = fs::remove_file(path);
        None
    }
}

/// Removes the session file when the supervisor is done.
struct Registration(PathBuf);

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Claim the session slot for `dir` atomically, so two launches of the same
/// game can't both proceed.
fn register(lib: &Library, dir: &str, started: u64) -> Result<Registration> {
    let path = session_path(lib, dir);
    fs::create_dir_all(path.parent().unwrap())?;
    let pid = std::process::id();
    let session = Session {
        library: lib.root.clone(),
        dir: dir.to_string(),
        pid,
        proc_start: proc_stat(pid).map(|(_, s)| s).unwrap_or(0),
        started,
    };
    // Write the complete file first, then link it into place: the link is
    // atomic and fails if the slot is taken, so readers never see a partial
    // file and two supervisors can't both register.
    let tmp = path.with_extension(format!("json.tmp-{pid}"));
    fs::write(&tmp, serde_json::to_string(&session)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    let result = (|| {
        for _ in 0..2 {
            match fs::hard_link(&tmp, &path) {
                Ok(()) => return Ok(Registration(path.clone())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(other) = find(lib, dir) {
                        bail!("{dir} is already running (pid {})", other.pid);
                    }
                    // `find` removed the stale file; try again.
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("couldn't register session for {dir}")
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Run a game to completion as its supervisor: register the session, run
/// umu-run with inherited stdio, record playtime. Messages go to stderr,
/// which is the game's log for detached launches.
pub fn run_supervised(lib: &Library, dir: &str, extra_args: &[String]) -> Result<ExitStatus> {
    let lock = lib.load()?;
    let plan = launch::plan(lib, &lock, dir, extra_args)?;
    let name = lock.games[dir].display_name(dir).to_string();
    let started = now();
    let _registration = register(lib, dir, started)?;

    eprintln!("gameshelf: launching {name} with {}", plan.runtime);
    match plan.umu_path() {
        Some(p) => eprintln!("gameshelf: using {}", p.display()),
        None => eprintln!("gameshelf: {} not found on PATH", plan.umu_run),
    }
    if let Some(w) = plan.environment_warning() {
        eprintln!("gameshelf: warning: {w}");
    }
    eprintln!("gameshelf: {}", plan.shell_line());

    launch::record_start(lib, dir)?;
    let status = plan.run_foreground();
    let secs = launch::record_end(lib, dir, started)?;
    let status = status?;
    eprintln!("gameshelf: {name} exited ({status}) after {}", format_duration(secs));
    Ok(status)
}

/// Start a detached supervisor (`gameshelf supervise`) for `dir`, with its
/// output going to the game's log. Returns the supervisor pid and the log path.
pub fn spawn_detached(lib: &Library, dir: &str, extra_args: &[String]) -> Result<(u32, PathBuf)> {
    // Validate here so problems show up in the caller, not just in the log.
    let lock = lib.load()?;
    launch::plan(lib, &lock, dir, extra_args)?;
    if let Some(s) = find(lib, dir) {
        bail!("{dir} is already running (pid {})", s.pid);
    }

    let log = launch::log_path(dir);
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let out = fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let exe = std::env::current_exe().context("locating the gameshelf executable")?;
    let child = Command::new(exe)
        .arg("--library")
        .arg(&lib.root)
        .arg("supervise")
        .arg(dir)
        .arg("--")
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        // Own process group: no terminal signals, survives the TUI exiting.
        .process_group(0)
        .spawn()
        .context("starting the gameshelf supervisor")?;
    Ok((child.id(), log))
}

/// All descendants of `root`, from a snapshot of `/proc`.
fn descendants(root: u32) -> Vec<(u32, u64)> {
    let mut children: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some((ppid, start)) = proc_stat(pid) {
            children.entry(ppid).or_default().push((pid, start));
        }
    }
    let mut out = Vec::new();
    let mut queue = vec![root];
    let mut seen = HashSet::from([root]);
    while let Some(p) = queue.pop() {
        for &(child, start) in children.get(&p).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(child) {
                out.push((child, start));
                queue.push(child);
            }
        }
    }
    out
}

fn signal(procs: &[(u32, u64)], sig: libc::c_int) {
    for &(pid, start) in procs {
        // Re-check identity right before signalling in case the pid was reused.
        if is_alive(pid, start) {
            // SAFETY: plain kill(2) on a pid we just verified.
            unsafe {
                libc::kill(pid as libc::pid_t, sig);
            }
        }
    }
}

/// Stop a running game: SIGTERM everything the supervisor started (umu-run,
/// the container, wine processes and the game), then SIGKILL whatever is
/// left after `grace`. The supervisor itself stays up to record playtime.
pub fn stop(lib: &Library, dir: &str, grace: Duration) -> Result<()> {
    let session = find(lib, dir).with_context(|| format!("{dir} is not running"))?;
    let initial = descendants(session.pid);
    signal(&initial, libc::SIGTERM);

    let deadline = Instant::now() + grace;
    loop {
        let mut left: Vec<(u32, u64)> = descendants(session.pid);
        left.extend(initial.iter().copied().filter(|&(p, s)| is_alive(p, s)));
        left.sort_unstable();
        left.dedup();
        if left.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            signal(&left, libc::SIGKILL);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Give the supervisor a moment to record playtime and unregister.
    let deadline = Instant::now() + Duration::from_secs(3);
    while session.is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_proc_stat() {
        let pid = std::process::id();
        let (ppid, start) = proc_stat(pid).unwrap();
        assert!(ppid > 0 && start > 0);
        assert!(is_alive(pid, start));
        assert!(!is_alive(pid, start + 1), "different start time = different process");
    }

    #[test]
    fn finds_and_signals_descendants() {
        let mut child = Command::new("sh").args(["-c", "sleep 30 & sleep 30; wait"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let desc = descendants(child.id());
        assert_eq!(desc.len(), 2, "two sleeps: {desc:?}");
        signal(&desc, libc::SIGTERM);
        let status = child.wait().unwrap();
        assert!(status.success(), "sh exits once its sleeps are gone: {status}");
        assert!(desc.iter().all(|&(p, s)| !is_alive(p, s)));
    }
}
