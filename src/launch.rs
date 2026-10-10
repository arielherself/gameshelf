//! Building and running the `umu-run` command for a game.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::library::Library;
use crate::lockfile::LockFile;
use crate::runtime;
use crate::util::now;

#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub dir: String,
    pub umu_run: String,
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Variables set on top of the inherited environment, in order.
    pub env: Vec<(String, String)>,
    pub prefix: PathBuf,
    pub runtime: String,
}

pub fn plan(lib: &Library, lock: &LockFile, dir: &str, extra_args: &[String]) -> Result<LaunchPlan> {
    let game = lock.games.get(dir).with_context(|| format!("unknown game {dir:?}"))?;
    if !lib.game_dir(dir).is_dir() {
        bail!("{} no longer exists", lib.game_dir(dir).display());
    }
    let exe = lib.executable_path(dir, game).with_context(|| {
        format!("no executable set for {dir} — pick one with `gameshelf set {dir:?} --exe <path>` or `e` in the TUI")
    })?;
    if !exe.is_file() {
        bail!("executable {} does not exist", exe.display());
    }
    let (runtime_name, runtime_path) = runtime::resolve(lib, lock, game)?;
    let prefix = lib.prefix_path(&game.prefix);

    let game_id = match (&game.game_id, game.appid) {
        (Some(id), _) => id.clone(),
        (None, Some(appid)) if lock.settings.gameid_from_appid => format!("umu-{appid}"),
        _ => "0".to_string(),
    };

    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: String| {
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v));
    };
    for (k, v) in lock.settings.env.iter().chain(game.env.iter()) {
        set(k, v.clone());
    }
    set("WINEPREFIX", prefix.display().to_string());
    set("PROTONPATH", runtime_path.display().to_string());
    set("GAMEID", game_id);

    let umu_run = lock
        .settings
        .umu_run
        .clone()
        .or_else(|| std::env::var("GAMESHELF_UMU_RUN").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "umu-run".to_string());

    let mut args = game.args.clone();
    args.extend_from_slice(extra_args);
    Ok(LaunchPlan {
        dir: dir.to_string(),
        umu_run,
        cwd: exe.parent().map(Path::to_path_buf).unwrap_or_else(|| lib.game_dir(dir)),
        exe,
        args,
        env,
        prefix,
        runtime: runtime_name,
    })
}

impl LaunchPlan {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.umu_run);
        cmd.arg(&self.exe).args(&self.args).current_dir(&self.cwd);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// The command as a copy-pasteable shell line.
    pub fn shell_line(&self) -> String {
        let q = |s: &str| shlex::try_quote(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
        let mut parts = vec![format!("cd {} &&", q(&self.cwd.display().to_string()))];
        parts.extend(self.env.iter().map(|(k, v)| format!("{k}={}", q(v))));
        parts.push(q(&self.umu_run));
        parts.push(q(&self.exe.display().to_string()));
        parts.extend(self.args.iter().map(|a| q(a)));
        parts.join(" ")
    }

    fn prepare(&self) -> Result<()> {
        // umu creates the prefix itself, but not missing parent directories.
        if let Some(parent) = self.prefix.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        Ok(())
    }

    fn spawn_err(&self, e: std::io::Error) -> anyhow::Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "`{}` not found — install umu-launcher, or set umu_run in gameshelf.lock \
                 (or GAMESHELF_UMU_RUN)",
                self.umu_run
            )
        } else {
            anyhow::Error::new(e).context(format!("starting {}", self.umu_run))
        }
    }

    /// Run attached to the terminal and wait for it to exit.
    pub fn run_foreground(&self) -> Result<std::process::ExitStatus> {
        self.prepare()?;
        let mut child = self.command().spawn().map_err(|e| self.spawn_err(e))?;
        Ok(child.wait()?)
    }

    /// The `umu-run` that will actually be executed.
    pub fn umu_path(&self) -> Option<PathBuf> {
        find_in_path(&self.umu_run)
    }

    /// Known-bad setups worth telling the user about before they wonder why
    /// the game shows no window.
    pub fn environment_warning(&self) -> Option<String> {
        nix_gpu_warning(&self.umu_path()?)
    }
}

/// Resolve a command name like the shell would.
pub fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        let p = PathBuf::from(cmd);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(cmd))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// umu-launcher built by Nix runs games in an FHS sandbox whose graphics
/// drivers come from `/run/opengl-driver`. Only NixOS (or a manual setup
/// such as nix-system-graphics) provides that; elsewhere Mesa finds no
/// driver and the game never opens a window.
fn nix_gpu_warning(umu: &Path) -> Option<String> {
    let real = fs::canonicalize(umu).unwrap_or_else(|_| umu.to_path_buf());
    let from_nix = real.starts_with("/nix/store");
    let has_drivers = Path::new("/run/opengl-driver").exists() || Path::new("/etc/NIXOS").exists();
    (from_nix && !has_drivers).then(|| {
        format!(
            "{} is the Nix build of umu-launcher, which can't see this system's GPU drivers \
             (no /run/opengl-driver; not NixOS). Install umu-launcher with the system package \
             manager so gameshelf picks it up, or set umu_run in gameshelf.lock.",
            umu.display()
        )
    })
}

