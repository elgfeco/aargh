//! Pure layout helpers — keeping `app.rs` focused on the event loop.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Split the terminal into 4 panes as described in the crate docs.
pub fn four_panel_layout(area: Rect) -> [Rect; 4] {
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(25),
            Constraint::Percentage(40),
        ])
        .split(horizontal[0]);

    // order: [book, positions, logs, right_column (caller splits further)]
    [left[0], left[1], left[2], horizontal[1]]
}
