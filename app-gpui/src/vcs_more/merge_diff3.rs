//! Three-way merge behind the merge editor, as IntelliJ's merge tool does it
//! (`MergeRangeUtil.getMergeType`, `MergeResolveUtil.tryResolve`): the changes the two sides made to
//! the base at line level, each classified as one-sided, identical or conflicting, and the word-level
//! merge that resolves a conflict whose sides touched different words.
//!
//! Offsets are UTF-16 code units.

use crate::vcs_diff::comparison::chunk_optimizer::{LineInfo, optimize_line_chunks};
use crate::vcs_diff::comparison::iterables::DiffIterable;
use crate::vcs_diff::comparison::lcs::{Range, diff_keys};
use crate::vcs_diff::doc::utf16_len;
use crate::vcs_diff::merge_gutters::MergeKind;
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

/// A half-open token range.
pub type Span = (usize, usize);

/// What the sides hold in place of one stretch of the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub left: Span,
    pub base: Span,
    pub right: Span,
}

/// How many distinct tokens the three versions may hold together: each one is numbered with a
/// 16-bit unit outside the control characters and the surrogates. Texts with more are too different
/// to compare token by token.
pub const MAX_TOKENS: usize = 0x10000 - 0x100 - 0x800;

/// Numbers tokens by their key, equal keys sharing a number.
#[derive(Default)]
struct Interner<'a> {
    ids: HashMap<Cow<'a, str>, u16>,
}

impl<'a> Interner<'a> {
    /// The numbers of `tokens`; `None` once there are more distinct keys than [`MAX_TOKENS`].
    fn encode(&mut self, tokens: &[&'a str], key: &impl Fn(&'a str) -> Cow<'a, str>) -> Option<Vec<u16>> {
        let mut out = Vec::with_capacity(tokens.len());
        for &token in tokens {
            let key = key(token);
            let id = match self.ids.get(&key) {
                Some(id) => *id,
                None => {
                    if self.ids.len() >= MAX_TOKENS {
                        return None;
                    }
                    let id = u16::try_from(self.ids.len()).ok()?;
                    self.ids.insert(key, id);
                    id
                }
            };
            out.push(id);
        }
        Some(out)
    }
}

/// Whether `c` is white space as the word and blank patterns of the merge count it.
const fn is_blank_or_break(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// How many UTF-16 code units of `token` are not white space: the weight of a line.
fn weight(token: &str) -> usize {
    token.chars().filter(|c| !is_blank_or_break(*c)).map(char::len_utf16).sum()
}

/// What the line optimizer knows of each line: its number as the key, and its weight.
fn line_infos<'a>(ids: &'a [u16], tokens: &[&str]) -> Vec<LineInfo<'a>> {
    ids.iter()
        .zip(tokens)
        .map(|(id, token)| LineInfo { key: std::slice::from_ref(id), non_space_chars: weight(token) })
        .collect()
}

/// The changes between the base and a side, in tokens; with `lines` (the tokens of both, which are
/// lines), moved to where a reader expects the boundaries of changed lines.
fn against(base: &[u16], side: &[u16], lines: Option<(&[&str], &[&str])>) -> Option<Vec<Range>> {
    let found = diff_keys(base, side).ok()?;
    let Some((base_tokens, side_tokens)) = lines else {
        return Some(found);
    };
    let lines1 = line_infos(base, base_tokens);
    let lines2 = line_infos(side, side_tokens);
    Some(optimize_line_chunks(&lines1, &lines2, &DiffIterable::new(found, base.len(), side.len())).changes)
}

/// The stretches of the base that either side changed, tokens being equal when they are the same
/// text. See [`diff3_by`].
pub fn diff3(left: &[&str], base: &[&str], right: &[&str]) -> Option<Vec<Cluster>> {
    diff3_by(left, base, right, Cow::Borrowed, false)
}

