//! Command-line interface. Without a subcommand, the TUI starts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::library::{self, Library, LookupOutcome};
use crate::lockfile::{LockFile, MatchState};
use crate::steam::SteamClient;
use crate::util::{format_ago, format_bytes, format_duration, now, tilde};
use crate::{config, launch, runtime, tui};

#[derive(Parser)]
#[command(
    name = "gameshelf",
    version,
    about = "A TUI game launcher for Windows games via umu-launcher and Proton"
)]
pub struct Cli {
    /// Game library directory [default: library from config.toml, else ~/Games]
    #[arg(short = 'L', long, global = true, env = "GAMESHELF_LIBRARY", value_name = "DIR")]
    pub library: Option<String>,

    /// Don't contact Steam (new games are still registered, looked up later)
    #[arg(long, global = true, env = "GAMESHELF_OFFLINE", value_parser = clap::builder::FalseyValueParser::new())]
    pub offline: bool,

    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Open the TUI (the default)
    Tui,
    /// List games in the library
    #[command(alias = "ls")]
    List {
        /// Print the lock entries as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show everything known about a game
    Info { game: String },
    /// Launch a game
    Run {
        /// Directory name, Steam title, app id, or an unambiguous fragment
        game: String,
        /// Print the command instead of running it
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Return immediately; output goes to the game's log file
        #[arg(short, long)]
        detach: bool,
        /// Extra arguments for the game (after `--`)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Register new directories and look them up on Steam
    Scan {
        /// Also retry games that previously found no Steam match
        #[arg(long)]
        retry_unmatched: bool,
    },
    /// Search the Steam store (to find an app id for `set --appid`)
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
    },
    /// Change a game's settings
    Set {
        game: String,
        /// Steam app id; `auto` to search again, `none` for no app id
        #[arg(long, value_name = "ID|auto|none")]
        appid: Option<String>,
        /// Executable, relative to the game directory; `auto` to re-detect
        #[arg(long, value_name = "PATH|auto")]
        exe: Option<String>,
        /// Wine prefix name in .proton-prefixes or an absolute path; `new` for a fresh one
        #[arg(long, value_name = "NAME|PATH|new")]
        prefix: Option<String>,
        /// Proton runtime name in .proton-runtimes or a path; `default` for the library default
        #[arg(long, value_name = "NAME|PATH|default")]
        runtime: Option<String>,
        /// umu GAMEID override; `auto` to derive it from the app id
        #[arg(long, value_name = "ID|auto")]
        game_id: Option<String>,
        /// Launch arguments as one shell-quoted string ("" to clear)
        #[arg(long, value_name = "ARGS", allow_hyphen_values = true)]
        args: Option<String>,
        /// Set an environment variable for this game
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Remove a per-game environment variable
        #[arg(long = "unset-env", value_name = "KEY")]
        unset_env: Vec<String>,
    },
    /// Manage Proton runtimes
    #[command(subcommand, alias = "rt")]
    Runtime(RuntimeCmd),
}

#[derive(Subcommand)]
pub enum RuntimeCmd {
    /// List installed runtimes
    #[command(alias = "ls")]
    List,
    /// List downloadable runtimes
    Available {
        /// ge, umu or cachyos (default: all)
        source: Option<String>,
    },
    /// Download and install a runtime: `ge`, `cachyos`, `umu` (latest), `ge:GE-Proton11-7`, or a full name
    Install { spec: String },
    /// Add an existing Proton directory (e.g. from Steam's compatibilitytools.d) as a symlink
    Link {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove an installed runtime
    #[command(alias = "rm")]
    Remove { name: String },
    /// Show or set the library's default runtime (`none` to unset)
    Default { name: Option<String> },
}

pub fn main() -> Result<i32> {
    let cli = Cli::parse();
    let (root, is_default) = config::library_path(cli.library.as_deref())?;
    let lib = Library::open(root, is_default)?;
    let (_, report) = lib.scan()?;

    match cli.command.unwrap_or(Cmd::Tui) {
        Cmd::Tui => {
            tui::run(lib, cli.offline)?;
            Ok(0)
        }
        cmd => {
            if !report.added.is_empty() {
                eprintln!("New: {}", report.added.join(", "));
            }
            if !cli.offline && !matches!(cmd, Cmd::Scan { .. }) {
                lookup_pending(&lib, false);
            }
            run_command(&lib, cli.offline, cmd)
        }
    }
}

/// Look up pending games, reporting progress on stderr. Network errors are
/// reported but not fatal; the games stay pending for next time.
fn lookup_pending(lib: &Library, verbose: bool) {
    let Ok(lock) = lib.load() else { return };
    let pending = library::pending_lookups(&lock);
    if pending.is_empty() {
        return;
    }
    let client = SteamClient::new();
    for dir in pending {
        match library::lookup_game(lib, &client, &dir) {
            Ok(LookupOutcome::Matched { appid, name, score }) => {
                eprintln!("{dir}: matched {name} ({appid}, score {score:.2})");
            }
            Ok(LookupOutcome::Unmatched) => {
                eprintln!("{dir}: no Steam match; set one with `gameshelf set {dir:?} --appid <ID>`");
            }
            Ok(LookupOutcome::Refreshed) if verbose => eprintln!("{dir}: details updated"),
            Ok(_) => {}
            Err(e) => {
                eprintln!("{dir}: Steam lookup failed: {e:#}");
                eprintln!("(will retry next run; use --offline to skip)");
                return;
            }
        }
    }
}

fn run_command(lib: &Library, offline: bool, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Tui => unreachable!(),
        Cmd::List { json } => {
            let lock = lib.load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&lock.games)?);
            } else {
                print_list(lib, &lock);
            }
        }
        Cmd::Info { game } => {
            let lock = lib.load()?;
            let dir = library::resolve_game(&lock, &game)?;
            print_info(lib, &lock, &dir);
        }
        Cmd::Run { game, dry_run, detach, args } => {
            let lock = lib.load()?;
            let dir = library::resolve_game(&lock, &game)?;
            let plan = launch::plan(lib, &lock, &dir, &args)?;
            if dry_run {
                println!("{}", plan.shell_line());
                return Ok(0);
            }
            let name = lock.games[&dir].display_name(&dir).to_string();
            eprintln!("Launching {name} with {}", plan.runtime);
            if detach {
                let log = launch::log_path(&dir);
                let child = plan.spawn_detached(&log)?;
                launch::record_start(lib, &dir)?;
                eprintln!("Started (pid {}), log: {}", child.id(), tilde(&log));
                return Ok(0);
            }
            let started = launch::record_start(lib, &dir)?;
            let status = plan.run_foreground()?;
            let secs = launch::record_end(lib, &dir, started)?;
            eprintln!("{name} exited ({status}) after {}", format_duration(secs));
            return Ok(status.code().unwrap_or(1));
        }
        Cmd::Scan { retry_unmatched } => {
            if retry_unmatched {
                lib.update(|lock| {
                    for g in lock.games.values_mut() {
                        if g.match_state == MatchState::Unmatched && g.appid.is_none() {
                            g.match_state = MatchState::Pending;
                        }
                    }
                    Ok(())
                })?;
            }
            if offline {
                eprintln!("offline: skipping Steam lookups");
            } else {
                lookup_pending(lib, true);
            }
            let lock = lib.load()?;
            let unmatched = lock.games.values().filter(|g| g.appid.is_none()).count();
            println!("{} games ({} without a Steam app id)", lock.games.len(), unmatched);
        }
        Cmd::Search { query } => {
            let query = query.join(" ");
            let items = SteamClient::new().search(&query)?;
            if items.is_empty() {
                println!("no results for {query:?}");
            }
            for item in items {
                println!("{:>8}  {}", item.appid, item.name);
            }
        }
        Cmd::Set { game, appid, exe, prefix, runtime, game_id, args, env, unset_env } => {
            let lock = lib.load()?;
            let dir = library::resolve_game(&lock, &game)?;
            set_game(
                lib,
                offline,
                &dir,
                SetArgs { appid, exe, prefix, runtime, game_id, args, env, unset_env },
            )?;
            print_info(lib, &lib.load()?, &dir);
        }
        Cmd::Runtime(rt) => runtime_command(lib, rt)?,
    }
    Ok(0)
}

