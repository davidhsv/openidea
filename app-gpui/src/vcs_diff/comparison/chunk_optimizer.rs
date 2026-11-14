//! Shifts ambiguous changes to where people expect them, as the IntelliJ Platform does
//! (`ChunkOptimizer`; Apache-2.0, Copyright JetBrains s.r.o. and contributors): touching changes are
//! merged, and an insertion or deletion that could sit at several places moves to a word boundary
//! (words) or next to an empty line (lines).

use super::by_word::InlineChunk;
use super::iterables::{DiffIterable, create_unchanged, expand_backward, expand_forward, unchanged};
use super::lcs::Range;
use super::text::is_space;

/// Two unchanged ranges with a change between them that could sit elsewhere.
struct Touch {
    /// The ranges touch on the first side: the change is an insertion.
    left: bool,
    /// How many elements after the first range equal those the second one starts with.
    equal_forward: usize,
    /// How many elements before the second range equal those the first one ends with.
    equal_backward: usize,
    range1: Range,
    range2: Range,
}

/// Walks the unchanged ranges, merging those a change needlessly splits; `get_shift` says by how many
/// elements the boundary between two touching ranges moves (forward when positive, backward when
/// negative).
fn optimize<T: PartialEq>(
    data1: &[T],
    data2: &[T],
    iterable: &DiffIterable,
    get_shift: impl Fn(&Touch) -> isize,
) -> DiffIterable {
    let mut ranges: Vec<Range> = Vec::new();
    for r in unchanged(iterable) {
        ranges.push(r);
        process_last_ranges(&mut ranges, data1, data2, &get_shift);
    }
    create_unchanged(ranges, data1.len(), data2.len())
}

fn process_last_ranges<T: PartialEq>(
    ranges: &mut Vec<Range>,
    data1: &[T],
    data2: &[T],
    get_shift: &impl Fn(&Touch) -> isize,
) {
    while ranges.len() >= 2 {
        let kept = ranges.len() - 2;
        let range1 = ranges[kept];
        let range2 = ranges[kept + 1];
        // Changes that do not touch cannot be optimized (the input would not be an LCS).
        if range1.end1 != range2.start1 && range1.end2 != range2.start2 {
            return;
        }
        let count1 = range1.end1 - range1.start1;
        let count2 = range2.end1 - range2.start1;
        let equal_forward = expand_forward(
            data1,
            data2,
            range1.end1,
            range1.end2,
            (range1.end1 + count2).min(data1.len()),
            (range1.end2 + count2).min(data2.len()),
        );
        let equal_backward = expand_backward(
            data1,
            data2,
            range2.start1.saturating_sub(count1),
            range2.start2.saturating_sub(count1),
            range2.start1,
            range2.start2,
        );
        if equal_forward == 0 && equal_backward == 0 {
            return;
        }
        // Merge chunks left: [A]B[B] -> [AB]B
        if equal_forward == count2 {
            ranges.truncate(kept);
            ranges.push(Range::new(range1.start1, range1.end1 + count2, range1.start2, range1.end2 + count2));
            continue;
        }
        // Merge chunks right: [A]A[B] -> A[AB]
        if equal_backward == count1 {
            ranges.truncate(kept);
            ranges.push(Range::new(range2.start1 - count1, range2.end1, range2.start2 - count1, range2.end2));
            continue;
        }
        let touch = Touch { left: range1.end1 == range2.start1, equal_forward, equal_backward, range1, range2 };
        let shift = get_shift(&touch);
        if shift != 0 {
            ranges[kept] = Range::new(
                range1.start1,
                range1.end1.saturating_add_signed(shift),
                range1.start2,
                range1.end2.saturating_add_signed(shift),
            );
            ranges[kept + 1] = Range::new(
                range2.start1.saturating_add_signed(shift),
                range2.end1,
                range2.start2.saturating_add_signed(shift),
                range2.end2,
            );
        }
        return;
    }
}