/// The stretches of the base that either side changed. Changes of the two sides that overlap or
/// touch form one cluster, so two edits next to each other conflict, as in IntelliJ. Tokens are
/// equal when their `key`s are; with `optimize_lines` the tokens are lines and the changes move to
/// where a reader expects their boundaries. `None` when there are too many distinct tokens to
/// compare, or the versions differ too much.
pub fn diff3_by<'a>(
    left: &[&'a str],
    base: &[&'a str],
    right: &[&'a str],
    key: impl Fn(&'a str) -> Cow<'a, str>,
    optimize_lines: bool,
) -> Option<Vec<Cluster>> {
    let mut interner = Interner::default();
    let b = interner.encode(base, &key)?;
    let l = interner.encode(left, &key)?;
    let r = interner.encode(right, &key)?;
    let lc = against(&b, &l, optimize_lines.then_some((base, left)))?;
    let rc = against(&b, &r, optimize_lines.then_some((base, right)))?;
    // By how many tokens a change makes its side longer than the base.
    let growth = |c: &Range| (c.end2 - c.start2) as isize - (c.end1 - c.start1) as isize;

    let mut clusters = Vec::new();
    let (mut i, mut j) = (0, 0);
    // How far each side's tokens are shifted from the base's by the changes before the cluster.
    let (mut shift_l, mut shift_r) = (0_isize, 0_isize);
    while i < lc.len() || j < rc.len() {
        let from_left = i < lc.len() && (j >= rc.len() || lc[i].start1 <= rc[j].start1);
        let first = if from_left { lc[i] } else { rc[j] };
        let start = first.start1;
        let mut end = first.end1;
        let (mut grow_l, mut grow_r) = (0_isize, 0_isize);
        if from_left {
            grow_l += growth(&first);
            i += 1;
        } else {
            grow_r += growth(&first);
            j += 1;
        }
        let mut progressed = true;
        while progressed {
            progressed = false;
            while i < lc.len() && lc[i].start1 <= end {
                end = end.max(lc[i].end1);
                grow_l += growth(&lc[i]);
                i += 1;
                progressed = true;
            }
            while j < rc.len() && rc[j].start1 <= end {
                end = end.max(rc[j].end1);
                grow_r += growth(&rc[j]);
                j += 1;
                progressed = true;
            }
        }
        clusters.push(Cluster {
            base: (start, end),
            left: (start.saturating_add_signed(shift_l), end.saturating_add_signed(shift_l + grow_l)),
            right: (start.saturating_add_signed(shift_r), end.saturating_add_signed(shift_r + grow_r)),
        });
        shift_l += grow_l;
        shift_r += grow_r;
    }
    Some(clusters)
}

/// Classifies a stretch from its texts in each version (`MergeRangeUtil.getMergeType`). `same`
/// compares texts; `None` when neither side changed it.
fn classify(left: &str, base: &str, right: &str, same: impl Fn(&str, &str) -> bool) -> Option<MergeKind> {
    if base.is_empty() {
        if left.is_empty() {
            return Some(MergeKind::Right);
        }
        if right.is_empty() {
            return Some(MergeKind::Left);
        }
        return Some(if same(left, right) { MergeKind::Both } else { MergeKind::Conflict });
    }
    if left.is_empty() && right.is_empty() {
        return Some(MergeKind::Both);
    }
    match (same(base, left), same(base, right)) {
        (true, true) => None,
        (true, false) => Some(MergeKind::Right),
        (false, true) => Some(MergeKind::Left),
        (false, false) => Some(if same(left, right) { MergeKind::Both } else { MergeKind::Conflict }),
    }
}

/// Lines of `text`, each with its terminator, so joining them gives `text` back.
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// One stretch the sides changed, with the text each version has there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreeWayChange {
    /// Offsets of the stretch in the base.
    pub base_from: usize,
    pub base_to: usize,
    /// Offsets of the stretch in the left and the right version.
    pub left_from: usize,
    pub left_to: usize,
    pub right_from: usize,
    pub right_to: usize,
    pub left: String,
    pub base: String,
    pub right: String,
    pub kind: MergeKind,
    /// The word-level merge of a conflict, when the sides changed different words.
    pub auto: Option<String>,
}

/// Where each line of a text starts, and where the text ends.
struct LineStarts {
    /// Byte offsets, for slicing.
    bytes: Vec<usize>,
    /// UTF-16 offsets, for reporting.
    units: Vec<usize>,
}

impl LineStarts {
    fn new(lines: &[&str]) -> Self {
        let mut bytes = vec![0];
        let mut units = vec![0];
        let (mut byte, mut unit) = (0, 0);
        for line in lines {
            byte += line.len();
            unit += utf16_len(line);
            bytes.push(byte);
            units.push(unit);
        }
        Self { bytes, units }
    }

    fn slice<'a>(&self, text: &'a str, (from, to): Span) -> &'a str {
        &text[self.bytes[from]..self.bytes[to]]
    }
}

