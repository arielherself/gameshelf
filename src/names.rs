//! Turning directory names into words: search queries, prefix names and
//! comparison keys.

/// Split a directory/file name into words.
///
/// Handles `snake_case`, `kebab-case`, dotted names and `CamelCase`
/// (`OrderOfTheSinkingStar` → `Order Of The Sinking Star`), keeps acronyms
/// together (`VVVVVV`, `XCOM2` → `XCOM 2`), and drops bracketed noise such
/// as `[GOG]` or `(v1.2)`.
pub fn words(name: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => cleaned.push(c),
        }
    }

    let cleaned = strip_versions(&cleaned);
    let mut out = Vec::new();
    for chunk in cleaned.split(|c: char| !c.is_alphanumeric() && c != '\'') {
        let chars: Vec<char> = chunk.chars().filter(|&c| c != '\'').collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 && !cur.is_empty() {
                let prev = chars[i - 1];
                let next = chars.get(i + 1).copied();
                let boundary = (prev.is_lowercase() && c.is_uppercase())
                    || (prev.is_alphabetic() && c.is_ascii_digit())
                    || (prev.is_ascii_digit() && c.is_alphabetic())
                    // "HTMLParser" → "HTML Parser": an upper followed by a lower
                    // ends a run of capitals.
                    || (prev.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(|n| n.is_lowercase()));
                if boundary {
                    out.push(std::mem::take(&mut cur));
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out.retain(|w| !is_noise_word(w));
    out
}

/// Blank out version strings (`v1.4.0.0`, `1.0.2`, `v12`) at word boundaries.
fn strip_versions(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if at_boundary && let Some(len) = version_len(&chars[i..]) {
            out.push(' ');
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Length of a version string at the start of `c`, if there is one.
fn version_len(c: &[char]) -> Option<usize> {
    let has_v = matches!(c.first(), Some('v' | 'V'));
    let mut i = usize::from(has_v);
    let mut groups = 0;
    loop {
        let start = i;
        while c.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            // A trailing dot isn't part of the version.
            if groups > 0 {
                i -= 1;
            }
            break;
        }
        groups += 1;
        if c.get(i) == Some(&'.') {
            i += 1;
        } else {
            break;
        }
    }
    let ends_at_boundary = !c.get(i).is_some_and(|ch| ch.is_alphanumeric());
    let is_version = groups >= 2 || (has_v && groups == 1);
    (is_version && ends_at_boundary).then_some(i)
}

/// Release/packaging tags that never appear in a store title.
fn is_noise_word(w: &str) -> bool {
    matches!(
        w.to_lowercase().as_str(),
        "gog" | "repack" | "portable" | "win" | "win64" | "win32" | "x64" | "x86"
    )
}

/// Human-friendly search query for a directory name.
pub fn search_query(name: &str) -> String {
    words(name).join(" ")
}

/// `snake_case` form used for new wine prefix names.
pub fn snake_case(name: &str) -> String {
    let s = words(name).iter().map(|w| w.to_lowercase()).collect::<Vec<_>>().join("_");
    if s.is_empty() { "game".to_string() } else { s }
}

/// Lowercase alphanumerics only, for spacing- and punctuation-insensitive
/// comparison ("Order of the Sinking Star" == "OrderOfTheSinkingStar").
pub fn compare_key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Words in store titles that mark non-game entries (soundtracks, demos...).
const SIDE_PRODUCT_WORDS: &[&str] = &[
    "soundtrack",
    "ost",
    "demo",
    "dlc",
    "artbook",
    "playtest",
    "prologue",
    "season pass",
    "dedicated server",
    "sdk",
    "editor",
    "bundle",
    "expansion",
    "supporter pack",
    "wallpaper",
];

/// How well a store title matches a local name, in `0.0..=1.0`.
pub fn match_score(local: &str, title: &str) -> f64 {
    let a = compare_key(local);
    let b = compare_key(title);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut score = strsim::normalized_levenshtein(&a, &b);

    // Penalise side products unless the local name mentions them too.
    let local_lower = search_query(local).to_lowercase();
    let title_lower = title.to_lowercase();
    for w in SIDE_PRODUCT_WORDS {
        if contains_word(&title_lower, w) && !contains_word(&local_lower, w) {
            score -= 0.3;
        }
    }
    score.clamp(0.0, 1.0)
}

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_case() {
        assert_eq!(search_query("OrderOfTheSinkingStar"), "Order Of The Sinking Star");
        assert_eq!(search_query("TheFinalStation"), "The Final Station");
        assert_eq!(search_query("PrisonEscapeSimulator"), "Prison Escape Simulator");
        assert_eq!(search_query("LevelDevil"), "Level Devil");
    }

    #[test]
    fn keeps_acronyms_and_digits() {
        assert_eq!(search_query("VVVVVV"), "VVVVVV");
        assert_eq!(search_query("XCOM2"), "XCOM 2");
        assert_eq!(search_query("HTMLParser"), "HTML Parser");
        assert_eq!(search_query("Half-Life_2"), "Half Life 2");
    }

    #[test]
    fn strips_noise() {
        assert_eq!(search_query("Celeste [GOG] v1.4.0.0"), "Celeste");
        assert_eq!(search_query("Hollow.Knight.(2017).Repack"), "Hollow Knight");
        assert_eq!(search_query("Baldur's Gate 3"), "Baldurs Gate 3");
        assert_eq!(search_query("Hades_v1.38290"), "Hades");
        assert_eq!(search_query("Factorio 2.0.55 Repack"), "Factorio");
        // Plain numbers and words starting with v are kept.
        assert_eq!(search_query("Prey 2006"), "Prey 2006");
        assert_eq!(search_query("Vampire Survivors"), "Vampire Survivors");
        assert_eq!(search_query("v2rays"), "v 2 rays");
    }

    #[test]
    fn snake_case_matches_hand_made_prefixes() {
        assert_eq!(snake_case("OrderOfTheSinkingStar"), "order_of_the_sinking_star");
        assert_eq!(snake_case("LevelDevil"), "level_devil");
        assert_eq!(snake_case("VVVVVV"), "vvvvvv");
        assert_eq!(snake_case("iwanna"), "iwanna");
        assert_eq!(snake_case("[]"), "game");
    }

    #[test]
    fn scoring() {
        let exact = match_score("OrderOfTheSinkingStar", "Order of the Sinking Star");
        assert!((exact - 1.0).abs() < 1e-9);
        let ost = match_score("OrderOfTheSinkingStar", "Order of the Sinking Star Soundtrack");
        assert!(ost < 0.5, "{ost}");
        // Mentioning the side product locally removes the penalty.
        assert!((match_score("CelesteSoundtrack", "Celeste Soundtrack") - 1.0).abs() < 1e-9);
        assert!(match_score("iwanna", "I Wanna Be") < 0.8);
        assert_eq!(match_score("", "x"), 0.0);
    }

    #[test]
    fn word_boundaries() {
        assert!(contains_word("celeste demo", "demo"));
        assert!(!contains_word("demon slayer", "demo"));
        assert!(!contains_word("ghost", "ost"));
    }
}
