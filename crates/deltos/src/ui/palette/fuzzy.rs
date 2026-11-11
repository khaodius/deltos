//! Fuzzy matching for the command palette: the query's characters must appear in the
//! candidate in order, and the best placement of them is scored.
//!
//! The score rewards what a person means when they type a few letters: characters at the
//! start of the candidate, at the start of a word (after a space, `_`, `-`, `.` or `/`),
//! at a camelCase or letter-to-digit boundary, and runs of consecutive characters. Gaps
//! between matched characters cost a little, so a tight match beats a scattered one. The
//! best placement is found with a small dynamic program over query and candidate
//! characters, which also gives the matched positions for highlighting.

use std::ops::Range;

/// Points for every matched character.
pub const MATCH: i32 = 16;
/// A match at the start of a word.
const BOUNDARY: i32 = 8;
/// A match at the very start of the candidate, on top of [`BOUNDARY`].
const FIRST: i32 = 8;
/// A match at a camelCase hump or where digits start.
const CAMEL: i32 = 7;
/// A match right after the previous one, when it is not already a boundary.
const CONSECUTIVE: i32 = 5;
/// The cost of opening a gap between two matched characters, and of each skipped one.
const GAP_OPEN: i32 = 2;
const GAP_EXTEND: i32 = 1;
/// Skipped characters before the first match cost at most this much.
const LEADING_MAX: i32 = 8;
/// Longer candidates are only matched over this many characters.
const MAX_CHARS: usize = 512;

const NONE: i32 = i32::MIN / 2;

/// Where the query matched and how well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    /// Byte offsets of the matched characters in the candidate, in order.
    pub positions: Vec<usize>,
}

impl FuzzyMatch {
    /// The matched characters as byte ranges, with neighbours joined, for highlighting.
    pub fn ranges(&self, candidate: &str) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for &start in &self.positions {
            let Some(c) = candidate.get(start..).and_then(|s| s.chars().next()) else {
                continue;
            };
            let end = start + c.len_utf8();
            match out.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => out.push(start..end),
            }
        }
        out
    }
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// The bonus for a match at `j`, from the character before it.
fn bonus(chars: &[char], j: usize) -> i32 {
    let Some(&c) = chars.get(j) else {
        return 0;
    };
    let Some(&prev) = j.checked_sub(1).and_then(|p| chars.get(p)) else {
        return BOUNDARY + FIRST;
    };
    if !prev.is_alphanumeric() && c.is_alphanumeric() {
        BOUNDARY
    } else if (prev.is_lowercase() && c.is_uppercase()) || (prev.is_alphabetic() && c.is_numeric())
    {
        CAMEL
    } else {
        0
    }
}

/// A query folded once, to match against many candidates: the palette builds one per
/// keystroke rather than one per item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    chars: Vec<char>,
}

impl Pattern {
    /// The query's characters, case folded, without whitespace. A query longer than
    /// [`MAX_CHARS`] can match nothing, so it is not kept past that.
    pub fn new(query: &str) -> Self {
        Self {
            chars: query
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(MAX_CHARS + 1)
                .map(fold)
                .collect(),
        }
    }

    /// How many characters must match.
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Matches this query against `candidate`. An empty query matches everything with a
    /// score of zero.
    pub fn matches(&self, candidate: &str) -> Option<FuzzyMatch> {
        let query = &self.chars;
        if query.is_empty() {
            return Some(FuzzyMatch {
                score: 0,
                positions: Vec::new(),
            });
        }
        // Most candidates fail; a plain subsequence check rejects them without
        // allocating anything.
        let mut matched = 0;
        for c in candidate.chars().take(MAX_CHARS) {
            if fold(c) == query[matched] {
                matched += 1;
                if matched == query.len() {
                    break;
                }
            }
        }
        if matched < query.len() {
            return None;
        }
        best_placement(query, candidate)
    }
}

/// Matches `query` against `candidate`, ignoring case and the query's whitespace. An
/// empty query matches everything with a score of zero.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    Pattern::new(query).matches(candidate)
}