/// The changes between the versions line by line; `None` when the files are too different to
/// compare.
pub fn merge_changes(left: &str, base: &str, right: &str) -> Option<Vec<ThreeWayChange>> {
    let lines = [split_lines(left), split_lines(base), split_lines(right)];
    let clusters = diff3_by(&lines[0], &lines[1], &lines[2], Cow::Borrowed, true)?;
    let at = [LineStarts::new(&lines[0]), LineStarts::new(&lines[1]), LineStarts::new(&lines[2])];
    let mut out = Vec::new();
    for c in clusters {
        let l = at[0].slice(left, c.left);
        let b = at[1].slice(base, c.base);
        let r = at[2].slice(right, c.right);
        let Some(kind) = classify(l, b, r, |x, y| x == y) else { continue };
        let resolvable = kind == MergeKind::Conflict && !b.is_empty() && !l.is_empty() && !r.is_empty();
        out.push(ThreeWayChange {
            base_from: at[1].units[c.base.0],
            base_to: at[1].units[c.base.1],
            left_from: at[0].units[c.left.0],
            left_to: at[0].units[c.left.1],
            right_from: at[2].units[c.right.0],
            right_to: at[2].units[c.right.1],
            left: l.to_owned(),
            base: b.to_owned(),
            right: r.to_owned(),
            kind,
            auto: if resolvable { try_resolve(l, b, r) } else { None },
        });
    }
    Some(out)
}

/// Blanks other than line breaks.
const BLANK_CLASS: &str =
    r"[\t\x0B\x0C \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";

/// A word, a line break, a run of blanks, or any other single character.
static WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"[\p{{L}}\p{{N}}\p{{M}}_]+|\r?\n|{BLANK_CLASS}+|(?s:.)")).expect("the word pattern is valid")
});

static BLANKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("{BLANK_CLASS}+")).expect("the blank pattern is valid"));

/// Words, punctuation, line breaks and runs of blanks of `text`; joined they give `text` back.
pub fn split_words(text: &str) -> Vec<&str> {
    WORDS.find_iter(text).map(|found| found.as_str()).collect()
}

/// Merges the three versions word by word, when no word was changed by both sides differently
/// (`MergeResolveUtil.tryResolve`). Tried exactly first, then ignoring the width of blanks, which
/// keeps the white space of the side that changed it. `None` when the sides really conflict.
pub fn try_resolve(left: &str, base: &str, right: &str) -> Option<String> {
    merge_words(left, base, right, false).or_else(|| merge_words(left, base, right, true))
}

/// `token` with every run of blanks as one space.
fn squash(token: &str) -> Cow<'_, str> {
    BLANKS.replace_all(token, " ")
}