pub fn log_path(dir: &str) -> PathBuf {
    let base = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    let safe: String =
        dir.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    base.join("gameshelf").join("logs").join(format!("{safe}.log"))
}

pub fn record_start(lib: &Library, dir: &str) -> Result<u64> {
    let started = now();
    lib.update(|lock| {
        if let Some(g) = lock.games.get_mut(dir) {
            g.last_played = Some(started);
        }
        Ok(())
    })?;
    Ok(started)
}

pub fn record_end(lib: &Library, dir: &str, started: u64) -> Result<u64> {
    let secs = now().saturating_sub(started);
    lib.update(|lock| {
        if let Some(g) = lock.games.get_mut(dir) {
            g.playtime_secs += secs;
        }
        Ok(())
    })?;
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::Game;
    use std::collections::BTreeMap;

    fn setup() -> (tempfile::TempDir, Library, LockFile) {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path().to_path_buf(), false).unwrap();
        fs::create_dir_all(t.path().join("Celeste/bin")).unwrap();
        fs::write(t.path().join("Celeste/bin/Celeste.exe"), "").unwrap();
        let rt = lib.runtimes_dir().join("GE-Proton11-7-x86_64");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("proton"), "").unwrap();
        let mut lock = LockFile::default();
        lock.games.insert(
            "Celeste".into(),
            Game {
                prefix: "celeste".into(),
                executable: Some("bin/Celeste.exe".into()),
                appid: Some(504230),
                args: vec!["-fullscreen".into()],
                env: BTreeMap::from([
                    ("PROTON_PREFER_SDL".into(), "0".into()),
                    ("DXVK_HUD".into(), "fps".into()),
                ]),
                ..Default::default()
            },
        );
        (t, lib, lock)
    }

    #[test]
    fn builds_umu_environment() {
        let (t, lib, lock) = setup();
        let p = plan(&lib, &lock, "Celeste", &["--extra".into()]).unwrap();
        let env: BTreeMap<_, _> = p.env.iter().cloned().collect();
        assert_eq!(env["WINEPREFIX"], t.path().join(".proton-prefixes/celeste").display().to_string());
        assert_eq!(
            env["PROTONPATH"],
            t.path().join(".proton-runtimes/GE-Proton11-7-x86_64").display().to_string()
        );
        assert_eq!(env["GAMEID"], "umu-504230");
        assert_eq!(env["PROTON_PREFER_SDL"], "0", "game env overrides library env");
        assert_eq!(env["DXVK_HUD"], "fps");
        assert_eq!(p.args, ["-fullscreen", "--extra"]);
        assert_eq!(p.cwd, t.path().join("Celeste/bin"));
        assert_eq!(p.runtime, "GE-Proton11-7-x86_64");
        assert!(p.shell_line().contains("GAMEID=umu-504230"));
    }

    #[test]
    fn gameid_rules() {
        let (_t, lib, mut lock) = setup();
        lock.settings.gameid_from_appid = false;
        let env = |lock: &LockFile| plan(&lib, lock, "Celeste", &[]).unwrap().env;
        assert!(env(&lock).contains(&("GAMEID".into(), "0".into())));
        lock.games.get_mut("Celeste").unwrap().game_id = Some("umu-custom".into());
        assert!(env(&lock).contains(&("GAMEID".into(), "umu-custom".into())));
    }

    #[test]
    fn explains_missing_pieces() {
        let (_t, lib, mut lock) = setup();
        lock.games.get_mut("Celeste").unwrap().executable = None;
        assert!(plan(&lib, &lock, "Celeste", &[]).unwrap_err().to_string().contains("no executable"));
        lock.games.get_mut("Celeste").unwrap().executable = Some("nope.exe".into());
        assert!(plan(&lib, &lock, "Celeste", &[]).unwrap_err().to_string().contains("does not exist"));
        assert!(plan(&lib, &lock, "Other", &[]).is_err());
    }

    #[test]
    fn resolves_commands_on_path() {
        assert_eq!(find_in_path("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert!(find_in_path("sh").is_some());
        assert_eq!(find_in_path("definitely-not-a-command-gameshelf"), None);
    }

    #[test]
    fn warns_about_nix_umu_only_without_drivers() {
        let fake = Path::new("/nix/store/xxx-umu-launcher/bin/umu-run");
        let expected = !Path::new("/run/opengl-driver").exists() && !Path::new("/etc/NIXOS").exists();
        assert_eq!(nix_gpu_warning(fake).is_some(), expected);
        assert_eq!(nix_gpu_warning(Path::new("/usr/bin/umu-run")), None);
    }

    #[test]
    fn log_names_are_safe() {
        let p = log_path("Some Game/../x");
        assert_eq!(p.file_name().unwrap(), "Some_Game_.._x.log");
    }
}
