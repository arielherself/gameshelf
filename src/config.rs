//! User configuration: `$XDG_CONFIG_HOME/gameshelf/config.toml`.
//!
//! ```toml
//! library = "~/Games"
//! ```

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util::expand_tilde;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library: Option<String>,
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gameshelf").join("config.toml"))
}

pub fn load() -> Result<Config> {
    let Some(path) = config_path() else { return Ok(Config::default()) };
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn default_library() -> PathBuf {
    expand_tilde("~/Games")
}

/// Where the library is, and whether it's the built-in default (which we
/// create on first use rather than erroring).
pub fn library_path(cli: Option<&str>) -> Result<(PathBuf, bool)> {
    if let Some(p) = cli.filter(|p| !p.is_empty()) {
        return Ok((expand_tilde(p), false));
    }
    if let Some(p) = load()?.library {
        return Ok((expand_tilde(&p), false));
    }
    Ok((default_library(), true))
}
