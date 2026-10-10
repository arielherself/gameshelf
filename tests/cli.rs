//! End-to-end tests of the `gameshelf` binary against a mock Steam/GitHub
//! server and a fake `umu-run`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use sha2::{Digest, Sha512};

type Routes = Arc<Mutex<BTreeMap<String, (String, Vec<u8>)>>>;

/// Minimal HTTP/1.1 server. Routes match on the request path including the
/// query string, by prefix; unknown paths get a 404.
struct MockServer {
    url: String,
    routes: Routes,
    log: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes: Routes = Arc::default();
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let (r, l) = (routes.clone(), log.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                l.lock().unwrap().push(path.clone());
                let routes = r.lock().unwrap();
                let hit =
                    routes.iter().filter(|(p, _)| path.starts_with(p.as_str())).max_by_key(|(p, _)| p.len());
                let (status, ctype, body) = match hit {
                    Some((_, (ctype, body))) => ("200 OK", ctype.clone(), body.clone()),
                    None => ("404 Not Found", "text/plain".into(), b"not found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self { url, routes, log }
    }

    fn route(&self, path: &str, ctype: &str, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(path.to_string(), (ctype.to_string(), body.into()));
    }

    fn json(&self, path: &str, body: serde_json::Value) {
        self.route(path, "application/json", body.to_string());
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

struct Env {
    tmp: tempfile::TempDir,
    server: MockServer,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("lib")).unwrap();
        let server = MockServer::start();
        server.json(
            "/api/storesearch/?term=Order%20Of%20The%20Sinking%20Star&",
            serde_json::json!({"total": 2, "items": [
                {"type": "app", "name": "Order of the Sinking Star Soundtrack", "id": 5217050},
                {"type": "app", "name": "Order of the Sinking Star", "id": 499170}
            ]}),
        );
        server.json(
            "/api/storesearch/?term=iwanna&",
            serde_json::json!({"total": 2, "items": [
                {"type": "app", "name": "I Wanna Maker", "id": 1114940},
                {"type": "app", "name": "I Wanna Be", "id": 4994740}
            ]}),
        );
        server.json("/api/storesearch/?term=IWBTGR&", serde_json::json!({"total": 0, "items": []}));
        server.json(
            "/api/appdetails?appids=499170&",
            serde_json::json!({"499170": {"success": true, "data": {
                "type": "game", "name": "Order of the Sinking Star", "steam_appid": 499170,
                "short_description": "A massive puzzle adventure.", "developers": ["Thekla, Inc."],
                "publishers": ["Arc Games"], "release_date": {"coming_soon": false, "date": "8 Oct, 2026"},
                "genres": [{"id": "25", "description": "Adventure"}]
            }}}),
        );
        server.json(
            "/api/appdetails?appids=12345&",
            serde_json::json!({"12345": {"success": true, "data": {"type": "game", "name": "I Wanna Be The Guy"}}}),
        );
        server.json("/api/appdetails?appids=999&", serde_json::json!({"999": {"success": false}}));
        Self { tmp, server }
    }

    fn lib(&self) -> PathBuf {
        self.tmp.path().join("lib")
    }

    fn add_game(&self, dir: &str, exe: &str) {
        let p = self.lib().join(dir).join(exe);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"MZ").unwrap();
    }

    fn add_runtime(&self, name: &str) {
        let p = self.lib().join(".proton-runtimes").join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("proton"), "#!/bin/sh\n").unwrap();
    }