struct SetArgs {
    appid: Option<String>,
    exe: Option<String>,
    prefix: Option<String>,
    runtime: Option<String>,
    game_id: Option<String>,
    args: Option<String>,
    env: Vec<String>,
    unset_env: Vec<String>,
}

fn set_game(lib: &Library, offline: bool, dir: &str, a: SetArgs) -> Result<()> {
    if let Some(appid) = &a.appid {
        match appid.as_str() {
            "auto" => {
                library::reset_match(lib, dir)?;
                if !offline {
                    lookup_pending(lib, false);
                }
            }
            "none" => {
                library::set_appid(lib, None, dir, None)?;
            }
            id => {
                let id: u32 = id.parse().with_context(|| format!("invalid app id {id:?}"))?;
                let client = (!offline).then(SteamClient::new);
                match library::set_appid(lib, client.as_ref(), dir, Some(id))? {
                    Some(info) => eprintln!("{dir}: app {id} is {}", info.name),
                    None if client.is_some() => {
                        eprintln!("warning: Steam has no store page for app {id}; keeping it anyway")
                    }
                    None => {}
                }
            }
        }
    }

    let env_pairs: Vec<(String, String)> = a
        .env
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("expected KEY=VALUE, got {kv:?}"))
        })
        .collect::<Result<_>>()?;
    let args = match &a.args {
        Some(s) => Some(shlex::split(s).with_context(|| format!("can't parse arguments {s:?}"))?),
        None => None,
    };
    if let Some(rt) = a.runtime.as_deref().filter(|r| *r != "default")
        && !runtime::is_proton_dir(&runtime::runtime_path(lib, rt))
    {
        let names: Vec<String> = runtime::list_installed(lib).into_iter().map(|r| r.name).collect();
        bail!(
            "runtime {rt:?} is not installed (installed: {})",
            if names.is_empty() { "none".into() } else { names.join(", ") }
        );
    }

    let game_dir = lib.game_dir(dir);
    let prefixes_dir = lib.prefixes_dir();
    lib.update(|lock| {
        let new_prefix = match a.prefix.as_deref() {
            Some("new") => Some(library::fresh_prefix(dir, lock, &prefixes_dir)),
            Some(p) if !p.is_empty() => Some(p.to_string()),
            Some(_) => bail!("prefix can't be empty"),
            None => None,
        };
        let g = lock.games.get_mut(dir).with_context(|| format!("unknown game {dir:?}"))?;
        if let Some(exe) = &a.exe {
            g.executable = if exe == "auto" {
                library::find_executables(&game_dir, dir).into_iter().next()
            } else {
                if !game_dir.join(crate::util::expand_tilde(exe)).is_file() {
                    bail!("{exe} does not exist in {}", game_dir.display());
                }
                Some(exe.clone())
            };
        }
        if let Some(p) = new_prefix {
            g.prefix = p;
        }
        if let Some(rt) = &a.runtime {
            g.runtime = (rt != "default").then(|| rt.clone());
        }
        if let Some(id) = &a.game_id {
            g.game_id = (id != "auto").then(|| id.clone());
        }
        if let Some(args) = args {
            g.args = args;
        }
        for (k, v) in env_pairs {
            g.env.insert(k, v);
        }
        for k in &a.unset_env {
            g.env.remove(k);
        }
        Ok(())
    })?;
    Ok(())
}

