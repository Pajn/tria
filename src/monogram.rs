//! The initials and stable colour used by the desktop's default project badge.

use regex::Regex;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

const COLOURS: &[&str] = &[
    "gray", "red", "orange", "amber", "yellow", "lime", "green", "emerald", "teal", "cyan", "sky",
    "blue", "indigo", "violet", "purple", "fuchsia", "pink", "rose",
];
static WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").expect("project name pattern"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{N}").expect("number pattern"));

pub fn fallback(title: &str, workspace_root: &str) -> (String, &'static str) {
    let name = if title.trim().is_empty() {
        workspace_root
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or("project")
    } else {
        title
    };
    let normalized: String = name.nfkc().collect();
    let name = normalized.trim();
    let words: Vec<_> = WORDS.find_iter(name).map(|word| word.as_str()).collect();
    let initials = match words.first() {
        None => "PR".to_string(),
        Some(word) => {
            let first = word.chars().next().unwrap();
            let last = word
                .chars()
                .skip(1)
                .find(|ch| NUMBER.is_match(&ch.to_string()))
                .or_else(|| {
                    if words.len() > 1 {
                        words.last()?.chars().next()
                    } else {
                        word.chars().next_back()
                    }
                })
                .unwrap_or(first);
            format!("{first}{last}")
                .to_uppercase()
                .chars()
                .take(2)
                .collect()
        }
    };
    let lower = name.to_lowercase();
    let hashed = if lower.is_empty() { "project" } else { &lower };
    let colour = hashed.chars().fold(0usize, |index, ch| {
        (index * 31 + ch as usize) % COLOURS.len()
    });
    (initials, COLOURS[colour])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktops_project_names_use_the_same_letters_and_colours() {
        assert_eq!(fallback("Kindra", ""), ("KA".into(), "amber"));
        assert_eq!(fallback("github_ui", ""), ("GU".into(), "gray"));
        assert_eq!(fallback("qk", ""), ("QK".into(), "sky"));
        assert_eq!(fallback("tria", ""), ("TA".into(), "gray"));
        assert_eq!(fallback("T3 Code", "").0, "T3");
        assert_eq!(fallback("backend", "").0, "BD");
        assert_eq!(fallback("MyTestSpec", "").0, "MC");
    }

    #[test]
    fn names_are_normalized_and_empty_or_single_character_names_are_defined() {
        assert_eq!(fallback("  Ｋｉｎｄｒａ  ", ""), fallback("Kindra", ""));
        assert_eq!(fallback("  ", "/src/qk/"), fallback("qk", ""));
        assert_eq!(fallback("x", "").0, "XX");
        assert_eq!(fallback("ß", "").0, "SS");
        assert_eq!(fallback("你好", "").0, "你好");
        assert_eq!(fallback("---", "").0, "PR");
        assert_eq!(fallback("e\u{301}clair", ""), fallback("éclair", ""));
    }
}
