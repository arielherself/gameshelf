//! A game library directory: every top-level directory is a game, metadata
//! lives in `gameshelf.lock`, prefixes in `.proton-prefixes`, runtimes in
//! `.proton-runtimes`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::lockfile::{Game, LockFile, MatchState, SteamInfo};
use crate::names;
use crate::steam::SteamClient;
use crate::util::{expand_tilde, now};

pub const LOCK_NAME: &str = "gameshelf.lock";
pub const PREFIXES_DIR: &str = ".proton-prefixes";
pub const RUNTIMES_DIR: &str = ".proton-runtimes";
const GUARD_NAME: &str = ".gameshelf.flock";

#[derive(Debug, Clone)]
pub struct Library {
    pub root: PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScanReport {
    pub added: Vec<String>,
    pub missing: Vec<String>,
}

impl Library {
    pub fn open(root: PathBuf, create: bool) -> Result<Self> {
        if !root.is_dir() {
            if create {
                fs::create_dir_all(&root).with_context(|| format!("creating library {}", root.display()))?;
            } else {
                bail!("library directory {} does not exist (create it or pass --library)", root.display());
            }
        }
        Ok(Self { root })
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_NAME)
    }

    pub fn prefixes_dir(&self) -> PathBuf {
        self.root.join(PREFIXES_DIR)
    }

    pub fn runtimes_dir(&self) -> PathBuf {
        self.root.join(RUNTIMES_DIR)
    }

    pub fn game_dir(&self, dir: &str) -> PathBuf {
        self.root.join(dir)
    }

    pub fn load(&self) -> Result<LockFile> {
        LockFile::load(&self.lock_path())
    }

    /// Read-modify-write the lock file under an exclusive advisory lock.
    pub fn update<T>(&self, f: impl FnOnce(&mut LockFile) -> Result<T>) -> Result<(LockFile, T)> {
        let guard = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(GUARD_NAME))
            .context("opening lock guard")?;
        guard.lock().context("locking gameshelf.lock")?;
        let mut lock = self.load()?;
        let before = lock.clone();
        let out = f(&mut lock)?;
        if lock != before {
            lock.save(&self.lock_path())?;
        }
        Ok((lock, out))
        // `guard` is dropped (and unlocked) here.
    }

    /// Game directories currently on disk (non-hidden top-level directories).
    pub fn game_dirs(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root).with_context(|| format!("reading {}", self.root.display()))? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Register new directories and fill in missing executables. No network.
    pub fn scan(&self) -> Result<(LockFile, ScanReport)> {
        let dirs = self.game_dirs()?;
        let prefixes_dir = self.prefixes_dir();
        let root = self.root.clone();
        self.update(|lock| {
            let mut report = ScanReport::default();
            for dir in &dirs {
                if let Some(game) = lock.games.get_mut(dir) {
                    if game.executable.is_none() {
                        game.executable = find_executables(&root.join(dir), dir).into_iter().next();
                    }
                    continue;
                }
                let prefix = choose_prefix(dir, lock, &prefixes_dir);
                let game = Game {
                    added: now(),
                    executable: find_executables(&root.join(dir), dir).into_iter().next(),
                    prefix,
                    ..Default::default()
                };
                lock.games.insert(dir.clone(), game);
                report.added.push(dir.clone());
            }
            report.missing = lock.games.keys().filter(|k| !dirs.contains(k)).cloned().collect();
            Ok(report)
        })
    }

    pub fn prefix_path(&self, prefix: &str) -> PathBuf {
        if prefix.contains('/') { expand_tilde(prefix) } else { self.prefixes_dir().join(prefix) }
    }

    /// Existing prefixes (directory names inside `.proton-prefixes`).
    pub fn prefixes(&self) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(self.prefixes_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        out.sort_by_key(|s| s.to_lowercase());
        out
    }

    pub fn executable_path(&self, dir: &str, game: &Game) -> Option<PathBuf> {
        let exe = game.executable.as_deref()?;
        let p = expand_tilde(exe);
        Some(if p.is_absolute() { p } else { self.game_dir(dir).join(p) })
    }
}

