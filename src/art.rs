//! Steam artwork: finding asset URLs, a download cache, decoding.
//!
//! Library artwork (the vertical cover and the wide hero) lives on Steam's CDN
//! under hashed paths for newer apps, so the URLs come from
//! `IStoreBrowseService/GetItems`. Everything is cached under
//! `$XDG_CACHE_HOME/gameshelf/art/<appid>/`; artwork an app doesn't have is
//! remembered with a `.missing` marker so it isn't requested again.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use image::DynamicImage;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtKind {
    /// Vertical library capsule, 600×900.
    Cover,
    /// Wide library hero, 3840×1240 (falls back to the store header).
    Hero,
}

impl ArtKind {
    fn name(self) -> &'static str {
        match self {
            ArtKind::Cover => "cover",
            ArtKind::Hero => "hero",
        }
    }

    /// Asset keys to try, best first.
    fn candidates(self) -> &'static [&'static str] {
        match self {
            ArtKind::Cover => &["library_capsule_2x", "library_capsule"],
            ArtKind::Hero => &["library_hero", "header_2x", "header"],
        }
    }

    /// Width / height of the artwork, used to lay out space before it loads.
    pub fn aspect(self) -> f64 {
        match self {
            ArtKind::Cover => 600.0 / 900.0,
            ArtKind::Hero => 3840.0 / 1240.0,
        }
    }
}

const ASSETS_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

fn api_base() -> String {
    std::env::var("GAMESHELF_STEAM_API").unwrap_or_else(|_| "https://api.steampowered.com".into())
}

fn cdn_base() -> String {
    std::env::var("GAMESHELF_STEAM_CDN")
        .unwrap_or_else(|_| "https://shared.akamai.steamstatic.com/store_item_assets".into())
}

/// Write via a uniquely named temp file and rename, so concurrent loaders
/// never see (or produce) a half-written file.
fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}-{n}", std::process::id()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn art_dir(appid: u32) -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("gameshelf").join("art").join(appid.to_string()))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("gameshelf/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .build()
        .into()
}

/// The `assets` object of a GetItems store item.
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Assets {
    asset_url_format: String,
    #[serde(flatten)]
    files: HashMap<String, serde_json::Value>,
}

impl Assets {
    fn url(&self, key: &str) -> Option<String> {
        let file = self.files.get(key)?.as_str()?;
        let path = self.asset_url_format.replace("${FILENAME}", file);
        Some(format!("{}/{}", cdn_base().trim_end_matches('/'), path.trim_start_matches('/')))
    }
}

#[derive(Deserialize)]
struct GetItems {
    response: GetItemsResponse,
}

#[derive(Deserialize)]
struct GetItemsResponse {
    #[serde(default)]
    store_items: Vec<StoreItem>,
}

#[derive(Deserialize)]
struct StoreItem {
    #[serde(default)]
    appid: Option<u32>,
    #[serde(default)]
    assets: Option<Assets>,
}

fn parse_get_items(text: &str, appid: u32) -> Result<Option<Assets>> {
    let parsed: GetItems = serde_json::from_str(text).context("decoding Steam asset list")?;
    Ok(parsed.response.store_items.into_iter().find(|i| i.appid == Some(appid)).and_then(|i| i.assets))
}

fn fetch_assets(appid: u32) -> Result<Option<Assets>> {
    let input = serde_json::json!({
        "ids": [{ "appid": appid }],
        "context": { "language": "english", "country_code": "US" },
        "data_request": { "include_assets": true },
    });
    let url = format!("{}/IStoreBrowseService/GetItems/v1/", api_base().trim_end_matches('/'));
    let mut resp = agent()
        .get(&url)
        .query("input_json", input.to_string())
        .call()
        .with_context(|| format!("listing artwork for app {appid}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("listing artwork for app {appid}: HTTP {}", resp.status());
    }
    parse_get_items(&resp.body_mut().read_to_string()?, appid)
}

/// The app's asset list, from the cache when fresh enough.
fn assets(appid: u32, offline: bool) -> Result<Option<Assets>> {
    let dir = art_dir(appid).context("no cache directory")?;
    let path = dir.join("assets.json");
    let cached = || -> Option<Option<Assets>> { serde_json::from_str(&fs::read_to_string(&path).ok()?).ok() };
    let fresh = fs::metadata(&path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() < ASSETS_MAX_AGE);
    if fresh || offline {
        if let Some(a) = cached() {
            return Ok(a);
        }
        if offline {
            return Ok(None);
        }
    }
    match fetch_assets(appid) {
        Ok(a) => {
            fs::create_dir_all(&dir)?;
            write_atomic(&path, serde_json::to_string(&a.as_ref().map(AssetsOut::from))?.as_bytes())?;
            Ok(a)
        }
        // Stale is better than nothing when Steam is unreachable.
        Err(e) => cached().ok_or(e),
    }
}

/// Serializable mirror of [`Assets`] for the cache file.
#[derive(serde::Serialize)]
struct AssetsOut<'a> {
    asset_url_format: &'a str,
    #[serde(flatten)]
    files: &'a HashMap<String, serde_json::Value>,
}

impl<'a> From<&'a Assets> for AssetsOut<'a> {
    fn from(a: &'a Assets) -> Self {
        Self { asset_url_format: &a.asset_url_format, files: &a.files }
    }
}