fn runtime_command(lib: &Library, cmd: RuntimeCmd) -> Result<()> {
    match cmd {
        RuntimeCmd::List => {
            let lock = lib.load()?;
            let installed = runtime::list_installed(lib);
            if installed.is_empty() {
                println!(
                    "No runtimes in {}. Try `gameshelf runtime install ge`.",
                    tilde(&lib.runtimes_dir())
                );
            }
            let effective_default = lock
                .settings
                .default_runtime
                .clone()
                .or_else(|| installed.iter().find(|r| r.valid).map(|r| r.name.clone()));
            for rt in &installed {
                let mut notes = Vec::new();
                if effective_default.as_deref() == Some(rt.name.as_str()) {
                    notes.push(
                        if lock.settings.default_runtime.is_some() { "default" } else { "default (newest)" }
                            .to_string(),
                    );
                }
                if let Some(t) = &rt.link_target {
                    notes.push(format!("→ {}", tilde(t)));
                }
                if !rt.valid {
                    notes.push("BROKEN: no proton script".into());
                }
                let users = runtime::users_of(&lock, &rt.name);
                if !users.is_empty() {
                    notes.push(format!("used by {}", users.join(", ")));
                }
                println!("{}  {}", rt.name, notes.join(", "));
            }
            let steam: Vec<_> = runtime::steam_compat_tools()
                .into_iter()
                .filter(|(n, _)| !installed.iter().any(|r| &r.name == n))
                .collect();
            if !steam.is_empty() {
                println!("\nFound in Steam (add with `gameshelf runtime link <path>`):");
                for (_, p) in steam {
                    println!("  {}", tilde(&p));
                }
            }
        }
        RuntimeCmd::Available { source } => {
            let sources = match source {
                Some(s) => vec![
                    runtime::Source::from_key(&s)
                        .with_context(|| format!("unknown source {s:?} (ge, umu, cachyos)"))?,
                ],
                None => runtime::Source::ALL.to_vec(),
            };
            let installed: Vec<String> = runtime::list_installed(lib).into_iter().map(|r| r.name).collect();
            for source in sources {
                println!("{} ({}):", source.label(), source.key());
                match runtime::fetch_available(source) {
                    Ok(list) => {
                        for a in list.iter().take(10) {
                            let mark = if installed.contains(&a.name) { "  [installed]" } else { "" };
                            println!("  {:<48} {}  {:>10}{mark}", a.name, a.published, format_bytes(a.size));
                        }
                    }
                    Err(e) => println!("  error: {e:#}"),
                }
            }
        }
        RuntimeCmd::Install { spec } => {
            let avail = runtime::find_available(&spec)?;
            eprintln!(
                "Installing {} ({}) from {}",
                avail.name,
                format_bytes(avail.size),
                avail.source.label()
            );
            let cancel = AtomicBool::new(false);
            let mut last_line = String::new();
            let name = runtime::install(lib, &avail, &cancel, &mut |p| {
                let line = match p.phase {
                    runtime::Phase::Downloading => {
                        format!("downloading {} / {}", format_bytes(p.done), format_bytes(p.total))
                    }
                    runtime::Phase::Verifying => "verifying checksum".into(),
                    runtime::Phase::Extracting => {
                        format!("extracting {}%", p.done * 100 / p.total.max(1))
                    }
                };
                if line != last_line {
                    eprint!("\r\x1b[2K{line}");
                    last_line = line;
                }
            });
            eprintln!();
            let name = name?;
            println!("Installed {name}");
        }
        RuntimeCmd::Link { path, name } => {
            let name = runtime::link(lib, &path, name.as_deref())?;
            println!("Linked {name} → {}", path.display());
        }
        RuntimeCmd::Remove { name } => {
            let lock = lib.load()?;
            runtime::remove(lib, &name)?;
            println!("Removed {name}");
            let users = runtime::users_of(&lock, &name);
            if !users.is_empty() {
                eprintln!("warning: still selected by {}; they won't launch until changed", users.join(", "));
            }
            if lock.settings.default_runtime.as_deref() == Some(name.as_str()) {
                eprintln!(
                    "warning: {name} was the library default; set a new one with `gameshelf runtime default`"
                );
            }
        }
        RuntimeCmd::Default { name } => match name {
            None => {
                let lock = lib.load()?;
                match &lock.settings.default_runtime {
                    Some(n) => println!("{n}"),
                    None => match runtime::list_installed(lib).into_iter().find(|r| r.valid) {
                        Some(r) => println!("{} (newest installed; no default set)", r.name),
                        None => println!("none"),
                    },
                }
            }
            Some(name) => {
                let value = if name == "none" {
                    None
                } else {
                    if !runtime::is_proton_dir(&runtime::runtime_path(lib, &name)) {
                        bail!("runtime {name:?} is not installed");
                    }
                    Some(name.clone())
                };
                lib.update(|lock| {
                    lock.settings.default_runtime = value;
                    Ok(())
                })?;
                println!("Default runtime: {name}");
            }
        },
    }
    Ok(())
}