/// Pick the prefix name for a newly discovered game.
///
/// Adopts an existing, unclaimed prefix named after the directory (exactly or
/// in snake_case) so libraries set up by hand keep their saves; otherwise a
/// fresh snake_case name that no other game uses.
pub fn choose_prefix(dir: &str, lock: &LockFile, prefixes_dir: &Path) -> String {
    let claimed = |name: &str| lock.games.values().any(|g| g.prefix == name);
    let snake = names::snake_case(dir);
    for candidate in [dir, snake.as_str()] {
        if prefixes_dir.join(candidate).is_dir() && !claimed(candidate) {
            return candidate.to_string();
        }
    }
    fresh_prefix(dir, lock, prefixes_dir)
}

/// A snake_case prefix name that neither exists on disk nor is used by any game.
pub fn fresh_prefix(dir: &str, lock: &LockFile, prefixes_dir: &Path) -> String {
    let snake = names::snake_case(dir);
    let taken =
        |name: &str| prefixes_dir.join(name).exists() || lock.games.values().any(|g| g.prefix == name);
    let mut name = snake.clone();
    let mut n = 2;
    while taken(&name) {
        name = format!("{snake}_{n}");
        n += 1;
    }
    name
}

/// File names that are installers, redistributables or helpers, never the game.
fn is_helper_exe(stem: &str) -> bool {
    let s = stem.to_lowercase();
    const PREFIXES: &[&str] = &[
        "unins",
        "setup",
        "install",
        "vcredist",
        "vc_redist",
        "dxsetup",
        "dxwebsetup",
        "directx",
        "dotnet",
        "ndp",
        "oalinst",
        "physx",
        "ue4prereq",
        "ueprereq",
        "easyanticheat",
        "eac",
        "battleye",
        "be_service",
        "notification_helper",
        "uploader",
        "cefprocess",
        "quicksfv",
        "dxcpl",
        "redist",
        "scriptinterpreter",
        "7z",
        "winrar",
    ];
    // Crash reporters are matched by their full names: plain "crash" would
    // also hide games like Crash Bandicoot or Crashday.
    const CONTAINS: &[&str] = &[
        "setup",
        "redist",
        "unins",
        "prereq",
        "crashreport",
        "crash_report",
        "crashhandler",
        "crashpad",
        "crashsender",
    ];
    PREFIXES.iter().any(|p| s.starts_with(p)) || CONTAINS.iter().any(|c| s.contains(c))
}

fn is_helper_dir(name: &str) -> bool {
    let s = name.to_lowercase();
    s.contains("redist")
        || s.contains("prereq")
        || matches!(
            s.as_str(),
            "directx"
                | "dotnet"
                | "installer"
                | "installers"
                | "support"
                | "_commonredist"
                | "easyanticheat"
                | "battleye"
                | "crashreporter"
                | "tools"
        )
}

/// Candidate `.exe` files in a game directory, best guess first, as paths
/// relative to the game directory.
pub fn find_executables(game_dir: &Path, dir_name: &str) -> Vec<String> {
    let mut found: Vec<(f64, String)> = Vec::new();
    walk(game_dir, game_dir, 0, &mut |rel, depth, path| {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if is_helper_exe(stem) {
            return;
        }
        // Shallow files win: a top-level launcher (e.g. a Steam emulator loader)
        // usually has to be started instead of the real binary below it.
        let mut score = 8.0 - 2.0 * depth as f64;
        score += 3.0 * names::match_score(dir_name, stem);
        // Unity games ship `<Name>.exe` next to `<Name>_Data`.
        if path.with_file_name(format!("{stem}_Data")).is_dir() {
            score += 1.0;
        }
        if let Ok(meta) = path.metadata() {
            score += (meta.len() as f64 / 1_000_000.0).ln_1p() * 0.15;
        }
        found.push((score, rel));
    });
    found.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, rel)| rel).collect()
}

const MAX_DEPTH: usize = 4;

