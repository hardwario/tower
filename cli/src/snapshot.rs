//! Snapshots of the two TUIs (test only): a frame drawn into ratatui's `TestBackend`, written
//! out as text for `insta` to compare with the `.snap` file committed next to the TUI's source.
//!
//! A snapshot has three parts. The **screen**: the cells as a terminal shows them, row by row.
//! The **style map**: the same grid with one letter per cell for its colours and attributes
//! (`.` for an unstyled cell), so a chip that lost its highlight or a border that lost its focus
//! colour shows as a changed letter even though the text did not move. The **legend**: what each
//! letter stands for, in order of first appearance. Rows are right-trimmed (trailing blanks and
//! unstyled cells carry nothing), so the files stay clean under any editor.
//!
//! The snapshots are what the layout looked like when a human last approved it: the truncated
//! response pane, the missing margins and the unreadable zoom and pause chips were all found by
//! eye, after the fact. A change to them is reviewed like a change to the code:
//!
//! ```sh
//! cargo test -p tower-cli                       # a changed frame fails, and leaves <name>.snap.new
//! INSTA_UPDATE=always cargo test -p tower-cli   # accept every changed frame, then review `git diff`
//! ```

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

/// The letters of the style map, in order of first use.
const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Draw one frame at `width` x `height` and write it out (module docs). `cursor`: the frame
/// places the terminal cursor (an input line with the focus), so where it put it is part of
/// the snapshot.
pub(crate) fn shoot(width: u16, height: u16, cursor: bool, draw: impl FnOnce(&mut Frame)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal.draw(draw).expect("the frame draws");
    let mut out = format!("{width}x{height}");
    if cursor {
        let at = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("the test backend has a cursor");
        out.push_str(&format!(", cursor at column {} row {}", at.x, at.y));
    }
    out.push('\n');

    let buf = terminal.backend().buffer();
    let mut styles: Vec<(Color, Color, Modifier)> = Vec::new();
    let mut screen = String::new();
    let mut map = String::new();
    for y in 0..buf.area.height {
        let mut row = String::new();
        let mut row_map = String::new();
        // A wide glyph takes two columns: the cell after it holds nothing of its own.
        let mut covered = 0;
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            if covered > 0 {
                covered -= 1;
            } else {
                row.push_str(cell.symbol());
                covered = Line::raw(cell.symbol()).width().saturating_sub(1);
            }
            let style = (cell.fg, cell.bg, cell.modifier);
            if style == (Color::Reset, Color::Reset, Modifier::empty()) {
                row_map.push('.');
                continue;
            }
            let index = styles.iter().position(|s| *s == style).unwrap_or_else(|| {
                styles.push(style);
                styles.len() - 1
            });
            row_map.push(char::from(*LETTERS.get(index).expect("more styles than letters")));
        }
        screen.push_str(row.trim_end());
        screen.push('\n');
        map.push_str(row_map.trim_end_matches('.'));
        map.push('\n');
    }

    out.push_str("--- screen ---\n");
    out.push_str(&screen);
    out.push_str("--- styles ---\n");
    out.push_str(&map);
    out.push_str("--- legend ---\n");
    for (index, (fg, bg, modifier)) in styles.iter().enumerate() {
        out.push_str(&format!("{}:", char::from(LETTERS[index])));
        if *fg != Color::Reset {
            out.push_str(&format!(" {fg:?}"));
        }
        if *bg != Color::Reset {
            out.push_str(&format!(" on {bg:?}"));
        }
        if !modifier.is_empty() {
            out.push_str(&format!(" {modifier:?}"));
        }
        out.push('\n');
    }
    out
}