fn merge_words(left: &str, base: &str, right: &str, ignore_blanks: bool) -> Option<String> {
    let same = |a: &str, b: &str| a == b || (ignore_blanks && squash(a) == squash(b));
    let tokens = [split_words(left), split_words(base), split_words(right)];
    let clusters = diff3_by(
        &tokens[0],
        &tokens[1],
        &tokens[2],
        |token| if ignore_blanks { squash(token) } else { Cow::Borrowed(token) },
        false,
    )?;
    let text = |side: usize, from: usize, to: usize| tokens[side][from..to].concat();
    let mut out = String::new();
    let (mut l, mut b, mut r) = (0, 0, 0);
    // The text between two changed stretches is equal under the policy; a side that changed only
    // blanks there keeps its version.
    let gap = |from: Span3, to: Span3| {
        let gl = text(0, from.0, to.0);
        let gb = text(1, from.1, to.1);
        let gr = text(2, from.2, to.2);
        if gl != gb {
            gl
        } else if gr != gb {
            gr
        } else {
            gb
        }
    };
    for c in clusters {
        out.push_str(&gap((l, b, r), (c.left.0, c.base.0, c.right.0)));
        let tl = text(0, c.left.0, c.left.1);
        let tb = text(1, c.base.0, c.base.1);
        let tr = text(2, c.right.0, c.right.1);
        match classify(&tl, &tb, &tr, same) {
            Some(MergeKind::Conflict) => return None,
            Some(MergeKind::Right) => out.push_str(&tr),
            _ => out.push_str(&tl),
        }
        (l, b, r) = (c.left.1, c.base.1, c.right.1);
    }
    out.push_str(&gap((l, b, r), (tokens[0].len(), tokens[1].len(), tokens[2].len())));
    Some(out)
}

/// A token index in the left, the base and the right version.
type Span3 = (usize, usize, usize);

#[cfg(test)]
pub(crate) mod test_support {
    /// Deterministic pseudo-random numbers in `[0, 1)`, so a failing case can be replayed.
    pub(crate) struct Random(u32);

    impl Random {
        pub(crate) fn new(seed: u32) -> Self {
            Self(seed)
        }

        pub(crate) fn unit(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(self.0) / 4_294_967_296.0
        }

        /// A whole number in `[0, below)`.
        pub(crate) fn below(&mut self, below: usize) -> usize {
            (self.unit() * below as f64).floor() as usize
        }
    }

    /// The lines joined, each with a line break after it.
    pub(crate) fn lines(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Random, lines};
    use super::*;
    mod proofs8_chunk_optimizer;

    /// The text of a side rebuilt from the base and the changes: it must give the version back.
    /// The texts are ASCII, so UTF-16 offsets are byte offsets.
    fn rebuild(base: &str, changes: &[ThreeWayChange], side: MergeKind) -> String {
        let mut out = String::new();
        let mut pos = 0;
        for c in changes {
            out.push_str(&base[pos..c.base_from]);
            out.push_str(if side == MergeKind::Left { &c.left } else { &c.right });
            pos = c.base_to;
        }
        out + &base[pos..]
    }

    fn cluster(left: Span, base: Span, right: Span) -> Cluster {
        Cluster { left, base, right }
    }

    fn kinds(left: &str, base: &str, right: &str) -> Vec<MergeKind> {
        merge_changes(left, base, right).expect("the versions are comparable").iter().map(|c| c.kind).collect()
    }