/// Words: fewer chunks, and fewer changed "sentences" (runs of words not separated by whitespace):
/// "[AX] [AZ]" - "[AX] AY [AZ]", not "[AX A][Z]".
pub(crate) fn optimize_word_chunks(
    words1: &[InlineChunk<'_>],
    words2: &[InlineChunk<'_>],
    text1: &[u16],
    text2: &[u16],
    iterable: &DiffIterable,
) -> DiffIterable {
    let keys1: Vec<&[u16]> = words1.iter().map(|word| word.key).collect();
    let keys2: Vec<&[u16]> = words2.iter().map(|word| word.key).collect();
    optimize(&keys1, &keys2, iterable, |touch| {
        let (words, text) = if touch.left { (words1, text1) } else { (words2, text2) };
        let touch_start = if touch.left { touch.range2.start1 } else { touch.range2.start2 };
        // Already separated by whitespace.
        if separated(text, &words[touch_start - 1], &words[touch_start]) {
            return 0;
        }
        // Shift chunks left: [X]A Y[A ZA] -> [XA] YA [ZA]
        let left = (0..touch.equal_forward)
            .find(|i| separated(text, &words[touch_start + i], &words[touch_start + i + 1]))
            .map(|i| i + 1);
        if let Some(left) = left {
            return left as isize;
        }
        // Shift chunks right: [AX A]Y A[Z] -> [AX] AY [AZ]
        let right = (0..touch.equal_backward)
            .find(|i| separated(text, &words[touch_start - i - 2], &words[touch_start - i - 1]))
            .map(|i| i + 1);
        if let Some(right) = right {
            return -(right as isize);
        }
        0
    })
}

/// Whether whitespace or a line break stands between two words that follow each other.
fn separated(text: &[u16], word1: &InlineChunk<'_>, word2: &InlineChunk<'_>) -> bool {
    word1.newline || word2.newline || (word1.offset2..word2.offset1).any(|i| is_space(text[i]))
}

/// Lines with at most this many non-space characters are "unimportant" (`}`, blank).
pub(crate) const UNIMPORTANT_LINE_CHAR_COUNT: usize = 3;

/// What the optimizer knows of a line: the key equal lines share, and its weight.
pub(crate) struct LineInfo<'a> {
    pub(crate) key: &'a [u16],
    pub(crate) non_space_chars: usize,
}

/// Lines: fewer chunks, and insertions / deletions bounded by an empty (or unimportant) line:
/// "ABooYZ [ABuuYZ ]ABzzYZ", not "ABooYZ AB[uuYZ AB]zzYZ".
pub(crate) fn optimize_line_chunks(
    lines1: &[LineInfo<'_>],
    lines2: &[LineInfo<'_>],
    iterable: &DiffIterable,
) -> DiffIterable {
    let keys1: Vec<&[u16]> = lines1.iter().map(|line| line.key).collect();
    let keys2: Vec<&[u16]> = lines2.iter().map(|line| line.key).collect();
    optimize(&keys1, &keys2, iterable, |touch| {
        // An empty line boundary in unchanged lines: the insertion / deletion goes right before / after it.
        let unchanged_boundary = |threshold: usize| {
            let lines = if touch.left { lines1 } else { lines2 };
            let touch_start = if touch.left { touch.range2.start1 } else { touch.range2.start2 };
            to_shift(
                next_unimportant(lines, touch_start, touch.equal_forward + 1, threshold),
                prev_unimportant(lines, touch_start - 1, touch.equal_backward + 1, threshold),
            )
        };
        // An empty line boundary in changed lines: the insertion / deletion starts / ends with it.
        let changed_boundary = |threshold: usize| {
            let lines = if touch.left { lines2 } else { lines1 };
            let change_start = if touch.left { touch.range1.end2 } else { touch.range1.end1 };
            let change_end = if touch.left { touch.range2.start2 } else { touch.range2.start1 };
            to_shift(
                next_unimportant(lines, change_start, touch.equal_forward + 1, threshold),
                prev_unimportant(lines, change_end - 1, touch.equal_backward + 1, threshold),
            )
        };
        unchanged_boundary(0)
            .or_else(|| changed_boundary(0))
            .or_else(|| unchanged_boundary(UNIMPORTANT_LINE_CHAR_COUNT))
            .or_else(|| changed_boundary(UNIMPORTANT_LINE_CHAR_COUNT))
            .unwrap_or(0)
    })
}

/// How far after `offset` the first unimportant line is, within `count` lines.
fn next_unimportant(lines: &[LineInfo<'_>], offset: usize, count: usize, threshold: usize) -> Option<usize> {
    (0..count).find(|i| lines[offset + i].non_space_chars <= threshold)
}

/// How far before `offset` the first unimportant line is, within `count` lines.
fn prev_unimportant(lines: &[LineInfo<'_>], offset: usize, count: usize, threshold: usize) -> Option<usize> {
    (0..count).find(|i| lines[offset - i].non_space_chars <= threshold)
}

/// The shift that puts the boundary at the unimportant line found; `None` when there is none.
fn to_shift(forward: Option<usize>, backward: Option<usize>) -> Option<isize> {
    match (forward, backward) {
        (None, None) => None,
        (Some(0), _) | (_, Some(0)) => Some(0),
        (Some(forward), _) => Some(forward as isize),
        (None, Some(backward)) => Some(-(backward as isize)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    mod proofs8_chunk_optimizer;

    fn units(texts: &[&str]) -> Vec<Vec<u16>> {
        texts.iter().map(|text| text.encode_utf16().collect()).collect()
    }

    fn infos(lines: &[Vec<u16>]) -> Vec<LineInfo<'_>> {
        lines.iter().map(|line| LineInfo { key: line, non_space_chars: line.len() }).collect()
    }

    // Not from a spec: an insertion that starts in the middle of a copy moves next to the blank line.
    #[test]
    fn moves_an_insertion_next_to_a_blank_line() {
        let before = units(&["p();", "q();", "", "r();"]);
        let after = units(&["p();", "q();", "", "z();", "q();", "", "r();"]);
        let mid_copy = DiffIterable::new(vec![Range::new(1, 1, 1, 4)], 4, 7);
        let optimized = optimize_line_chunks(&infos(&before), &infos(&after), &mid_copy);
        assert_eq!(optimized.changes, [Range::new(2, 2, 2, 5)]);
    }

    // Not from a spec: an insertion that repeats the lines before it becomes the first copy.
    #[test]
    fn merges_an_insertion_with_the_copy_before_it() {
        let before = units(&["p();", "q();", "", "r();"]);
        let after = units(&["p();", "q();", "", "p();", "q();", "", "r();"]);
        let mid_copy = DiffIterable::new(vec![Range::new(1, 1, 1, 4)], 4, 7);
        let optimized = optimize_line_chunks(&infos(&before), &infos(&after), &mid_copy);
        assert_eq!(optimized.changes, [Range::new(0, 0, 0, 3)]);
    }
}
