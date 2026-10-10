//! Proton runtimes in `<library>/.proton-runtimes`: listing, resolving,
//! downloading from GitHub releases, and linking existing Steam compat tools.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};

use crate::library::Library;
use crate::lockfile::{Game, LockFile};
use crate::util::expand_tilde;

#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub name: String,
    pub path: PathBuf,
    /// Symlink target, for runtimes linked from elsewhere.
    pub link_target: Option<PathBuf>,
    /// Has a `proton` launcher script, i.e. is usable by umu.
    pub valid: bool,
    pub modified: SystemTime,
}

pub fn is_proton_dir(p: &Path) -> bool {
    p.join("proton").is_file()
}

/// Installed runtimes, most recently installed first.
pub fn list_installed(lib: &Library) -> Vec<Installed> {
    let mut out: Vec<Installed> = fs::read_dir(lib.runtimes_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let path = e.path();
            if name.starts_with('.') || !path.is_dir() {
                return None;
            }
            let link_target = fs::read_link(&path).ok();
            // For links, the link's own mtime is when it was added.
            let modified = fs::symlink_metadata(&path).and_then(|m| m.modified()).ok()?;
            Some(Installed { valid: is_proton_dir(&path), name, path, link_target, modified })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The runtime a game will use, as `(display name, path)`.
pub fn resolve(lib: &Library, lock: &LockFile, game: &Game) -> Result<(String, PathBuf)> {
    let (name, source) = match (&game.runtime, &lock.settings.default_runtime) {
        (Some(r), _) => (r.clone(), "game setting"),
        (None, Some(r)) => (r.clone(), "library default"),
        (None, None) => {
            let installed = list_installed(lib);
            let rt = installed.iter().find(|r| r.valid).ok_or_else(|| {
                anyhow!(
                    "no Proton runtime installed in {} — install one with \
                     `gameshelf runtime install ge` or from the TUI (R)",
                    lib.runtimes_dir().display()
                )
            })?;
            return Ok((rt.name.clone(), rt.path.clone()));
        }
    };
    let path = runtime_path(lib, &name);
    if !is_proton_dir(&path) {
        bail!("runtime {name:?} ({source}) is not a Proton build: {} has no `proton` script", path.display());
    }
    Ok((name, path))
}

pub fn runtime_path(lib: &Library, name: &str) -> PathBuf {
    if name.contains('/') { expand_tilde(name) } else { lib.runtimes_dir().join(name) }
}

// ---------------------------------------------------------------------------
// Downloadable runtimes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    GeProton,
    UmuProton,
    CachyOs,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::GeProton, Source::UmuProton, Source::CachyOs];

    pub fn key(self) -> &'static str {
        match self {
            Source::GeProton => "ge",
            Source::UmuProton => "umu",
            Source::CachyOs => "cachyos",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::GeProton => "GE-Proton",
            Source::UmuProton => "UMU-Proton",
            Source::CachyOs => "Proton-CachyOS",
        }
    }

    pub fn repo(self) -> &'static str {
        match self {
            Source::GeProton => "GloriousEggroll/proton-ge-custom",
            Source::UmuProton => "Open-Wine-Components/umu-proton",
            Source::CachyOs => "CachyOS/proton-cachyos",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|src| src.key().eq_ignore_ascii_case(s))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Available {
    pub source: Source,
    pub tag: String,
    pub published: String,
    /// Archive name without extension; usually also the extracted directory.
    pub name: String,
    pub asset: String,
    pub url: String,
    pub size: u64,
    pub sha512_url: Option<String>,
    /// GitHub's own `sha256:` digest of the asset, when present.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

const ARCHIVE_EXTS: [&str; 3] = [".tar.gz", ".tar.xz", ".tgz"];

fn archive_stem(name: &str) -> Option<&str> {
    ARCHIVE_EXTS.iter().find_map(|ext| name.strip_suffix(ext))
}

fn matches_host_arch(name: &str) -> bool {
    let lower = name.to_lowercase();
    let is_arm = lower.contains("aarch64") || lower.contains("arm64");
    match std::env::consts::ARCH {
        "aarch64" => is_arm,
        _ => !is_arm,
    }
}

const GITHUB_API: &str = "https://api.github.com";

/// How long a fetched release list is reused before asking GitHub again.
/// Unauthenticated API calls are limited to 60 per hour.
const RELEASES_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

fn github_base() -> String {
    std::env::var("GAMESHELF_GITHUB_API")
        .unwrap_or_else(|_| GITHUB_API.into())
        .trim_end_matches('/')
        .to_string()
}

/// A GitHub token for higher rate limits: `GITHUB_TOKEN`, `GH_TOKEN`, or the
/// GitHub CLI's login. Only ever sent to api.github.com.
fn github_token() -> Option<&'static str> {
    static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TOKEN
        .get_or_init(|| {
            if github_base() != GITHUB_API {
                return None;
            }
            for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
                if let Ok(t) = std::env::var(var)
                    && !t.trim().is_empty()
                {
                    return Some(t.trim().to_string());
                }
            }
            let out = std::process::Command::new("gh")
                .args(["auth", "token"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (out.status.success() && !token.is_empty()).then_some(token)
        })
        .as_deref()
}

fn agent(global_timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(global_timeout)
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(concat!("gameshelf/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn releases_cache_path(source: Source) -> Option<PathBuf> {
    // Keyed by API base so a test/mirror server never pollutes the real cache.
    let base_hash = hex::encode(&Sha256::digest(github_base().as_bytes())[..4]);
    dirs::cache_dir().map(|d| d.join("gameshelf").join(format!("releases-{}-{base_hash}.json", source.key())))
}

/// Recent releases of `source`, newest first, one entry per usable archive.
pub fn fetch_available(source: Source) -> Result<Vec<Available>> {
    let text = releases_json(source)?;
    let releases: Vec<GhRelease> =
        serde_json::from_str(&text).with_context(|| format!("decoding {} releases", source.label()))?;
    Ok(parse_releases(source, releases))
}

/// Release list JSON: from a fresh cache, else GitHub, else a stale cache.
fn releases_json(source: Source) -> Result<String> {
    let cache = releases_cache_path(source);
    let cached = |max_age: Option<Duration>| -> Option<String> {
        let path = cache.as_ref()?;
        let age = fs::metadata(path).ok()?.modified().ok()?.elapsed().unwrap_or_default();
        if max_age.is_some_and(|m| age > m) {
            return None;
        }
        fs::read_to_string(path).ok()
    };
    if let Some(text) = cached(Some(RELEASES_CACHE_TTL)) {
        return Ok(text);
    }
    match fetch_releases_live(source) {
        Ok(text) => {
            if let Some(path) = &cache {
                let _ = fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
                let _ = fs::write(path, &text);
            }
            Ok(text)
        }
        Err(e) => cached(None).ok_or(e),
    }
}

fn fetch_releases_live(source: Source) -> Result<String> {
    let url = format!("{}/repos/{}/releases?per_page=30", github_base(), source.repo());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("gameshelf/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(&url).header("Accept", "application/vnd.github+json");
    if let Some(token) = github_token() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let mut resp = req.call().with_context(|| format!("listing {} releases", source.label()))?;
    let status = resp.status();
    let body = resp
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_string()
        .with_context(|| format!("reading {} releases", source.label()))?;
    if status.is_success() {
        return Ok(body);
    }
    if matches!(status.as_u16(), 403 | 429) && body.contains("rate limit") {
        bail!(
            "GitHub API rate limit exceeded — try again later, or set GITHUB_TOKEN \
             (or log in with `gh auth login`)"
        );
    }
    bail!("listing {} releases: GitHub returned {status}", source.label())
}

fn parse_releases(source: Source, releases: Vec<GhRelease>) -> Vec<Available> {
    let mut out = Vec::new();
    for rel in releases.into_iter().filter(|r| !r.draft && !r.prerelease) {
        for asset in &rel.assets {
            let Some(stem) = archive_stem(&asset.name) else { continue };
            if !matches_host_arch(&asset.name) {
                continue;
            }
            let sum_name = format!("{stem}.sha512sum");
            out.push(Available {
                source,
                tag: rel.tag_name.clone(),
                published: rel.published_at.clone().unwrap_or_default().chars().take(10).collect(),
                name: stem.to_string(),
                asset: asset.name.clone(),
                url: asset.browser_download_url.clone(),
                size: asset.size,
                sha512_url: rel
                    .assets
                    .iter()
                    .find(|a| a.name == sum_name)
                    .map(|a| a.browser_download_url.clone()),
                sha256: asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_string),
            });
        }
    }
    out
}

/// Find a release by `<source>` (latest), `<source>:<tag>` or an exact
/// archive/runtime name like `GE-Proton11-7-x86_64`.
pub fn find_available(spec: &str) -> Result<Available> {
    let (source_key, rest) = spec.split_once(':').unwrap_or((spec, ""));
    if let Some(source) = Source::from_key(source_key) {
        let all = fetch_available(source)?;
        let found = if rest.is_empty() {
            all.into_iter().next()
        } else {
            all.into_iter().find(|a| a.tag == rest || a.name == rest)
        };
        return found.ok_or_else(|| anyhow!("no {} release matches {spec:?}", source.label()));
    }
    // Ask the source the name points at first, to avoid hitting every repo.
    let lower = spec.to_lowercase();
    let likely = if lower.starts_with("ge-proton") {
        Some(Source::GeProton)
    } else if lower.starts_with("umu-proton") {
        Some(Source::UmuProton)
    } else if lower.contains("cachyos") {
        Some(Source::CachyOs)
    } else {
        None
    };
    let mut order: Vec<Source> = Source::ALL.to_vec();
    if let Some(l) = likely {
        order.retain(|s| *s != l);
        order.insert(0, l);
    }
    for source in order {
        if let Some(a) = fetch_available(source)?
            .into_iter()
            .find(|a| a.name.eq_ignore_ascii_case(spec) || a.tag.eq_ignore_ascii_case(spec))
        {
            return Ok(a);
        }
    }
    bail!("no release named {spec:?} (try `gameshelf runtime available`)")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Downloading,
    Verifying,
    Extracting,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

/// Download, verify and unpack a release into `.proton-runtimes`. Returns
/// the installed runtime name. `cancel` aborts the download/extraction.
pub fn install(
    lib: &Library,
    avail: &Available,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<String> {
    let dir = lib.runtimes_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if dir.join(&avail.name).exists() {
        bail!("{} is already installed", avail.name);
    }

    let part = dir.join(format!(".{}.part", avail.asset));
    let staging = dir.join(format!(".{}.extract", avail.name));
    let result = (|| {
        let expected_sha512 = match &avail.sha512_url {
            Some(url) => Some(fetch_checksum(url, &avail.asset)?),
            None => None,
        };
        let (sha512, sha256) = download(&avail.url, &part, avail.size, cancel, progress)?;

        progress(Progress { phase: Phase::Verifying, done: 0, total: 0 });
        if let Some(want) = &expected_sha512 {
            if !want.eq_ignore_ascii_case(&sha512) {
                bail!("SHA-512 mismatch for {}: expected {want}, got {sha512}", avail.asset);
            }
        } else if let Some(want) = &avail.sha256
            && !want.eq_ignore_ascii_case(&sha256)
        {
            bail!("SHA-256 mismatch for {}: expected {want}, got {sha256}", avail.asset);
        }

        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        extract(&part, &avail.asset, &staging, cancel, progress)?;
        let name = finalize(&staging, &dir, &avail.name)?;
        Ok(name)
    })();
    let _ = fs::remove_file(&part);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn fetch_checksum(url: &str, asset: &str) -> Result<String> {
    let text = agent(Some(Duration::from_secs(30)))
        .get(url)
        .call()
        .with_context(|| format!("fetching checksum {url}"))?
        .body_mut()
        .read_to_string()?;
    parse_checksum(&text, asset).ok_or_else(|| anyhow!("no checksum for {asset} in {url}"))
}

/// Parse `sha512sum` output; accepts a bare hash or `<hash>  <file>` lines.
fn parse_checksum(text: &str, asset: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    for line in &lines {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        match parts.next() {
            Some(file) if file.trim_start_matches('*').ends_with(asset) => {
                return Some(hash.to_lowercase());
            }
            None if lines.len() == 1 => return Some(hash.to_lowercase()),
            _ => {}
        }
    }
    None
}

/// Stream to `dest`, hashing as we go. Returns hex `(sha512, sha256)`.
fn download(
    url: &str,
    dest: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<(String, String)> {
    let resp = agent(None).get(url).call().with_context(|| format!("downloading {url}"))?;
    let total = resp.body().content_length().unwrap_or(expected_size);
    let mut reader = resp.into_body().into_reader();
    let mut out =
        io::BufWriter::new(fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?);
    let mut sha512 = Sha512::new();
    let mut sha256 = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let mut last_report = 0u64;
    progress(Progress { phase: Phase::Downloading, done, total });
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("reading download")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        sha512.update(&buf[..n]);
        sha256.update(&buf[..n]);
        done += n as u64;
        if done - last_report >= 1 << 20 {
            last_report = done;
            progress(Progress { phase: Phase::Downloading, done, total });
        }
    }
    out.flush()?;
    progress(Progress { phase: Phase::Downloading, done, total: total.max(done) });
    Ok((hex::encode(sha512.finalize()), hex::encode(sha256.finalize())))
}

struct CountingReader<'a, R> {
    inner: R,
    count: u64,
    total: u64,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(Progress),
    last: u64,
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        if self.count - self.last >= 1 << 20 || n == 0 {
            self.last = self.count;
            (self.progress)(Progress { phase: Phase::Extracting, done: self.count, total: self.total });
        }
        Ok(n)
    }
}

fn extract(
    archive: &Path,
    asset: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let file = fs::File::open(archive)?;
    let total = file.metadata()?.len();
    let counting =
        CountingReader { inner: io::BufReader::new(file), count: 0, total, cancel, progress, last: 0 };
    let decoder: Box<dyn Read> = if asset.ends_with(".tar.xz") {
        Box::new(liblzma::read::XzDecoder::new(counting))
    } else {
        Box::new(flate2::read::GzDecoder::new(counting))
    };
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    for entry in tar.entries().context("reading archive")? {
        let mut entry = entry.context("reading archive entry")?;
        // `unpack_in` refuses paths that escape `dest`.
        entry.unpack_in(dest).with_context(|| {
            format!("extracting {}", entry.path().map(|p| p.display().to_string()).unwrap_or_default())
        })?;
    }
    Ok(())
}

/// Move the extracted runtime into place. Archives normally contain a single
/// top-level directory, which becomes the runtime; otherwise the staging
/// directory itself is used under `fallback_name`.
fn finalize(staging: &Path, runtimes: &Path, fallback_name: &str) -> Result<String> {
    let entries: Vec<_> = fs::read_dir(staging)?.flatten().collect();
    let (src, name) = match entries.as_slice() {
        [only] if only.path().is_dir() => (only.path(), only.file_name().to_string_lossy().into_owned()),
        _ => (staging.to_path_buf(), fallback_name.to_string()),
    };
    if !is_proton_dir(&src) {
        bail!("the archive doesn't look like a Proton build (no `proton` script)");
    }
    let target = runtimes.join(&name);
    if target.exists() {
        bail!("{name} is already installed");
    }
    fs::rename(&src, &target).with_context(|| format!("moving runtime to {}", target.display()))?;
    Ok(name)
}

// ---------------------------------------------------------------------------
// Existing installs

/// Proton builds in Steam's `compatibilitytools.d` directories.
pub fn steam_compat_tools() -> Vec<(String, PathBuf)> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let roots = [
        home.join(".local/share/Steam/compatibilitytools.d"),
        home.join(".steam/root/compatibilitytools.d"),
        home.join(".steam/steam/compatibilitytools.d"),
        home.join(".var/app/com.valvesoftware.Steam/data/Steam/compatibilitytools.d"),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
            let path = entry.path();
            if !is_proton_dir(&path) {
                continue;
            }
            let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.insert(canon) {
                out.push((entry.file_name().to_string_lossy().into_owned(), path));
            }
        }
    }
    out.sort();
    out
}

/// Symlink an existing Proton directory into `.proton-runtimes`.
pub fn link(lib: &Library, target: &Path, name: Option<&str>) -> Result<String> {
    let target = fs::canonicalize(target).with_context(|| format!("resolving {}", target.display()))?;
    if !is_proton_dir(&target) {
        bail!("{} is not a Proton build (no `proton` script)", target.display());
    }
    let name = match name {
        Some(n) => n.to_string(),
        None => target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("can't name runtime {}", target.display()))?,
    };
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid runtime name {name:?}");
    }
    let dir = lib.runtimes_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(&name);
    if dest.exists() || dest.is_symlink() {
        bail!("a runtime named {name} already exists");
    }
    std::os::unix::fs::symlink(&target, &dest).with_context(|| format!("linking {}", dest.display()))?;
    Ok(name)
}

