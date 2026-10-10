use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Expand a leading `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None if p == "~" => dirs::home_dir().unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

/// Show a path with `$HOME` abbreviated to `~`.
pub fn tilde(p: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = p.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    p.display().to_string()
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    match (h, m) {
        (0, 0) if secs > 0 => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m:02}m"),
    }
}

pub fn format_ago(ts: u64, now: u64) -> String {
    let d = now.saturating_sub(ts);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86400 => format!("{} h ago", d / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", d / 86400),
    }
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(125), "2m");
        assert_eq!(format_duration(3 * 3600 + 5 * 60), "3h 05m");
    }

    #[test]
    fn ago() {
        assert_eq!(format_ago(100, 130), "just now");
        assert_eq!(format_ago(0, 7200), "2 h ago");
        assert_eq!(format_ago(0, 86400 * 3), "3 days ago");
        assert_eq!(format_ago(200, 100), "just now");
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(563_784_602), "537.7 MiB");
    }

    #[test]
    fn tilde_roundtrip() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/Games"), home.join("Games"));
        assert_eq!(tilde(&home.join("Games")), "~/Games");
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
