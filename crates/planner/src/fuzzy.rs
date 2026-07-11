//! Approximate word matching that stays deterministic.
//!
//! The planner must accept human-typed input — "instal nginx", "dokcer",
//! "running the server" — without ever guessing so loosely that it runs the
//! wrong command. Two bounded techniques, no randomness, no models:
//!
//! - a light stemmer (strip one English suffix), so "running" matches "run";
//! - bounded Damerau-Levenshtein distance, so one typo or a transposed pair
//!   matches, and only on words long enough that a single edit cannot turn
//!   one real word into another ("stop" never matches "step").

/// Does the input word count as the keyword, allowing typos and inflection?
pub fn word_matches(word: &str, keyword: &str) -> bool {
    if word == keyword {
        return true;
    }
    let (ws, ks) = (stem(word), stem(keyword));
    if ws == ks {
        return true;
    }
    within_typo_distance(word, keyword) || within_typo_distance(ws, ks)
}

/// Strip one common suffix and any doubled final consonant it exposed
/// ("running" → "runn" → "run"), keeping at least three characters.
/// Deliberately crude: a real stemmer would accept more, and accepting more
/// is the risk.
fn stem(word: &str) -> &str {
    for suffix in ["ing", "es", "ed", "s"] {
        if let Some(base) = word.strip_suffix(suffix) {
            if base.len() >= 3 {
                let mut chars = base.chars().rev();
                if chars.next() == chars.next() && base.len() > 3 {
                    return &base[..base.len() - 1];
                }
                return base;
            }
        }
    }
    word
}

/// One edit, and only for words of five letters or more. Shorter words get
/// no allowance (too many real words are one edit apart), and two edits are
/// never allowed: "reinstall" is two edits from "uninstall", and matching
/// those would run the opposite command.
fn within_typo_distance(a: &str, b: &str) -> bool {
    if a.len().min(b.len()) < 5 {
        return false;
    }
    damerau(a, b) <= 1
}

/// The closest of `options` to `word` within two edits — "did you mean".
/// Deterministic: smallest distance wins, first-listed on a tie. Very short
/// inputs suggest nothing (two edits away from "x" is everything).
pub fn closest<'a>(word: &str, options: &[&'a str]) -> Option<&'a str> {
    options
        .iter()
        .map(|o| (damerau(word, o), *o))
        .filter(|(d, _)| *d <= 2 && d * 2 <= word.len())
        .min_by_key(|(d, _)| *d)
        .map(|(_, o)| o)
}

/// Damerau-Levenshtein (with adjacent transposition), small inputs only.
fn damerau(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return n.max(m);
    }
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typos_and_inflections_match() {
        assert!(word_matches("install", "install"));
        assert!(word_matches("instal", "install")); // dropped letter
        assert!(word_matches("intsall", "install")); // transposition
        assert!(word_matches("installing", "install")); // stem
        assert!(word_matches("running", "run")); // stem then distance
        assert!(word_matches("starts", "start"));
        assert!(word_matches("stopped", "stop"));
        assert!(word_matches("containers", "container"));
        assert!(word_matches("dokcer", "docker"));
        assert!(word_matches("ngnix", "nginx"));
    }

    /// Short words get no allowance: one edit turns too many real words
    /// into each other, and a false match runs the wrong command.
    #[test]
    fn short_words_must_be_exact() {
        assert!(!word_matches("stop", "step"));
        assert!(!word_matches("port", "post"));
        assert!(!word_matches("rum", "run"));
        assert!(!word_matches("user", "use"));
    }

    #[test]
    fn unrelated_words_do_not_match() {
        assert!(!word_matches("delete", "deploy"));
        assert!(!word_matches("firewall", "file"));
        assert!(!word_matches("service", "server")); // 2 edits at len 6-7
                                                     // Two edits apart and opposite in meaning: must never match.
        assert!(!word_matches("reinstall", "uninstall"));
        assert!(!word_matches("reinstall", "install"));
    }

    #[test]
    fn distance_is_damerau() {
        assert_eq!(damerau("abc", "acb"), 1); // transposition is one edit
        assert_eq!(damerau("abc", "abc"), 0);
        assert_eq!(damerau("", "abc"), 3);
    }
}
