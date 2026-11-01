//! Name matching for search. A Mac writes "Café" decomposed (e + U+0301, NFD) while Windows,
//! Linux and a model's query use the composed é (NFC), so both sides are decomposed, stripped
//! of combining marks and lowercased: "cafe", "CAFÉ" and either spelling of "Café" all match.
//! That also makes the search accent-insensitive, which suits a query typed by a model.
//!
//! It lives here, not in a backend, so `search_files` matches the names it lists itself (when
//! it splits a search around turned-off folders) exactly as a backend's search does.

use icu_normalizer::DecomposingNormalizerBorrowed;
use icu_normalizer::properties::CanonicalCombiningClassMapBorrowed;

/// True when `name` contains `query`, ignoring case, accents and Unicode normalization form.
/// For many names against one query, [`NameMatcher`] avoids folding the query each time.
pub fn name_matches(query: &str, name: &str) -> bool {
    NameMatcher::new(query).matches(name)
}

/// Tests names against one query, reusing a buffer so a search over thousands of names
/// doesn't allocate for each.
#[derive(Debug, Clone)]
pub struct NameMatcher {
    needle: String,
    scratch: String,
}

impl NameMatcher {
    pub fn new(query: &str) -> Self {
        let mut needle = String::new();
        fold_into(query, &mut needle);
        if needle.is_empty() {
            // A query of nothing but combining marks; match it as typed rather than everything.
            needle = query.to_lowercase();
        }
        Self {
            needle,
            scratch: String::new(),
        }
    }

    pub fn matches(&mut self, name: &str) -> bool {
        self.scratch.clear();
        fold_into(name, &mut self.scratch);
        self.scratch.contains(&self.needle)
    }
}

/// `text` folded the way names are matched: decomposed, without accents, in lower case. Two
/// paths that fold alike name the same folder as far as the policy is concerned.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    fold_into(text, &mut out);
    out
}

/// Appends `text` decomposed (NFD), without combining marks, in lower case.
fn fold_into(text: &str, out: &mut String) {
    if text.is_ascii() {
        // Most names; ASCII has nothing to decompose.
        let start = out.len();
        out.push_str(text);
        out[start..].make_ascii_lowercase();
        return;
    }
    let combining_class = CanonicalCombiningClassMapBorrowed::new();
    for c in DecomposingNormalizerBorrowed::new_nfd().normalize_iter(text.chars()) {
        // Class 0 are base characters; the rest are marks that attach to them (accents,
        // dakuten, Hebrew points and so on).
        if combining_class.get32_u8(u32::from(c)) == 0 {
            out.extend(c.to_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_case() {
        assert!(name_matches("LEAF", "leaf.txt"));
        assert!(name_matches("leaf", "Big LEAF.TXT"));
        assert!(!name_matches("leaves", "leaf.txt"));
    }

    #[test]
    fn composed_and_decomposed_forms_match_each_other() {
        let nfc = "Caf\u{e9} menu.txt";
        let nfd = "Cafe\u{301} menu.txt";
        assert!(name_matches("caf\u{e9}", nfd));
        assert!(name_matches("cafe\u{301}", nfc));
        assert!(name_matches("CAF\u{c9}", nfd));
        assert!(name_matches("cafe", nfd));
        assert!(name_matches("caf\u{e9}", "cafe.txt"));
        // Beyond Latin: が (U+304C) is か + the combining voiced mark U+3099.
        assert!(name_matches("\u{304c}", "\u{304b}\u{3099}\u{3093}.txt"));
        assert!(name_matches("\u{1e9e}", "stra\u{df}e.txt"));
    }

    #[test]
    fn a_query_of_only_marks_is_not_a_wildcard() {
        assert!(!name_matches("\u{301}", "plain.txt"));
        assert!(!name_matches("\u{301}", "e\u{301}.txt"));
    }

    #[test]
    fn fold_keeps_slashes_and_drops_case_and_accents() {
        assert_eq!(fold("/Photos/Caf\u{e9}"), "/photos/cafe");
        assert_eq!(fold("/PHOTOS/Cafe\u{301}"), "/photos/cafe");
    }

    #[test]
    fn a_matcher_can_be_reused() {
        let mut matcher = NameMatcher::new("Caf\u{e9}");
        assert!(matcher.matches("cafe\u{301}.txt"));
        assert!(!matcher.matches("tea.txt"));
        assert!(matcher.matches("CAFETERIA"));
    }
}
