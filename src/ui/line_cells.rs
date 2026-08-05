//! Box-drawing line cells shared by every surface that draws joined lines.
//!
//! Both items lived in `src/ui/panes.rs` and were module-private, which made
//! the pane-border joining logic unreachable from the DAG overlay
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §8). They now live
//! here so `panes.rs` and `workflow_dag.rs` accumulate direction bits the same
//! way and crossings resolve to the same glyphs.

/// Which of the four directions a line occupies in one terminal cell.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct LineCell {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

/// The box-drawing glyph for one cell's accumulated directions. An empty cell
/// stringifies to `""`, which callers skip rather than draw.
pub(crate) fn line_cell_symbol(line: LineCell) -> &'static str {
    match (line.up, line.down, line.left, line.right) {
        (true, true, true, true) => "┼",
        (true, true, true, false) => "┤",
        (true, true, false, true) => "├",
        (true, false, true, true) => "┴",
        (false, true, true, true) => "┬",
        (true, true, false, false) | (true, false, false, false) | (false, true, false, false) => {
            "│"
        }
        (false, false, true, true) | (false, false, true, false) | (false, false, false, true) => {
            "─"
        }
        (false, true, false, true) => "┌",
        (false, true, true, false) => "┐",
        (true, false, false, true) => "└",
        (true, false, true, false) => "┘",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(up: bool, down: bool, left: bool, right: bool) -> LineCell {
        LineCell {
            up,
            down,
            left,
            right,
        }
    }

    #[test]
    fn crossings_and_tees_join() {
        assert_eq!(line_cell_symbol(cell(true, true, true, true)), "┼");
        assert_eq!(line_cell_symbol(cell(false, true, true, true)), "┬");
        assert_eq!(line_cell_symbol(cell(true, false, true, true)), "┴");
        assert_eq!(line_cell_symbol(cell(true, true, true, false)), "┤");
        assert_eq!(line_cell_symbol(cell(true, true, false, true)), "├");
    }

    #[test]
    fn straight_runs_and_corners_join() {
        assert_eq!(line_cell_symbol(cell(true, true, false, false)), "│");
        assert_eq!(line_cell_symbol(cell(false, false, true, true)), "─");
        assert_eq!(line_cell_symbol(cell(false, true, false, true)), "┌");
        assert_eq!(line_cell_symbol(cell(false, true, true, false)), "┐");
        assert_eq!(line_cell_symbol(cell(true, false, false, true)), "└");
        assert_eq!(line_cell_symbol(cell(true, false, true, false)), "┘");
    }

    #[test]
    fn empty_cell_has_no_glyph() {
        assert_eq!(line_cell_symbol(LineCell::default()), "");
    }
}
