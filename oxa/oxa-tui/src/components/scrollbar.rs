//! B12: scrollbar geometry and drawing.
// ported from pi packages/tui/src/components/scroll-view.ts
use crate::Theme;

/// Thumb length in track rows: proportional to the visible share, at least 1 row, and at most
/// `rows - 1` so that the thumb can leave the top and bottom positions distinct.
fn thumb_len(total: usize, rows: usize) -> usize {
    let proportional = (rows * rows / total).max(1);
    proportional.min(rows.saturating_sub(1).max(1))
}

/// Top row of the thumb for `offset` in `0..=max` (`max = total - rows`), with `free` = the
/// number of positions the top can take (`rows - len`). Offset 0 maps to the top and the last
/// offset to the bottom; the offsets in between map inside, monotonically.
fn thumb_top(offset: usize, max: usize, free: usize) -> usize {
    if offset == 0 {
        0
    } else if offset >= max || free < 2 {
        free
    } else {
        1 + (offset - 1) * (free - 2) / (max - 1)
    }
}

/// Thumb geometry for a viewport: `(first row, row count)` of the thumb within `rows` track
/// rows, for `total` content lines, `rows` visible, the first visible line at `offset`.
/// The thumb is at least 1 row, touches the top only at `offset == 0` and the bottom only at
/// the last position. `None` when everything fits (`total <= rows`) or `rows == 0`.
pub fn scrollbar_thumb(total: usize, rows: usize, offset: usize) -> Option<(usize, usize)> {
    if rows == 0 || total <= rows {
        return None;
    }
    let max = total - rows;
    let offset = offset.min(max);
    let len = thumb_len(total, rows);
    let free = rows - len;
    Some((thumb_top(offset, max, free), len))
}

/// The scrollbar column: one single-column string per track row (`█` thumb in
/// `scrollbarThumb`, `│` track in `scrollbarTrack`). All-space cells when nothing scrolls.
pub fn scrollbar_cells(total: usize, rows: usize, offset: usize, theme: &Theme) -> Vec<String> {
    match scrollbar_thumb(total, rows, offset) {
        None => vec![" ".to_string(); rows],
        Some((top, len)) => (0..rows)
            .map(|row| {
                if row >= top && row < top + len {
                    theme.fg("scrollbarThumb", "█")
                } else {
                    theme.fg("scrollbarTrack", "│")
                }
            })
            .collect(),
    }
}

/// The content offset that puts the thumb's top at track row `row` (mouse drag on the bar).
pub fn scrollbar_offset_for_row(total: usize, rows: usize, row: usize) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    let max = total - rows;
    let free = rows - thumb_len(total, rows);
    if row == 0 {
        return 0;
    }
    if row >= free {
        return max;
    }
    if free == 2 {
        // Every middle offset lands on row 1; pick the middle of the range.
        return (max / 2).max(1).min(max);
    }
    // Smallest offset whose thumb top reaches `row`.
    let f = free - 2;
    let m = max - 1;
    let offset = 1 + ((row - 1) * m).div_ceil(f);
    offset.clamp(1, max)
}