/// Download `url`; `Ok(None)` if the CDN doesn't have it.
fn download(url: &str) -> Result<Option<Vec<u8>>> {
    let mut resp = agent().get(url).call().with_context(|| format!("downloading {url}"))?;
    if resp.status().as_u16() == 404 || resp.status().as_u16() == 403 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("downloading {url}: HTTP {}", resp.status());
    }
    let mut buf = Vec::new();
    resp.body_mut().as_reader().take(64 * 1024 * 1024).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// Local file with the artwork, downloading it if needed. `Ok(None)` means
/// the app has no such artwork (or we're offline and it isn't cached).
pub fn image_path(appid: u32, kind: ArtKind, offline: bool) -> Result<Option<PathBuf>> {
    let dir = art_dir(appid).context("no cache directory")?;
    let file = dir.join(format!("{}.img", kind.name()));
    let missing = dir.join(format!("{}.missing", kind.name()));
    if file.is_file() {
        return Ok(Some(file));
    }
    if missing.exists() || offline {
        return Ok(None);
    }
    let Some(assets) = assets(appid, offline)? else {
        fs::create_dir_all(&dir)?;
        fs::write(&missing, "")?;
        return Ok(None);
    };
    for key in kind.candidates() {
        let Some(url) = assets.url(key) else { continue };
        if let Some(bytes) = download(&url)? {
            fs::create_dir_all(&dir)?;
            write_atomic(&file, &bytes)?;
            return Ok(Some(file));
        }
    }
    fs::create_dir_all(&dir)?;
    fs::write(&missing, "")?;
    Ok(None)
}

pub fn load(appid: u32, kind: ArtKind, offline: bool) -> Result<Option<DynamicImage>> {
    let Some(path) = image_path(appid, kind, offline)? else { return Ok(None) };
    let img = image::ImageReader::open(&path)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;
    Ok(Some(img))
}

/// A vivid, readable accent colour for an image: the average of its more
/// saturated pixels, brightened for use as text on a dark background.
pub fn accent(img: &DynamicImage) -> (u8, u8, u8) {
    let small = img.thumbnail(48, 48).to_rgb8();
    let (mut r, mut g, mut b, mut w) = (0f64, 0f64, 0f64, 0f64);
    for p in small.pixels() {
        let [pr, pg, pb] = p.0.map(|c| c as f64 / 255.0);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        // Favour saturated, mid-to-bright pixels.
        let weight = sat * sat * (0.2 + max) + 1e-4;
        r += pr * weight;
        g += pg * weight;
        b += pb * weight;
        w += weight;
    }
    let (r, g, b) = (r / w, g / w, b / w);
    // Raise lightness to 0.65–0.8 and, for images that have a hue at all,
    // saturation to at least 0.45 (a grey image stays neutral).
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let s = if s < 0.08 { s } else { s.clamp(0.45, 0.9) };
    let (r, g, b) = hsl_to_rgb(h, s, l.clamp(0.65, 0.8));
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEMS: &str = r#"{"response":{"store_items":[{"item_type":0,"id":499170,"appid":499170,
        "name":"Order of the Sinking Star","assets":{
        "asset_url_format":"steam/apps/499170/${FILENAME}?t=1791571622",
        "library_capsule":"e061e665/library_capsule.jpg",
        "library_capsule_2x":"e061e665/library_capsule_2x.jpg",
        "header":"deda420d/header.jpg","last_modified":1791571622}}]}}"#;

    #[test]
    fn parses_asset_urls() {
        let a = parse_get_items(ITEMS, 499170).unwrap().unwrap();
        assert_eq!(
            a.url("library_capsule_2x").unwrap(),
            format!("{}/steam/apps/499170/e061e665/library_capsule_2x.jpg?t=1791571622", cdn_base())
        );
        assert_eq!(a.url("library_hero"), None);
        assert_eq!(a.url("last_modified"), None, "non-string fields aren't files");
        assert_eq!(parse_get_items(ITEMS, 1).unwrap(), None);
        assert_eq!(parse_get_items(r#"{"response":{}}"#, 1).unwrap(), None);
    }

    #[test]
    fn assets_cache_roundtrip() {
        let a = parse_get_items(ITEMS, 499170).unwrap().unwrap();
        let text = serde_json::to_string(&Some(AssetsOut::from(&a))).unwrap();
        let back: Option<Assets> = serde_json::from_str(&text).unwrap();
        assert_eq!(back.unwrap(), a);
        let none: Option<Assets> = serde_json::from_str("null").unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn accent_is_vivid_and_readable() {
        let mut img = image::RgbImage::from_pixel(32, 32, image::Rgb([20, 20, 30]));
        for x in 0..32 {
            for y in 0..8 {
                img.put_pixel(x, y, image::Rgb([200, 40, 40]));
            }
        }
        let (r, g, b) = accent(&DynamicImage::ImageRgb8(img));
        assert!(r > g && r > b, "picks up the red: {r},{g},{b}");
        let (_, s, l) = rgb_to_hsl(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
        assert!(l >= 0.64 && s >= 0.44, "{s} {l}");

        let gray = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([90, 90, 90])));
        let (r, g, b) = accent(&gray);
        assert!(r >= 160 && g >= 160 && b >= 160, "grey images give a light neutral: {r},{g},{b}");
    }

    #[test]
    fn hsl_roundtrip() {
        for &(r, g, b) in &[(0.8, 0.2, 0.1), (0.1, 0.5, 0.9), (0.3, 0.3, 0.3), (0.0, 1.0, 0.5)] {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let (r2, g2, b2) = hsl_to_rgb(h, s, l);
            assert!((r - r2).abs() < 1e-6 && (g - g2).abs() < 1e-6 && (b - b2).abs() < 1e-6);
        }
    }
}
