//! Turning a frame: exact at quarter turns (equal to drawing in the new rotation), sane between.

use tower_hmi::turn::{self, ONE, QUARTER};
use tower_hmi::{Color, Frame, Rect, Rotation, TextStyle, fonts};

type F = Frame<16, 128>;

const ROTATIONS: [Rotation; 4] = [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270];

/// An asymmetric picture (text, an off-center box, a diagonal), drawn in `rotation`.
fn scene(rotation: Rotation) -> F {
    let mut f = F::new();
    f.set_rotation(rotation);
    let mut c = f.canvas();
    c.fill_rect(Rect::new(0, 0, 128, 18), Color::Black);
    c.text(4, 1, "Turn me", &TextStyle::new(&fonts::SPLEEN_8X16).inverted());
    c.round_rect(Rect::new(10, 30, 60, 40), 6, Color::Black);
    c.line(0, 127, 90, 75, Color::Black);
    c.fill_circle(100, 100, 12, Color::Black);
    f
}

#[test]
fn quarter_turns_equal_a_redraw() {
    for from in ROTATIONS {
        for to in ROTATIONS {
            let want = scene(to);
            let q = turn::quarters(from, to);

            let mut in_place = scene(from);
            in_place.turn_quarters(q);
            assert_eq!(in_place.bytes(), want.bytes(), "turn_quarters {from:?} -> {to:?}");

            let snapshot = scene(from);
            let mut drawn = F::new();
            drawn.draw_turned(snapshot.bytes(), q * QUARTER, ONE);
            assert_eq!(drawn.bytes(), want.bytes(), "draw_turned {from:?} -> {to:?}");
        }
    }
}

#[test]
fn full_turn_and_zero_are_identity() {
    let s = scene(Rotation::R0);
    for a in [0, 4 * QUARTER, -4 * QUARTER] {
        let mut f = F::new();
        f.draw_turned(s.bytes(), a, ONE);
        assert_eq!(f.bytes(), s.bytes(), "{a}");
    }
}

#[test]
fn intermediate_steps_stay_on_screen_when_shrunk() {
    // A frame-filling square turned by any angle and shrunk to fit leaves the corners paper and
    // keeps ink around the center.
    let mut full = F::new();
    full.canvas().clear(Color::Black);
    for a in (0..=2 * QUARTER).step_by(8) {
        let mut f = F::new();
        f.draw_turned(full.bytes(), a, turn::fit(a));
        let c = f.canvas();
        assert!(c.get_pixel(64, 64) && c.get_pixel(60, 70), "center lost at {a}");
        if a % QUARTER != 0 {
            assert!(
                !c.get_pixel(0, 0) && !c.get_pixel(127, 127),
                "corner inked at {a}"
            );
        }
    }
}

#[test]
fn turned_steps_mark_rows_dirty() {
    let s = scene(Rotation::R0);
    let mut f = F::new();
    f.draw_turned(s.bytes(), QUARTER / 3, ONE);
    // Starting from a blank frame, exactly the rows that now hold ink changed.
    let inked: Vec<usize> = (0..128).filter(|&y| f.row(y).iter().any(|b| *b != 0)).collect();
    assert!(inked.len() > 50);
    assert_eq!(f.dirty_rows().collect::<Vec<_>>(), inked);
    f.clear_all_dirty();
    f.draw_turned(s.bytes(), QUARTER / 3, ONE); // the same step again changes nothing
    assert!(!f.any_dirty());
}
