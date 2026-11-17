//! Wheel normalization independent of terminal input modes.

/// Maximum work accepted from a single native wheel event.
pub const MAX_SCROLL_STEPS: i32 = 100;

/// A wheel gesture at a visible source-terminal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalScroll {
    pub lines: i32,
    pub column: u16,
    pub row: u16,
    /// Dimensions of the source frame used for hit testing.
    pub size: (usize, usize),
    /// Crossterm modifier bits, as expected by the HERDR terminal protocol.
    pub modifiers: u8,
}

#[derive(Default)]
pub(crate) struct ScrollAccumulator {
    pending: f64,
}

impl ScrollAccumulator {
    pub(crate) fn reset(&mut self) {
        self.pending = 0.0;
    }

    pub(crate) fn steps(&mut self, rows: f64, precise: bool, started: bool, ended: bool) -> i32 {
        if started {
            self.reset();
        }
        if !rows.is_finite() {
            self.reset();
            return 0;
        }
        let steps = if rows == 0.0 {
            0
        } else if precise {
            if rows != 0.0 && self.pending.signum() != rows.signum() {
                self.reset();
            }
            self.pending += rows;
            let nearest = self.pending.round();
            if (self.pending - nearest).abs() < 1e-6 {
                self.pending = nearest;
            }
            let whole = self.pending.trunc();
            self.pending -= whole;
            whole.clamp(-f64::from(MAX_SCROLL_STEPS), f64::from(MAX_SCROLL_STEPS)) as i32
        } else {
            // macOS can report a slow discrete notch as 0.1 lines.
            self.reset();
            (rows.signum() * rows.abs().max(1.0).round())
                .clamp(-f64::from(MAX_SCROLL_STEPS), f64::from(MAX_SCROLL_STEPS)) as i32
        };
        if ended {
            self.reset();
        }
        steps
    }
}

pub(crate) fn cell_at(
    x: f32,
    y: f32,
    cell_width: f32,
    cell_height: f32,
    columns: usize,
    rows: usize,
) -> Option<(u16, u16)> {
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return None;
    }
    let column = (x / cell_width).floor() as usize;
    let row = (y / cell_height).floor() as usize;
    if column >= columns || row >= rows {
        return None;
    }
    Some((u16::try_from(column).ok()?, u16::try_from(row).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_accumulates_reverses_and_resets_between_gestures() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(
            (0..12)
                .map(|_| scroll.steps(0.25, true, false, false))
                .sum::<i32>(),
            3
        );
        assert_eq!(
            (0..10)
                .map(|_| scroll.steps(0.1, true, false, false))
                .sum::<i32>(),
            1
        );
        assert_eq!(scroll.steps(0.75, true, false, false), 0);
        assert_eq!(scroll.steps(-0.75, true, false, false), 0);
        assert_eq!(scroll.steps(-0.25, true, false, false), -1);
        assert_eq!(scroll.steps(0.75, true, false, true), 0);
        assert_eq!(scroll.steps(0.25, true, true, false), 0);
        assert_eq!(scroll.steps(0.75, true, false, false), 1);
    }

    #[test]
    fn discrete_ticks_and_extreme_events_are_bounded() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(scroll.steps(0.1, false, false, false), 1);
        assert_eq!(scroll.steps(-0.1, false, false, false), -1);
        assert_eq!(scroll.steps(0.0, false, false, false), 0);
        assert_eq!(scroll.steps(10_000.0, true, false, false), MAX_SCROLL_STEPS);
        assert_eq!(scroll.steps(f64::NAN, true, false, false), 0);
    }

    #[test]
    fn pointer_cells_reject_padding_and_outside_source_grid() {
        assert_eq!(cell_at(25.0, 41.0, 10.0, 20.0, 80, 24), Some((2, 2)));
        assert_eq!(cell_at(-0.1, 0.0, 10.0, 20.0, 80, 24), None);
        assert_eq!(cell_at(800.0, 0.0, 10.0, 20.0, 80, 24), None);
        assert_eq!(cell_at(0.0, 480.0, 10.0, 20.0, 80, 24), None);
        assert_eq!(cell_at(0.0, 0.0, 10.0, 20.0, 0, 0), None);
    }
}