fn walk(root: &Path, dir: &Path, depth: usize, f: &mut dyn FnMut(String, usize, &Path)) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth < MAX_DEPTH && !is_helper_dir(&name) && !name.starts_with('.') {
                walk(root, &path, depth + 1, f);
            }
        } else if name.to_lowercase().ends_with(".exe")
            && let Ok(rel) = path.strip_prefix(root)
        {
            f(rel.to_string_lossy().into_owned(), depth, &path);
        }
    }
}

/// Resolve a user-supplied game reference (directory name, Steam title, app
/// id or an unambiguous fragment) to a directory name.
pub fn resolve_game(lock: &LockFile, query: &str) -> Result<String> {
    if lock.games.contains_key(query) {
        return Ok(query.to_string());
    }
    let key = names::compare_key(query);
    let appid: Option<u32> = query.parse().ok();
    let exact: Vec<&String> = lock
        .games
        .iter()
        .filter(|(dir, g)| {
            names::compare_key(dir) == key
                || names::compare_key(g.display_name(dir)) == key
                || (appid.is_some() && g.appid == appid)
        })
        .map(|(d, _)| d)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    let partial: Vec<&String> = if key.is_empty() {
        Vec::new()
    } else {
        lock.games
            .iter()
            .filter(|(dir, g)| {
                names::compare_key(dir).contains(&key)
                    || names::compare_key(g.display_name(dir)).contains(&key)
            })
            .map(|(d, _)| d)
            .collect()
    };
    let candidates = if exact.is_empty() { partial } else { exact };
    match candidates.len() {
        0 => bail!("no game matches {query:?} (see `gameshelf list`)"),
        1 => Ok(candidates[0].clone()),
        _ => bail!(
            "{query:?} is ambiguous: {}",
            candidates.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Games that still need a Steam lookup.
pub fn pending_lookups(lock: &LockFile) -> Vec<String> {
    lock.games.iter().filter(|(_, g)| needs_lookup(g)).map(|(d, _)| d.clone()).collect()
}

fn needs_lookup(g: &Game) -> bool {
    match g.match_state {
        MatchState::Pending => true,
        MatchState::Matched | MatchState::Manual => g.appid.is_some() && g.steam.is_none(),
        MatchState::Unmatched => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LookupOutcome {
    Matched {
        appid: u32,
        name: String,
        score: f64,
    },
    Unmatched,
    /// Details fetched for an already-known app id.
    Refreshed,
    /// The game changed while we were looking it up; nothing applied.
    Skipped,
}

/// Look up one game on Steam and record the result. The network calls are
/// made without holding the lock; the result is only applied if the entry
/// wasn't changed in the meantime (e.g. the user set an app id).
pub fn lookup_game(lib: &Library, client: &SteamClient, dir: &str) -> Result<LookupOutcome> {
    let lock = lib.load()?;
    let Some(game) = lock.games.get(dir).cloned() else {
        return Ok(LookupOutcome::Skipped);
    };
    if !needs_lookup(&game) {
        return Ok(LookupOutcome::Skipped);
    }

    let (appid, outcome) = match (game.match_state, game.appid) {
        (MatchState::Pending, _) => {
            let exe_stem = game
                .executable
                .as_deref()
                .and_then(|e| Path::new(e).file_stem())
                .and_then(|s| s.to_str())
                .map(str::to_string);
            match client.auto_match(dir, exe_stem.as_deref())? {
                Some(m) => {
                    (Some(m.appid), LookupOutcome::Matched { appid: m.appid, name: m.name, score: m.score })
                }
                None => (None, LookupOutcome::Unmatched),
            }
        }
        (_, appid) => (appid, LookupOutcome::Refreshed),
    };
    let steam = match appid {
        Some(id) => client.details(id)?,
        None => None,
    };

    let (_, applied) = lib.update(|lock| {
        let Some(g) = lock.games.get_mut(dir) else { return Ok(false) };
        if g.match_state != game.match_state || g.appid != game.appid {
            return Ok(false);
        }
        match &outcome {
            LookupOutcome::Matched { appid, .. } => {
                g.match_state = MatchState::Matched;
                g.appid = Some(*appid);
            }
            LookupOutcome::Unmatched => g.match_state = MatchState::Unmatched,
            _ => {}
        }
        // A known app id without store details (delisted, region locked...)
        // keeps a stub so it isn't re-fetched on every start.
        g.steam = g
            .appid
            .map(|_| steam.clone().unwrap_or_else(|| SteamInfo { fetched_at: now(), ..Default::default() }));
        Ok(true)
    })?;
    Ok(if applied { outcome } else { LookupOutcome::Skipped })
}

/// Set (or clear) a game's app id by hand and fetch its store details.
///
/// Without a client (offline) the details are left empty and fetched by the
/// next online lookup. Returns the fetched details, `None` if Steam has no
/// store page for the id (or we're offline).
pub fn set_appid(
    lib: &Library,
    client: Option<&SteamClient>,
    dir: &str,
    appid: Option<u32>,
) -> Result<Option<SteamInfo>> {
    let steam = match (appid, client) {
        (Some(id), Some(c)) => c.details(id)?,
        _ => None,
    };
    lib.update(|lock| {
        let g = lock.games.get_mut(dir).with_context(|| format!("unknown game {dir:?}"))?;
        g.appid = appid;
        g.match_state = if appid.is_some() { MatchState::Manual } else { MatchState::Unmatched };
        g.steam = match (appid, client) {
            (Some(_), Some(_)) => {
                Some(steam.clone().unwrap_or_else(|| SteamInfo { fetched_at: now(), ..Default::default() }))
            }
            _ => None,
        };
        Ok(())
    })?;
    Ok(steam)
}

/// Forget the current match and search Steam again on the next lookup.
pub fn reset_match(lib: &Library, dir: &str) -> Result<()> {
    lib.update(|lock| {
        let g = lock.games.get_mut(dir).with_context(|| format!("unknown game {dir:?}"))?;
        g.match_state = MatchState::Pending;
        g.appid = None;
        g.steam = None;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, size: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; size]).unwrap();
    }

    #[test]
    fn picks_main_executable() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path().join("OrderOfTheSinkingStar");
        touch(&g.join("sinking_star.exe"), 1000);
        touch(&g.join("_Redist/dxwebsetup.exe"), 10);
        touch(&g.join("_Redist/oalinst.exe"), 10);
        assert_eq!(find_executables(&g, "OrderOfTheSinkingStar"), vec!["sinking_star.exe"]);

        let g = t.path().join("PrisonEscapeSimulator");
        touch(&g.join("Prison Escape Simulator Dig Out/Prison Escape Simulator.exe"), 10);
        touch(&g.join("Prison Escape Simulator Dig Out/UnityCrashHandler64.exe"), 10);
        touch(&g.join("Redist/vcredist_x86.exe"), 10);
        assert_eq!(
            find_executables(&g, "PrisonEscapeSimulator"),
            vec!["Prison Escape Simulator Dig Out/Prison Escape Simulator.exe"]
        );
    }

    #[test]
    fn crash_in_game_name_is_not_a_helper() {
        for stem in ["CrashBandicoot", "CrashTeamRacing", "Crashday"] {
            assert!(!is_helper_exe(stem), "{stem}");
        }
        for stem in [
            "UnityCrashHandler64",
            "CrashReportClient",
            "UE4-CrashReporter",
            "crash_reporter",
            "crashpad_handler",
            "CrashSender1403",
        ] {
            assert!(is_helper_exe(stem), "{stem}");
        }
    }

    #[test]
    fn prefers_top_level_loader_over_nested_copy() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path().join("Paws");
        touch(&g.join("Paws.exe"), 800_000);
        touch(&g.join("Paws/Paws.exe"), 11_000_000);
        fs::create_dir_all(g.join("Paws/Paws_Data")).unwrap();
        assert_eq!(find_executables(&g, "Paws"), vec!["Paws.exe", "Paws/Paws.exe"]);
    }

    #[test]
    fn adopts_existing_prefixes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(PREFIXES_DIR);
        fs::create_dir_all(p.join("level_devil")).unwrap();
        fs::create_dir_all(p.join("Webbed")).unwrap();
        let mut lock = LockFile::default();
        assert_eq!(choose_prefix("LevelDevil", &lock, &p), "level_devil");
        assert_eq!(choose_prefix("Webbed", &lock, &p), "Webbed");
        assert_eq!(choose_prefix("TheFinalStation", &lock, &p), "the_final_station");

        // A prefix claimed by another game isn't adopted twice.
        lock.games.insert("LevelDevil".into(), Game { prefix: "level_devil".into(), ..Default::default() });
        assert_eq!(choose_prefix("Level_Devil", &lock, &p), "level_devil_2");
    }

    #[test]
    fn scan_adds_and_reports_missing() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path().to_path_buf(), false).unwrap();
        touch(&t.path().join("Celeste/Celeste.exe"), 10);
        fs::create_dir_all(t.path().join(PREFIXES_DIR)).unwrap();
        fs::create_dir_all(t.path().join(RUNTIMES_DIR)).unwrap();
        fs::write(t.path().join("notes.txt"), "not a game").unwrap();

        let (lock, report) = lib.scan().unwrap();
        assert_eq!(report.added, vec!["Celeste"]);
        let g = &lock.games["Celeste"];
        assert_eq!(g.executable.as_deref(), Some("Celeste.exe"));
        assert_eq!(g.prefix, "celeste");
        assert_eq!(g.match_state, MatchState::Pending);

        // Second scan: nothing new. Removing the dir reports it as missing
        // but keeps its settings.
        let (_, report) = lib.scan().unwrap();
        assert!(report.added.is_empty());
        fs::remove_dir_all(t.path().join("Celeste")).unwrap();
        let (lock, report) = lib.scan().unwrap();
        assert_eq!(report.missing, vec!["Celeste"]);
        assert!(lock.games.contains_key("Celeste"));
    }

    #[test]
    fn resolves_games() {
        let mut lock = LockFile::default();
        let steam = |n: &str| crate::lockfile::SteamInfo { name: n.into(), ..Default::default() };
        lock.games.insert(
            "OrderOfTheSinkingStar".into(),
            Game {
                appid: Some(499170),
                steam: Some(steam("Order of the Sinking Star")),
                ..Default::default()
            },
        );
        lock.games.insert("TheFinalStation".into(), Game::default());
        lock.games.insert("Paws".into(), Game::default());
        lock.games.insert("PawsAndClaws".into(), Game::default());

        assert_eq!(resolve_game(&lock, "Paws").unwrap(), "Paws");
        assert_eq!(resolve_game(&lock, "paws").unwrap(), "Paws");
        assert_eq!(resolve_game(&lock, "order of the sinking star").unwrap(), "OrderOfTheSinkingStar");
        assert_eq!(resolve_game(&lock, "499170").unwrap(), "OrderOfTheSinkingStar");
        assert_eq!(resolve_game(&lock, "final").unwrap(), "TheFinalStation");
        assert_eq!(resolve_game(&lock, "sinking").unwrap(), "OrderOfTheSinkingStar");
        assert!(resolve_game(&lock, "aw").is_err());
        assert!(resolve_game(&lock, "zelda").is_err());
    }

    #[test]
    fn lookup_queue() {
        let mut lock = LockFile::default();
        lock.games.insert("a".into(), Game::default());
        lock.games.insert("b".into(), Game { match_state: MatchState::Unmatched, ..Default::default() });
        lock.games.insert(
            "c".into(),
            Game { match_state: MatchState::Manual, appid: Some(1), ..Default::default() },
        );
        lock.games.insert(
            "d".into(),
            Game {
                match_state: MatchState::Matched,
                appid: Some(2),
                steam: Some(Default::default()),
                ..Default::default()
            },
        );
        assert_eq!(pending_lookups(&lock), vec!["a", "c"]);
    }
}
