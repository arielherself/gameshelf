//! Steam store lookups: search for a directory name, fetch app details.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::lockfile::SteamInfo;
use crate::names;
use crate::util::now;

/// Minimum [`names::match_score`] for an automatic match.
pub const MATCH_THRESHOLD: f64 = 0.8;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchItem {
    pub appid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoMatch {
    pub appid: u32,
    pub name: String,
    pub score: f64,
}

pub struct SteamClient {
    agent: ureq::Agent,
    base: String,
}

impl SteamClient {
    pub fn new() -> Self {
        let base = std::env::var("GAMESHELF_STEAM_URL")
            .unwrap_or_else(|_| "https://store.steampowered.com".to_string());
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .user_agent(concat!("gameshelf/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent, base: base.trim_end_matches('/').to_string() }
    }

    pub fn search(&self, term: &str) -> Result<Vec<SearchItem>> {
        let url = format!("{}/api/storesearch/", self.base);
        let resp: SearchResponse = self
            .agent
            .get(&url)
            .query("term", term)
            .query("cc", "us")
            .query("l", "english")
            .call()
            .with_context(|| format!("searching Steam for {term:?}"))?
            .body_mut()
            .read_json()
            .context("decoding Steam search response")?;
        Ok(resp
            .items
            .into_iter()
            .filter(|i| i.kind.as_deref().is_none_or(|k| k == "app"))
            .map(|i| SearchItem { appid: i.id, name: i.name })
            .collect())
    }

    /// Fetch store details. `Ok(None)` means Steam doesn't know the app id.
    pub fn details(&self, appid: u32) -> Result<Option<SteamInfo>> {
        let url = format!("{}/api/appdetails", self.base);
        let text = self
            .agent
            .get(&url)
            .query("appids", appid.to_string())
            .query("cc", "us")
            .query("l", "english")
            .call()
            .with_context(|| format!("fetching Steam details for app {appid}"))?
            .body_mut()
            .read_to_string()
            .context("reading Steam details response")?;
        parse_details(&text, appid)
    }

    /// Search with a few query variants derived from `dir` (and the executable
    /// name, if any) and return the best-scoring title above the threshold.
    pub fn auto_match(&self, dir: &str, exe_stem: Option<&str>) -> Result<Option<AutoMatch>> {
        let mut queries = vec![(names::search_query(dir), dir.to_string())];
        if let Some(stem) = exe_stem {
            queries.push((names::search_query(stem), stem.to_string()));
        }
        queries.retain(|(q, _)| !q.is_empty());
        queries.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));

        let mut best: Option<AutoMatch> = None;
        for (query, local) in &queries {
            for item in self.search(query)? {
                let score = names::match_score(local, &item.name);
                if best.as_ref().is_none_or(|b| score > b.score) {
                    best = Some(AutoMatch { appid: item.appid, name: item.name, score });
                }
            }
            if best.as_ref().is_some_and(|b| b.score >= 0.999) {
                break;
            }
        }
        Ok(best.filter(|b| b.score >= MATCH_THRESHOLD))
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<RawSearchItem>,
}

#[derive(Deserialize)]
struct RawSearchItem {
    id: u32,
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct DetailsEntry {
    success: bool,
    data: Option<RawDetails>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawDetails {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    short_description: String,
    developers: Vec<String>,
    publishers: Vec<String>,
    release_date: Option<RawReleaseDate>,
    genres: Vec<RawGenre>,
    website: Option<String>,
}

#[derive(Deserialize)]
struct RawReleaseDate {
    #[serde(default)]
    date: String,
}

#[derive(Deserialize)]
struct RawGenre {
    description: String,
}

fn parse_details(text: &str, appid: u32) -> Result<Option<SteamInfo>> {
    let map: std::collections::HashMap<String, DetailsEntry> =
        serde_json::from_str(text).context("decoding Steam details response")?;
    let Some(entry) = map.into_iter().find(|(k, _)| k == &appid.to_string()).map(|(_, v)| v) else {
        return Ok(None);
    };
    let Some(d) = entry.data.filter(|_| entry.success) else {
        return Ok(None);
    };
    Ok(Some(SteamInfo {
        name: d.name,
        kind: d.kind,
        short_description: strip_html(&d.short_description),
        developers: d.developers,
        publishers: d.publishers,
        release_date: d.release_date.map(|r| r.date).unwrap_or_default(),
        genres: d.genres.into_iter().map(|g| g.description).collect(),
        website: d.website.unwrap_or_default(),
        fetched_at: now(),
    }))
}

/// Store descriptions occasionally contain markup and entities.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_details() {
        let json = r#"{"499170":{"success":true,"data":{"type":"game","name":"Order of the Sinking Star",
            "steam_appid":499170,"short_description":"From director Jonathan Blow &amp; co.<br>Puzzles.",
            "developers":["Thekla, Inc."],"publishers":["Arc Games"],
            "release_date":{"coming_soon":false,"date":"8 Oct, 2026"},
            "genres":[{"id":"25","description":"Adventure"},{"id":"23","description":"Indie"}],
            "website":null}}}"#;
        let info = parse_details(json, 499170).unwrap().unwrap();
        assert_eq!(info.name, "Order of the Sinking Star");
        assert_eq!(info.kind, "game");
        assert_eq!(info.short_description, "From director Jonathan Blow & co. Puzzles.");
        assert_eq!(info.release_date, "8 Oct, 2026");
        assert_eq!(info.genres, vec!["Adventure", "Indie"]);
        assert_eq!(info.website, "");
    }

    #[test]
    fn unknown_app_is_none() {
        assert_eq!(parse_details(r#"{"1":{"success":false}}"#, 1).unwrap(), None);
        assert_eq!(parse_details(r#"{}"#, 1).unwrap(), None);
    }
}
