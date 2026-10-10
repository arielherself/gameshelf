//! The `gameshelf.lock` file: per-library settings and per-game metadata.
//!
//! The lock file lives at the root of the library directory. It is plain TOML
//! so it can be inspected or edited by hand, but it is primarily maintained by
//! gameshelf itself. Every mutation goes through [`crate::library::Library::update`],
//! which holds an advisory lock while it re-reads, modifies and atomically
//! rewrites the file, so a TUI session and a concurrent `gameshelf run` don't
//! clobber each other's changes.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockFile {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    /// Keyed by the game's directory name inside the library.
    #[serde(default)]
    pub games: BTreeMap<String, Game>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self { version: LOCK_VERSION, settings: Settings::default(), games: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Runtime used by games that don't pick one explicitly. Either a directory
    /// name inside `.proton-runtimes` or an absolute path. When unset, the most
    /// recently installed runtime is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_runtime: Option<String>,
    /// The `umu-run` executable. Defaults to `umu-run` from `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub umu_run: Option<String>,
    /// Derive umu's `GAMEID` from the Steam app id (`umu-<appid>`), which lets
    /// umu apply protonfixes for that game. When false, `GAMEID=0` is used.
    pub gameid_from_appid: bool,
    /// Environment variables applied to every game (games can override them).
    pub env: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_runtime: None,
            umu_run: None,
            gameid_from_appid: true,
            env: BTreeMap::from([("PROTON_PREFER_SDL".to_string(), "1".to_string())]),
        }
    }
}

/// Where a game's Steam app id came from, and whether a lookup is still due.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MatchState {
    /// Not looked up yet (or the last lookup failed for network reasons).
    #[default]
    Pending,
    /// Matched automatically by searching the Steam store.
    Matched,
    /// The store search found nothing that looked right.
    Unmatched,
    /// The user set the app id explicitly; never overwritten automatically.
    Manual,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Game {
    /// Unix timestamp of when the directory was first seen.
    pub added: u64,
    #[serde(rename = "match")]
    pub match_state: MatchState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appid: Option<u32>,
    /// Executable to launch, relative to the game directory (or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Wine prefix: a directory name inside `.proton-prefixes`, or an absolute path.
    pub prefix: String,
    /// Proton runtime override; `None` means "use the library default".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Overrides the `GAMEID` passed to umu.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
    #[serde(skip_serializing_if = "is_zero")]
    pub playtime_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steam: Option<SteamInfo>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// The subset of Steam's `appdetails` we keep.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SteamInfo {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub short_description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub developers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub publishers: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub release_date: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub website: String,
    pub fetched_at: u64,
}

impl Game {
    /// Name to show in lists: the Steam title when known, else the directory name.
    pub fn display_name<'a>(&'a self, dir: &'a str) -> &'a str {
        match &self.steam {
            Some(s) if !s.name.is_empty() => &s.name,
            _ => dir,
        }
    }
}

impl LockFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let lock: LockFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if lock.version > LOCK_VERSION {
            bail!(
                "{} was written by a newer gameshelf (lock version {}, supported {})",
                path.display(),
                lock.version,
                LOCK_VERSION
            );
        }
        Ok(lock)
    }

    /// Write atomically: serialize to a sibling temp file, then rename over.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = String::from(
            "# Maintained by gameshelf. Manual edits are preserved, but prefer `gameshelf set`.\n",
        );
        text.push_str(&toml::to_string_pretty(self).context("serializing lock file")?);
        let tmp = path.with_extension("lock.tmp");
        {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gameshelf.lock");
        let mut lock = LockFile::default();
        lock.settings.default_runtime = Some("GE-Proton11-7-x86_64".into());
        lock.games.insert(
            "Order Of The Sinking Star".into(),
            Game {
                added: 42,
                match_state: MatchState::Matched,
                appid: Some(499170),
                executable: Some("sinking_star.exe".into()),
                prefix: "order_of_the_sinking_star".into(),
                args: vec!["-windowed".into()],
                env: BTreeMap::from([("DXVK_HUD".into(), "fps".into())]),
                playtime_secs: 3600,
                steam: Some(SteamInfo {
                    name: "Order of the Sinking Star".into(),
                    developers: vec!["Thekla, Inc.".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        lock.save(&path).unwrap();
        let loaded = LockFile::load(&path).unwrap();
        assert_eq!(loaded, lock);
        assert!(!path.with_extension("lock.tmp").exists());
    }

    #[test]
    fn missing_file_is_empty_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = LockFile::load(&dir.path().join("nope.lock")).unwrap();
        assert!(lock.games.is_empty());
        assert_eq!(lock.settings.env.get("PROTON_PREFER_SDL").map(String::as_str), Some("1"));
    }

    #[test]
    fn rejects_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gameshelf.lock");
        fs::write(&path, "version = 99\n").unwrap();
        assert!(LockFile::load(&path).is_err());
    }

    #[test]
    fn minimal_hand_written_entry_parses() {
        let lock: LockFile = toml::from_str(
            "version = 1\n[games.Celeste]\nprefix = \"celeste\"\nappid = 504230\nmatch = \"manual\"\n",
        )
        .unwrap();
        let g = &lock.games["Celeste"];
        assert_eq!(g.match_state, MatchState::Manual);
        assert_eq!(g.appid, Some(504230));
        assert!(lock.settings.gameid_from_appid);
    }
}
