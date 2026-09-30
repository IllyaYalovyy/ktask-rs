//! Windowing a list of variable-height blocks onto a fixed number of screen lines, keeping one
//! block in view.

/// The index of the first block shown when a list of blocks, each `block_heights[i]` lines
/// tall, is windowed to `height` lines: walks back from `selected`, adding earlier blocks
/// while everything from there to the selection still fits, so the selected block ends up the
/// last one in view exactly when it would not fit otherwise. `0` when nothing is selected.
#[must_use]
pub(crate) fn first_shown(
    block_heights: &[usize],
    selected: Option<usize>,
    height: usize,
) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let mut first = selected;
    let mut shown = block_heights.get(selected).copied().unwrap_or(1);
    while first > 0 {
        let Some(&before) = block_heights.get(first - 1) else {
            break;
        };
        if shown + before > height {
            break;
        }
        first -= 1;
        shown += before;
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_selected_starts_at_the_first_block() {
        assert_eq!(first_shown(&[1, 1, 1], None, 2), 0);
    }

    #[test]
    fn every_block_fits_and_the_window_starts_at_the_first() {
        assert_eq!(first_shown(&[3, 3, 3], Some(0), 9), 0);
        assert_eq!(first_shown(&[3, 3, 3], Some(2), 9), 0);
    }

    #[test]
    fn the_selected_block_ends_the_window_when_it_would_not_fit_growing_from_the_first() {
        assert_eq!(first_shown(&[3, 3, 3, 3], Some(3), 6), 2);
        assert_eq!(first_shown(&[3, 3, 3, 3], Some(1), 6), 0);
    }

    #[test]
    fn a_block_taller_than_the_whole_window_is_still_shown_alone() {
        assert_eq!(first_shown(&[3, 3], Some(1), 2), 1);
    }
}