    /// A fake `umu-run` that records its environment, arguments and cwd.
    fn fake_umu(&self, exit_code: i32) -> PathBuf {
        let script = self.tmp.path().join("umu-run");
        let record = self.tmp.path().join("umu-record");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n{{ echo \"CWD=$(pwd)\"; for a in \"$@\"; do echo \"ARG=$a\"; done; env; }} > '{}'\nexit {exit_code}\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn umu_record(&self) -> Vec<String> {
        fs::read_to_string(self.tmp.path().join("umu-record")).unwrap().lines().map(str::to_string).collect()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gameshelf"));
        c.args(args)
            .env("GAMESHELF_LIBRARY", self.lib())
            .env("GAMESHELF_STEAM_URL", &self.server.url)
            .env("GAMESHELF_GITHUB_API", &self.server.url)
            .env("XDG_CONFIG_HOME", self.tmp.path().join("config"))
            .env("XDG_STATE_HOME", self.tmp.path().join("state"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
            .env_remove("GAMESHELF_OFFLINE")
            .env_remove("GAMESHELF_UMU_RUN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "gameshelf {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn lock(&self) -> toml::Table {
        toml::from_str(&fs::read_to_string(self.lib().join("gameshelf.lock")).unwrap()).unwrap()
    }

    fn game(&self, dir: &str) -> toml::Table {
        self.lock()["games"][dir].as_table().unwrap().clone()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn scan_matches_new_games_and_launches_them() {
    let env = Env::new();
    env.add_game("OrderOfTheSinkingStar", "sinking_star.exe");
    env.add_game("OrderOfTheSinkingStar", "_Redist/dxwebsetup.exe");
    env.add_game("iwanna", "IWBTGR.exe");
    env.add_runtime("GE-Proton11-7-x86_64");
    fs::create_dir_all(env.lib().join(".proton-prefixes/iwanna")).unwrap();

    let out = env.run(&["list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("OrderOfTheSinkingStar: matched Order of the Sinking Star (499170"), "{err}");
    assert!(err.contains("iwanna: no Steam match"), "{err}");

    let g = env.game("OrderOfTheSinkingStar");
    assert_eq!(g["appid"].as_integer(), Some(499170));
    assert_eq!(g["match"].as_str(), Some("matched"));
    assert_eq!(g["executable"].as_str(), Some("sinking_star.exe"));
    assert_eq!(g["prefix"].as_str(), Some("order_of_the_sinking_star"));
    assert_eq!(g["steam"]["name"].as_str(), Some("Order of the Sinking Star"));
    assert_eq!(g["steam"]["developers"][0].as_str(), Some("Thekla, Inc."));
    let iw = env.game("iwanna");
    assert_eq!(iw["match"].as_str(), Some("unmatched"));
    assert_eq!(iw["prefix"].as_str(), Some("iwanna"), "adopts the existing prefix");
    assert!(iw.get("appid").is_none());

    // Known games aren't looked up again.
    let before = env.server.requests().len();
    env.ok(&["list"]);
    assert_eq!(env.server.requests().len(), before);

    // Run by fragment of the Steam title; exit code is passed through.
    let umu = env.fake_umu(3);
    let out =
        env.cmd(&["run", "sinking", "--", "-windowed"]).env("GAMESHELF_UMU_RUN", &umu).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let rec = env.umu_record();
    let lib = env.lib().canonicalize().unwrap();
    let has = |line: String| assert!(rec.contains(&line), "missing {line:?} in {rec:#?}");
    has(format!("WINEPREFIX={}", env.lib().join(".proton-prefixes/order_of_the_sinking_star").display()));
    has(format!("PROTONPATH={}", env.lib().join(".proton-runtimes/GE-Proton11-7-x86_64").display()));
    has("GAMEID=umu-499170".into());
    has("PROTON_PREFER_SDL=1".into());
    has(format!("ARG={}", env.lib().join("OrderOfTheSinkingStar/sinking_star.exe").display()));
    has("ARG=-windowed".into());
    has(format!("CWD={}", lib.join("OrderOfTheSinkingStar").display()));
    assert!(env.lib().join(".proton-prefixes").is_dir());
    assert!(env.game("OrderOfTheSinkingStar")["last_played"].as_integer().is_some());

    // GAMEID falls back to 0 without an app id.
    let out = env.cmd(&["run", "iwanna"]).env("GAMESHELF_UMU_RUN", &umu).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(env.umu_record().contains(&"GAMEID=0".to_string()));
}

#[test]
fn manual_settings() {
    let env = Env::new();
    env.add_game("iwanna", "IWBTGR.exe");
    env.add_game("iwanna", "bin/other.exe");
    env.add_runtime("GE-Proton11-7-x86_64");
    env.add_runtime("UMU-Proton-10.0-4");
    env.ok(&["scan"]);

    // App id by hand fetches details and is never overwritten by matching.
    let out = env.ok(&["set", "iwanna", "--appid", "12345"]);
    assert!(out.contains("I Wanna Be The Guy"), "{out}");
    let g = env.game("iwanna");
    assert_eq!(g["appid"].as_integer(), Some(12345));
    assert_eq!(g["match"].as_str(), Some("manual"));
    assert_eq!(g["steam"]["name"].as_str(), Some("I Wanna Be The Guy"));
    env.ok(&["scan"]);
    assert_eq!(env.game("iwanna")["appid"].as_integer(), Some(12345));

    // An id without a store page is kept, with a warning.
    let out = env.run(&["set", "iwanna", "--appid", "999"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("no store page"), "{}", stderr(&out));
    assert_eq!(env.game("iwanna")["appid"].as_integer(), Some(999));
    env.ok(&["set", "iwanna", "--appid", "none"]);
    assert!(env.game("iwanna").get("appid").is_none());
    assert!(!env.run(&["set", "iwanna", "--appid", "abc"]).status.success());

    env.ok(&[
        "set",
        "iwanna",
        "--exe",
        "bin/other.exe",
        "--runtime",
        "UMU-Proton-10.0-4",
        "--prefix",
        "shared",
        "--args",
        "-a 'b c'",
        "--env",
        "DXVK_HUD=fps",
        "--game-id",
        "umu-custom",
    ]);
    let g = env.game("iwanna");
    assert_eq!(g["executable"].as_str(), Some("bin/other.exe"));
    assert_eq!(g["runtime"].as_str(), Some("UMU-Proton-10.0-4"));
    assert_eq!(g["prefix"].as_str(), Some("shared"));
    assert_eq!(g["args"].as_array().unwrap().len(), 2);
    assert_eq!(g["env"]["DXVK_HUD"].as_str(), Some("fps"));

    let line = env.ok(&["run", "iwanna", "--dry-run"]);
    assert!(line.contains("GAMEID=umu-custom"), "{line}");
    assert!(line.contains("DXVK_HUD=fps"), "{line}");
    assert!(line.contains("UMU-Proton-10.0-4"), "{line}");
    assert!(line.contains("-a 'b c'"), "{line}");
    assert!(line.contains(".proton-prefixes/shared"), "{line}");

    // Invalid values are rejected and leave the entry alone.
    assert!(!env.run(&["set", "iwanna", "--runtime", "Nope"]).status.success());
    assert!(!env.run(&["set", "iwanna", "--exe", "missing.exe"]).status.success());
    assert!(!env.run(&["set", "iwanna", "--env", "NOEQUALS"]).status.success());
    assert_eq!(env.game("iwanna")["runtime"].as_str(), Some("UMU-Proton-10.0-4"));

    // Back to defaults.
    env.ok(&[
        "set",
        "iwanna",
        "--exe",
        "auto",
        "--runtime",
        "default",
        "--prefix",
        "new",
        "--game-id",
        "auto",
        "--args",
        "",
        "--unset-env",
        "DXVK_HUD",
    ]);
    let g = env.game("iwanna");
    assert_eq!(g["executable"].as_str(), Some("IWBTGR.exe"));
    assert!(
        g.get("runtime").is_none()
            && g.get("args").is_none()
            && g.get("env").is_none()
            && g.get("game_id").is_none()
    );
    assert_eq!(g["prefix"].as_str(), Some("iwanna"));

    // Library default runtime.
    env.ok(&["runtime", "default", "UMU-Proton-10.0-4"]);
    assert!(env.ok(&["run", "iwanna", "-n"]).contains("UMU-Proton-10.0-4"));
    assert!(!env.run(&["runtime", "default", "Nope"]).status.success());
}

#[test]
fn offline_registers_without_network() {
    let env = Env::new();
    env.add_game("Celeste", "Celeste.exe");
    let out = env.run(&["--offline", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.server.requests().is_empty());
    assert_eq!(env.game("Celeste")["match"].as_str(), Some("pending"));
    // Without a runtime, launching explains what to do.
    let out = env.run(&["--offline", "run", "celeste", "-n"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("runtime install"), "{}", stderr(&out));
}

#[test]
fn unreachable_steam_is_not_fatal() {
    let env = Env::new();
    env.add_game("Celeste", "Celeste.exe");
    env.add_runtime("GE-Proton11-7-x86_64");
    let out =
        env.cmd(&["run", "Celeste", "-n"]).env("GAMESHELF_STEAM_URL", "http://127.0.0.1:9").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("will retry"), "{}", stderr(&out));
    assert_eq!(env.game("Celeste")["match"].as_str(), Some("pending"));
}

#[test]
fn missing_library_is_an_error_unless_default() {
    let env = Env::new();
    let out = env.cmd(&["list"]).env("GAMESHELF_LIBRARY", env.tmp.path().join("nope")).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not exist"));

    // The library can also come from config.toml.
    fs::create_dir_all(env.tmp.path().join("config/gameshelf")).unwrap();
    let other = env.tmp.path().join("other");
    fs::create_dir_all(other.join("Webbed")).unwrap();
    fs::write(
        env.tmp.path().join("config/gameshelf/config.toml"),
        format!("library = {:?}\n", other.display().to_string()),
    )
    .unwrap();
    let out = env.cmd(&["--offline", "list"]).env_remove("GAMESHELF_LIBRARY").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("Webbed"), "{}", stderr(&out));
}

fn make_runtime_tarball(top: &str) -> Vec<u8> {
    let mut builder =
        tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
    let mut add = |path: &str, data: &[u8], mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        builder.append_data(&mut h, path, data).unwrap();
    };
    add(&format!("{top}/proton"), b"#!/bin/sh\n", 0o755);
    add(&format!("{top}/files/bin/wine"), b"wine", 0o755);
    add(&format!("{top}/version"), b"1", 0o644);
    builder.into_inner().unwrap().finish().unwrap()
}

fn sha512_hex(data: &[u8]) -> String {
    hex::encode(Sha512::digest(data))
}

#[test]
fn installs_runtimes_from_github() {
    let env = Env::new();
    let url = &env.server.url;
    let tarball = make_runtime_tarball("GE-Proton99-1-x86_64");
    env.server.route("/dl/GE-Proton99-1-x86_64.tar.gz", "application/gzip", tarball.clone());
    env.server.route(
        "/dl/GE-Proton99-1-x86_64.sha512sum",
        "text/plain",
        format!("{}  GE-Proton99-1-x86_64.tar.gz\n", sha512_hex(&tarball)),
    );
    let bad = make_runtime_tarball("GE-Proton98-1-x86_64");
    env.server.route("/dl/GE-Proton98-1-x86_64.tar.gz", "application/gzip", bad);
    env.server.route(
        "/dl/GE-Proton98-1-x86_64.sha512sum",
        "text/plain",
        format!("{}  GE-Proton98-1-x86_64.tar.gz\n", "0".repeat(128)),
    );
    let asset = |name: &str| serde_json::json!({"name": name, "browser_download_url": format!("{url}/dl/{name}"), "size": 100});
    env.server.json(
        "/repos/GloriousEggroll/proton-ge-custom/releases",
        serde_json::json!([
            {"tag_name": "GE-Proton99-1", "published_at": "2026-10-01T00:00:00Z", "assets": [
                asset("GE-Proton99-1-x86_64.tar.gz"), asset("GE-Proton99-1-x86_64.sha512sum"),
                asset("GE-Proton99-1-aarch64.tar.gz")]},
            {"tag_name": "GE-Proton98-1", "published_at": "2026-09-01T00:00:00Z", "assets": [
                asset("GE-Proton98-1-x86_64.tar.gz"), asset("GE-Proton98-1-x86_64.sha512sum")]}
        ]),
    );
    if std::env::consts::ARCH != "x86_64" {
        return;
    }

    let avail = env.ok(&["runtime", "available", "ge"]);
    assert!(avail.contains("GE-Proton99-1-x86_64") && !avail.contains("aarch64"), "{avail}");

    env.ok(&["runtime", "install", "ge"]);
    let rt = env.lib().join(".proton-runtimes/GE-Proton99-1-x86_64");
    assert!(rt.join("proton").is_file());
    assert_ne!(fs::metadata(rt.join("proton")).unwrap().permissions().mode() & 0o111, 0, "keeps exec bits");
    assert!(env.ok(&["runtime", "list"]).contains("GE-Proton99-1-x86_64  default (newest)"));
    assert!(!env.run(&["runtime", "install", "ge"]).status.success(), "already installed");

    // Checksum mismatch: nothing is left behind.
    let out = env.run(&["runtime", "install", "GE-Proton98-1-x86_64"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("SHA-512 mismatch"), "{}", stderr(&out));
    let leftovers: Vec<_> =
        fs::read_dir(env.lib().join(".proton-runtimes")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers, vec!["GE-Proton99-1-x86_64"]);

    // Link an existing build, then remove both.
    let external = env.tmp.path().join("steam/GE-Proton11-7-x86_64");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("proton"), "").unwrap();
    env.ok(&["runtime", "link", external.to_str().unwrap()]);
    let list = env.ok(&["runtime", "list"]);
    assert!(list.lines().any(|l| l.starts_with("GE-Proton11-7-x86_64  ") && l.contains("→")), "{list}");
    env.ok(&["runtime", "remove", "GE-Proton11-7-x86_64"]);
    assert!(external.join("proton").exists());
    env.ok(&["runtime", "remove", "GE-Proton99-1-x86_64"]);
    assert!(fs::read_dir(env.lib().join(".proton-runtimes")).unwrap().next().is_none());
}

#[test]
fn concurrent_updates_do_not_lose_writes() {
    let env = Env::new();
    for i in 0..8 {
        env.add_game(&format!("Game{i}"), "game.exe");
    }
    env.ok(&["--offline", "scan"]);
    let handles: Vec<_> = (0..8)
        .map(|i| {
            env.cmd(&["--offline", "set", &format!("Game{i}"), "--env", &format!("N={i}")])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut h in handles {
        assert!(h.wait().unwrap().success());
    }
    for i in 0..8 {
        assert_eq!(env.game(&format!("Game{i}"))["env"]["N"].as_str(), Some(i.to_string().as_str()));
    }
}