/// The best-scoring placement of `query`, which is known to be a subsequence of the
/// candidate's first [`MAX_CHARS`] characters. Its cost is bounded by `MAX_CHARS` squared.
fn best_placement(query: &[char], candidate: &str) -> Option<FuzzyMatch> {
    let (offsets, chars): (Vec<usize>, Vec<char>) =
        candidate.char_indices().take(MAX_CHARS).unzip();
    let folded: Vec<char> = chars.iter().copied().map(fold).collect();
    let (n, m) = (query.len(), chars.len());
    let bonuses: Vec<i32> = (0..m).map(|j| bonus(&chars, j)).collect();
    // score[i * m + j]: the best score with query[i] matched at candidate[j]; `from` is
    // where query[i - 1] was matched on that path.
    let mut score = vec![NONE; n * m];
    let mut from = vec![usize::MAX; n * m];
    for i in 0..n {
        // The best `score[i - 1][k] - gap` over k <= j - 2, kept as j moves right.
        let mut gap_best = NONE;
        let mut gap_from = usize::MAX;
        for j in 0..m {
            if i > 0 && j >= 2 {
                let k = j - 2;
                if gap_best > NONE {
                    gap_best -= GAP_EXTEND;
                }
                let prev = score[(i - 1) * m + k];
                if prev > NONE && prev - GAP_EXTEND > gap_best {
                    gap_best = prev - GAP_EXTEND;
                    gap_from = k;
                }
            }
            if folded[j] != query[i] {
                continue;
            }
            let b = bonuses[j];
            let ix = i * m + j;
            if i == 0 {
                score[ix] = MATCH + b - (j as i32).min(LEADING_MAX);
                continue;
            }
            let consecutive = match j.checked_sub(1).map(|k| score[(i - 1) * m + k]) {
                Some(prev) if prev > NONE => prev + MATCH + b.max(CONSECUTIVE),
                _ => NONE,
            };
            let gapped = if gap_best > NONE {
                gap_best - GAP_OPEN + MATCH + b
            } else {
                NONE
            };
            if consecutive >= gapped && consecutive > NONE {
                score[ix] = consecutive;
                from[ix] = j - 1;
            } else if gapped > NONE {
                score[ix] = gapped;
                from[ix] = gap_from;
            }
        }
    }
    let last = (n - 1) * m;
    let (mut j, best) = (0..m)
        .map(|j| (j, score[last + j]))
        .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
    if best <= NONE {
        return None;
    }
    let mut positions = vec![0; n];
    for i in (0..n).rev() {
        positions[i] = offsets[j];
        if i > 0 {
            j = from[i * m + j];
            if j == usize::MAX {
                return None;
            }
        }
    }
    Some(FuzzyMatch {
        score: best,
        positions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, c: &str) -> i32 {
        fuzzy_match(q, c).map_or(i32::MIN, |m| m.score)
    }

    fn marked(q: &str, c: &str) -> String {
        let m = fuzzy_match(q, c).expect("should match");
        let mut out = String::new();
        for (i, ch) in c.char_indices() {
            if m.positions.contains(&i) {
                out.extend(ch.to_uppercase());
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn needs_every_character_in_order() {
        assert!(fuzzy_match("ord", "orders").is_some());
        assert!(fuzzy_match("ORD", "orders").is_some());
        assert!(fuzzy_match("dro", "orders").is_none());
        assert!(fuzzy_match("ordersx", "orders").is_none());
        assert!(fuzzy_match("", "anything").is_some_and(|m| m.score == 0));
        assert!(fuzzy_match("x", "").is_none());
    }

    #[test]
    fn query_spaces_are_ignored() {
        assert!(fuzzy_match("new q", "New query tab").is_some());
        assert!(fuzzy_match("   ", "x").is_some_and(|m| m.positions.is_empty()));
    }

    #[test]
    fn prefix_beats_a_match_further_in() {
        assert!(score("tab", "Table") > score("tab", "Close tab"));
        assert!(score("ord", "orders") > score("ord", "user_orders"));
    }

    #[test]
    fn word_starts_beat_the_middle_of_words() {
        assert!(score("nq", "New query tab") > score("nq", "Unique"));
        assert_eq!(marked("nqt", "New query tab"), "New Query Tab");
        assert!(score("ui", "user_id") > score("ui", "build"));
        assert_eq!(marked("ui", "user_id"), "User_Id");
    }

    #[test]
    fn camel_case_humps_count_as_word_starts() {
        assert_eq!(marked("gc", "getCollection"), "GetCollection");
        assert!(score("gc", "getCollection") > score("gc", "magic"));
        assert_eq!(marked("e2", "events2024"), "Events2024");
    }

    #[test]
    fn consecutive_runs_beat_scattered_letters() {
        assert!(score("set", "Settings") > score("set", "Select tab"));
        assert_eq!(marked("ord", "user_orders"), "user_ORDers");
        assert!(score("abc", "xabcx") > score("abc", "xaxbxcx"));
        assert_eq!(marked("abc", "xaxbx abc"), "xaxbx ABC");
    }

    #[test]
    fn picks_the_best_placement_not_the_first() {
        // "o" could match the first letter of "toggle", but "Objects" is a word start.
        assert_eq!(marked("obj", "toggle Objects"), "toggle OBJects");
    }

    #[test]
    fn ranges_join_neighbours() {
        let m = fuzzy_match("ordr", "user_orders").unwrap();
        assert_eq!(m.ranges("user_orders"), vec![5..8, 9..10]);
    }

    #[test]
    fn handles_non_ascii_text() {
        let c = "Größe_Übersicht";
        let m = fuzzy_match("gü", c).unwrap();
        assert_eq!(m.positions, vec![0, c.find('Ü').unwrap()]);
        assert_eq!(m.ranges(c), vec![0..1, 8..10]);
        assert!(fuzzy_match("ÖSS", "größe").is_none());
        assert!(fuzzy_match("ÖSE", "größe").is_none());
        assert!(fuzzy_match("ÖE", "größe").is_some());
    }

    #[test]
    fn a_pattern_matches_like_the_function() {
        let p = Pattern::new(" Or D ");
        assert_eq!(p.len(), 3);
        assert_eq!(p.matches("user_orders"), fuzzy_match("ord", "user_orders"));
        assert!(p.matches("dro").is_none());
        assert!(Pattern::new("").matches("x").is_some_and(|m| m.score == 0));
    }

    #[test]
    fn long_queries_stay_bounded() {
        // A pasted wall of text is folded once and rejects candidates at once.
        let p = Pattern::new(&"ab".repeat(100_000));
        assert_eq!(p.len(), MAX_CHARS + 1);
        assert!(p.matches(&"ab".repeat(400)).is_none());
        let fits = "ab".repeat(MAX_CHARS / 2);
        assert!(Pattern::new(&fits).matches(&fits).is_some());
    }

    #[test]
    fn long_candidates_stay_bounded() {
        let long = "x".repeat(10_000) + "needle";
        // Past the limit the needle is not looked at, and nothing panics.
        assert!(fuzzy_match("needle", &long).is_none());
        let fits = "x".repeat(100) + "needle";
        assert!(fuzzy_match("needle", &fits).is_some());
    }
}