/// Remove an installed runtime (only the link, for linked runtimes).
pub fn remove(lib: &Library, name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid runtime name {name:?}");
    }
    let path = lib.runtimes_dir().join(name);
    if path.is_symlink() {
        fs::remove_file(&path)?;
    } else if path.is_dir() {
        fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        bail!("runtime {name} is not installed");
    }
    Ok(())
}

/// Games whose explicit runtime setting is `name`.
pub fn users_of<'a>(lock: &'a LockFile, name: &str) -> Vec<&'a str> {
    lock.games.iter().filter(|(_, g)| g.runtime.as_deref() == Some(name)).map(|(d, _)| d.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            published_at: Some("2026-09-16T02:28:16Z".into()),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|a| GhAsset {
                    name: a.to_string(),
                    browser_download_url: format!("https://example.invalid/{a}"),
                    size: 1,
                    digest: Some("sha256:abc".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn picks_archives_for_host_arch() {
        let avail = parse_releases(
            Source::GeProton,
            vec![rel(
                "GE-Proton11-7",
                &[
                    "GE-Proton11-7-aarch64.sha512sum",
                    "GE-Proton11-7-aarch64.tar.gz",
                    "GE-Proton11-7-x86_64.sha512sum",
                    "GE-Proton11-7-x86_64.tar.gz",
                ],
            )],
        );
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(avail.len(), 1);
            let a = &avail[0];
            assert_eq!(a.name, "GE-Proton11-7-x86_64");
            assert_eq!(a.published, "2026-09-16");
            assert_eq!(a.sha256.as_deref(), Some("abc"));
            assert!(a.sha512_url.as_deref().unwrap().ends_with("GE-Proton11-7-x86_64.sha512sum"));
        }
    }

    #[test]
    fn cachyos_variants_and_umu() {
        let avail = parse_releases(
            Source::CachyOs,
            vec![rel(
                "cachyos-11.0-20261005-slr",
                &[
                    "proton-cachyos-11.0-20261005-slr-arm64.tar.xz",
                    "proton-cachyos-11.0-20261005-slr-x86_64.tar.xz",
                    "proton-cachyos-11.0-20261005-slr-x86_64_v3.tar.xz",
                ],
            )],
        );
        if std::env::consts::ARCH == "x86_64" {
            let names: Vec<_> = avail.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(
                names,
                ["proton-cachyos-11.0-20261005-slr-x86_64", "proton-cachyos-11.0-20261005-slr-x86_64_v3"]
            );
            assert!(avail[0].sha512_url.is_none());
        }
        // No arch marker at all (UMU-Proton) is fine on x86_64.
        let umu =
            parse_releases(Source::UmuProton, vec![rel("UMU-Proton-10.0-4", &["UMU-Proton-10.0-4.tar.gz"])]);
        assert_eq!(umu.len(), usize::from(std::env::consts::ARCH == "x86_64"));
    }

    #[test]
    fn checksum_formats() {
        let h = "7db8".to_string();
        assert_eq!(
            parse_checksum("7DB8  GE-Proton11-7-x86_64.tar.gz\n", "GE-Proton11-7-x86_64.tar.gz"),
            Some(h.clone())
        );
        assert_eq!(
            parse_checksum("7db8 *GE-Proton11-7-x86_64.tar.gz", "GE-Proton11-7-x86_64.tar.gz"),
            Some(h.clone())
        );
        assert_eq!(parse_checksum("7db8\n", "x.tar.gz"), Some(h));
        assert_eq!(parse_checksum("aaaa  other.tar.gz\nbbbb  more.tar.gz", "x.tar.gz"), None);
    }

    #[test]
    fn link_and_remove() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path().join("lib"), true).unwrap();
        let proton = t.path().join("GE-Proton11-7-x86_64");
        fs::create_dir_all(&proton).unwrap();
        assert!(link(&lib, &proton, None).is_err(), "not a proton dir yet");
        fs::write(proton.join("proton"), "#!/bin/sh\n").unwrap();

        assert_eq!(link(&lib, &proton, None).unwrap(), "GE-Proton11-7-x86_64");
        assert!(link(&lib, &proton, None).is_err(), "duplicate name");
        let installed = list_installed(&lib);
        assert_eq!(installed.len(), 1);
        assert!(installed[0].valid);
        assert!(installed[0].link_target.is_some());

        remove(&lib, "GE-Proton11-7-x86_64").unwrap();
        assert!(proton.join("proton").exists(), "removing a link keeps the target");
        assert!(list_installed(&lib).is_empty());
        assert!(remove(&lib, "../etc").is_err());
    }

    #[test]
    fn resolve_order() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path().to_path_buf(), false).unwrap();
        let mut lock = LockFile::default();
        let game = Game::default();
        assert!(resolve(&lib, &lock, &game).is_err(), "nothing installed");

        for n in ["A", "B"] {
            let p = lib.runtimes_dir().join(n);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("proton"), "").unwrap();
        }
        // Without a default, the most recently installed one wins.
        let b = lib.runtimes_dir().join("B");
        let later = SystemTime::now() + Duration::from_secs(60);
        fs::File::open(&b).unwrap().set_modified(later).unwrap();
        assert_eq!(resolve(&lib, &lock, &game).unwrap().0, "B");

        lock.settings.default_runtime = Some("A".into());
        assert_eq!(resolve(&lib, &lock, &game).unwrap().0, "A");
        let game = Game { runtime: Some("B".into()), ..Default::default() };
        assert_eq!(resolve(&lib, &lock, &game).unwrap().0, "B");
        let game = Game { runtime: Some("missing".into()), ..Default::default() };
        assert!(resolve(&lib, &lock, &game).is_err());
    }
}