    fn base() -> String {
        lines(&["a", "b", "c", "d", "e", "f", "g"])
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › splitting › keeps every character
    #[test]
    fn keeps_every_character() {
        for text in ["", "a", "a\n", "a\n\nb", "a\r\nb\r\n", "foo(bar, 12) // ünï — 日本語\t x\r"] {
            assert_eq!(split_lines(text).concat(), text);
            assert_eq!(split_words(text).concat(), text);
        }
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › splitting › separates words, punctuation, blanks and line breaks
    #[test]
    fn separates_words_punctuation_blanks_and_line_breaks() {
        assert_eq!(split_words("foo(a, b)\n"), ["foo", "(", "a", ",", " ", "b", ")", "\n"]);
        assert_eq!(split_words("a \t b\r\nc"), ["a", " \t ", "b", "\r\n", "c"]);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › diff3 › keeps edits of both sides apart when unchanged tokens lie between them
    #[test]
    fn keeps_edits_of_both_sides_apart_when_unchanged_tokens_lie_between_them() {
        let clusters = diff3(&["a", "X", "c", "d", "e"], &["a", "b", "c", "d", "e"], &["a", "b", "c", "d", "Y"]);
        assert_eq!(clusters, Some(vec![cluster((1, 2), (1, 2), (1, 2)), cluster((4, 5), (4, 5), (4, 5))]));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › diff3 › joins edits that touch into one cluster
    #[test]
    fn joins_edits_that_touch_into_one_cluster() {
        let clusters = diff3(&["a", "X", "c"], &["a", "b", "c"], &["a", "b", "Y"]);
        assert_eq!(clusters, Some(vec![cluster((1, 3), (1, 3), (1, 3))]));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › diff3 › handles insertions at the same point and at the ends
    #[test]
    fn handles_insertions_at_the_same_point_and_at_the_ends() {
        assert_eq!(diff3(&["a", "L", "b"], &["a", "b"], &["a", "R", "b"]), Some(vec![cluster((1, 2), (1, 1), (1, 2))]));
        assert_eq!(
            diff3(&["L", "a"], &["a"], &["a", "R"]),
            Some(vec![cluster((0, 1), (0, 0), (0, 0)), cluster((2, 2), (1, 1), (1, 2))])
        );
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › diff3 › gives up on more distinct tokens than it can intern
    #[test]
    fn gives_up_on_more_distinct_tokens_than_it_can_intern() {
        let owned: Vec<String> = (0..70_000).map(|i| format!("t{i}")).collect();
        let many: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert_eq!(diff3(&many, &many, &many), None);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › classifies one-sided, identical and conflicting changes
    #[test]
    fn classifies_one_sided_identical_and_conflicting_changes() {
        let left = lines(&["a", "B", "c", "d", "e", "f", "G"]);
        let right = lines(&["a", "b", "c", "D", "e", "f", "G"]);
        let changes = merge_changes(&left, &base(), &right).expect("the versions are comparable");
        let found: Vec<MergeKind> = changes.iter().map(|c| c.kind).collect();
        assert_eq!(found, [MergeKind::Left, MergeKind::Right, MergeKind::Both]);
        let first = &changes[0];
        assert_eq!((first.base_from, first.base_to), (2, 4));
        assert_eq!((first.left.as_str(), first.base.as_str(), first.right.as_str()), ("B\n", "b\n", "b\n"));

        // Changes of adjacent lines are one change, which conflicts, as in IntelliJ.
        let adjacent = kinds(&lines(&["a", "B", "c"]), &lines(&["a", "b", "c"]), &lines(&["a", "b", "C"]));
        assert_eq!(adjacent, [MergeKind::Conflict]);

        let conflict = merge_changes(&lines(&["a", "L", "c"]), &lines(&["a", "b", "c"]), &lines(&["a", "R", "c"]))
            .expect("the versions are comparable");
        assert_eq!(conflict.len(), 1);
        assert_eq!(conflict[0].kind, MergeKind::Conflict);
        assert_eq!(conflict[0].auto, None);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › treats a deletion by both sides and equal insertions as agreement
    #[test]
    fn treats_a_deletion_by_both_sides_and_equal_insertions_as_agreement() {
        assert_eq!(kinds(&lines(&["a", "c"]), &lines(&["a", "b", "c"]), &lines(&["a", "c"])), [MergeKind::Both]);
        assert_eq!(kinds(&lines(&["a", "N", "b"]), &lines(&["a", "b"]), &lines(&["a", "N", "b"])), [MergeKind::Both]);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › classifies an insertion by one side, and an insertion against a change
    #[test]
    fn classifies_an_insertion_by_one_side_and_an_insertion_against_a_change() {
        let inserted = merge_changes(&lines(&["a", "N", "b"]), &lines(&["a", "b"]), &lines(&["a", "b"]))
            .expect("the versions are comparable");
        let insert = &inserted[0];
        assert_eq!((insert.kind, insert.base.as_str(), insert.left.as_str()), (MergeKind::Left, "", "N\n"));
        assert_eq!((insert.base_from, insert.base_to), (2, 2));
        let clashing = merge_changes(&lines(&["a", "N", "b"]), &lines(&["a", "b"]), &lines(&["a", "M", "b"]))
            .expect("the versions are comparable");
        assert_eq!(clashing[0].kind, MergeKind::Conflict);
        assert_eq!(clashing[0].auto, None);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › does not resolve a conflict where one side deleted what the other changed
    #[test]
    fn does_not_resolve_a_conflict_where_one_side_deleted_what_the_other_changed() {
        let changes = merge_changes(&lines(&["a", "c"]), &lines(&["a", "b", "c"]), &lines(&["a", "B", "c"]))
            .expect("the versions are comparable");
        assert_eq!((changes[0].kind, changes[0].auto.as_deref()), (MergeKind::Conflict, None));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › offers the word-level merge of a conflict whose sides changed different words
    #[test]
    fn offers_the_word_level_merge_of_a_conflict_whose_sides_changed_different_words() {
        let changes = merge_changes("let x = 1 + b;\n", "let a = 1 + b;\n", "let a = 1 + 2;\n")
            .expect("the versions are comparable");
        assert_eq!((changes[0].kind, changes[0].auto.as_deref()), (MergeKind::Conflict, Some("let x = 1 + 2;\n")));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › reports no change for equal versions
    #[test]
    fn reports_no_change_for_equal_versions() {
        assert_eq!(merge_changes(&base(), &base(), &base()), Some(Vec::new()));
        assert_eq!(merge_changes("", "", ""), Some(Vec::new()));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › marks a line that gained its terminator as changed
    #[test]
    fn marks_a_line_that_gained_its_terminator_as_changed() {
        let changes = merge_changes("a\nb\n", "a\nb", "a\nb").expect("the versions are comparable");
        let c = &changes[0];
        assert_eq!((c.kind, c.left.as_str(), c.base.as_str()), (MergeKind::Left, "b\n", "b"));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › mergeChanges (line level) › rebuilds both versions from the base and the changes, whatever the edits
    #[test]
    fn rebuilds_both_versions_from_the_base_and_the_changes_whatever_the_edits() {
        let mut rnd = Random::new(7);
        let vocab = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta"];
        for round in 0..300 {
            let count = rnd.below(14);
            let base_lines: Vec<String> = (0..count).map(|_| vocab[rnd.below(vocab.len())].to_owned()).collect();
            let mut mutate = || {
                let mut out = base_lines.clone();
                for _ in 0..rnd.below(4) {
                    let at = rnd.below(out.len() + 1);
                    let op = rnd.unit();
                    if op < 0.34 {
                        if at < out.len() {
                            out.remove(at);
                        }
                    } else if op < 0.67 {
                        out.insert(at, format!("{}!", vocab[rnd.below(vocab.len())]));
                    } else if at < out.len() {
                        out[at].push('?');
                    }
                }
                out
            };
            let joined = |list: &[String]| lines(&list.iter().map(String::as_str).collect::<Vec<_>>());
            let l = joined(&mutate());
            let r = joined(&mutate());
            let b = joined(&base_lines);
            let changes = merge_changes(&l, &b, &r).expect("the versions are comparable");
            assert_eq!(rebuild(&b, &changes, MergeKind::Left), l, "round {round}");
            assert_eq!(rebuild(&b, &changes, MergeKind::Right), r, "round {round}");
            for c in &changes {
                match c.kind {
                    MergeKind::Left => assert_eq!(c.right, c.base, "round {round}"),
                    MergeKind::Right => assert_eq!(c.left, c.base, "round {round}"),
                    MergeKind::Both => assert_eq!(c.left, c.right, "round {round}"),
                    MergeKind::Conflict => assert_ne!(c.left, c.right, "round {round}"),
                }
            }
        }
    }

    fn resolved(left: &str, base: &str, right: &str) -> Option<String> {
        try_resolve(left, base, right)
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › merges changes to different words
    #[test]
    fn merges_changes_to_different_words() {
        assert_eq!(resolved("foo(1, b)", "foo(a, b)", "foo(a, 2)").as_deref(), Some("foo(1, 2)"));
        assert_eq!(resolved("let x = 1;", "let a = 1;", "let a = 2;").as_deref(), Some("let x = 2;"));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › merges an edit at the start of a line with one at its end
    #[test]
    fn merges_an_edit_at_the_start_of_a_line_with_one_at_its_end() {
        assert_eq!(resolved("const a = 1; // first", "var a = 1;", "var a = 1; // note"), None);
        assert_eq!(
            resolved("const a = 1;", "var a = 1;", "var a = 1; // note").as_deref(),
            Some("const a = 1; // note")
        );
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › gives up when both sides changed the same word differently
    #[test]
    fn gives_up_when_both_sides_changed_the_same_word_differently() {
        assert_eq!(resolved("let x = 1;", "let a = 1;", "let y = 1;"), None);
        assert_eq!(resolved("f(a)", "f(b)", "f(c)"), None);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › gives up when the edits touch
    #[test]
    fn gives_up_when_the_edits_touch() {
        assert_eq!(resolved("x y", "a b", "a z"), None);
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › accepts identical changes
    #[test]
    fn accepts_identical_changes() {
        assert_eq!(resolved("let x = 1;", "let a = 1;", "let x = 1;").as_deref(), Some("let x = 1;"));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › falls back to ignoring the width of blanks, keeping the changed side's blanks
    #[test]
    fn falls_back_to_ignoring_the_width_of_blanks_keeping_the_changed_sides_blanks() {
        // The edits touch, so the exact pass fails; the blank-insensitive one merges them.
        assert_eq!(resolved("a  =  1", "a = 1", "a = 2").as_deref(), Some("a  =  2"));
        assert_eq!(resolved("a   b c", "a b c", "a b  c").as_deref(), Some("a   b  c"));
        assert_eq!(resolved("a  b", "a b", "a b").as_deref(), Some("a  b"));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › does not take a line break for a blank
    #[test]
    fn does_not_take_a_line_break_for_a_blank() {
        // Were a line break a blank, both sides would equal the base here and the merge would succeed.
        assert_eq!(resolved("a\nb", "a b", "a\n\nb"), None);
        assert_eq!(resolved("a\nb", "a b", "a b c").as_deref(), Some("a\nb c"));
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › returns the changed side when only one side changed
    #[test]
    fn returns_the_changed_side_when_only_one_side_changed() {
        let mut rnd = Random::new(11);
        let vocab = ["foo", "bar", "baz", "(", ")", ",", " ", "\n", "12", "x_1"];
        for round in 0..300 {
            let base_len = 1 + rnd.below(20);
            let base: String = (0..base_len).map(|_| vocab[rnd.below(vocab.len())]).collect();
            let other_len = rnd.below(20);
            let other: String = (0..other_len).map(|_| vocab[rnd.below(vocab.len())]).collect();
            assert_eq!(resolved(&other, &base, &base).as_deref(), Some(other.as_str()), "left {round}");
            assert_eq!(resolved(&base, &base, &other).as_deref(), Some(other.as_str()), "right {round}");
            assert_eq!(resolved(&other, &base, &other).as_deref(), Some(other.as_str()), "same {round}");
        }
    }

    // src/features/vcs/__tests__/mergeDiff3.test.ts › tryResolve (word level) › is symmetric in the sides it can merge
    #[test]
    fn is_symmetric_in_the_sides_it_can_merge() {
        let mut rnd = Random::new(5);
        let vocab = ["a", "b", "c", "d", "e", "f", " ", ","];
        for round in 0..300 {
            let tokens: Vec<&str> = (0..12).map(|_| vocab[rnd.below(vocab.len())]).collect();
            let edit = |i: usize| -> String {
                tokens.iter().enumerate().map(|(k, t)| if k == i { t.repeat(2) } else { (*t).to_owned() }).collect()
            };
            let base = tokens.concat();
            let i = rnd.below(12);
            let j = rnd.below(12);
            let ab = resolved(&edit(i), &base, &edit(j));
            let ba = resolved(&edit(j), &base, &edit(i));
            assert_eq!(ab.is_none(), ba.is_none(), "round {round}");
            if ab.is_some() && i.abs_diff(j) > 1 {
                assert_eq!(ab, ba, "round {round}");
            }
        }
    }
}