fn print_list(lib: &Library, lock: &LockFile) {
    if lock.games.is_empty() {
        println!("No games in {} — each directory there is treated as a game.", tilde(&lib.root));
        return;
    }
    let mut rows: Vec<(String, String, String, String)> = lock
        .games
        .iter()
        .map(|(dir, g)| {
            let mut name = g.display_name(dir).to_string();
            if !lib.game_dir(dir).is_dir() {
                name.push_str("  (missing)");
            }
            let appid = match (g.appid, g.match_state) {
                (Some(id), MatchState::Manual) => format!("{id}*"),
                (Some(id), _) => id.to_string(),
                (None, MatchState::Pending) => "…".into(),
                (None, _) => "-".into(),
            };
            let played = match g.last_played {
                Some(t) => format!("{} · {}", format_duration(g.playtime_secs), format_ago(t, now())),
                None => String::new(),
            };
            (dir.clone(), name, appid, played)
        })
        .collect();
    rows.sort_by_key(|r| r.1.to_lowercase());
    let w0 = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    for (dir, name, appid, played) in rows {
        println!("{dir:<w0$}  {name:<w1$}  {appid:>8}  {played}");
    }
}

fn print_info(lib: &Library, lock: &LockFile, dir: &str) {
    let g = &lock.games[dir];
    println!("{}", g.display_name(dir));
    let field = |k: &str, v: &str| println!("  {k:<12} {v}");
    field("directory", &tilde(&lib.game_dir(dir)));
    let appid = match (g.appid, g.match_state) {
        (Some(id), MatchState::Manual) => format!("{id} (set manually)"),
        (Some(id), _) => format!("{id} (matched automatically)"),
        (None, MatchState::Pending) => "not looked up yet".into(),
        (None, _) => "none".into(),
    };
    field("steam app", &appid);
    if let Some(s) = &g.steam {
        if !s.developers.is_empty() {
            field("developer", &s.developers.join(", "));
        }
        if !s.release_date.is_empty() {
            field("released", &s.release_date);
        }
        if !s.genres.is_empty() {
            field("genres", &s.genres.join(", "));
        }
    }
    field("executable", g.executable.as_deref().unwrap_or("(none found)"));
    let prefix_path = lib.prefix_path(&g.prefix);
    field(
        "prefix",
        &format!(
            "{}{}",
            tilde(&prefix_path),
            if prefix_path.exists() { "" } else { " (created on first launch)" }
        ),
    );
    let rt = match runtime::resolve(lib, lock, g) {
        Ok((name, _)) if g.runtime.is_some() => name,
        Ok((name, _)) => format!("{name} (library default)"),
        Err(e) => format!("error: {e:#}"),
    };
    field("runtime", &rt);
    if !g.args.is_empty() {
        field("args", &shlex::try_join(g.args.iter().map(String::as_str)).unwrap_or_default());
    }
    let env: BTreeMap<_, _> = g.env.iter().collect();
    for (k, v) in env {
        field("env", &format!("{k}={v}"));
    }
    if let Some(id) = &g.game_id {
        field("GAMEID", id);
    }
    if let Some(t) = g.last_played {
        field(
            "played",
            &format!("{} total, last {}", format_duration(g.playtime_secs), format_ago(t, now())),
        );
    }
    if let Some(s) = g.steam.as_ref().filter(|s| !s.short_description.is_empty()) {
        println!("\n  {}", s.short_description);
    }
}
