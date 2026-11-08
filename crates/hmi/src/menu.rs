//! Declarative menus: a tree of [`Menu`]s and [`Item`]s defined as `static` data, a
//! [`MenuState`] that navigates it, and a renderer.
//!
//! The menu holds no values of its own. Toggles, numbers and choices name a `u16` id, and the
//! application supplies a [`Values`] that reads and writes them, so a value can live wherever
//! the application keeps it (a struct, an atomic, a setting in EEPROM). Navigation takes four
//! commands ([`Nav`]); the driver maps buttons or gestures onto them.
//!
//! ```
//! use tower_hmi::menu::{Item, Menu, MenuState, Nav, Outcome, Table};
//!
//! const LIGHT: u16 = 1;
//! const PERIOD: u16 = 2;
//! const MODE: u16 = 3;
//! const REBOOT: u16 = 10;
//!
//! static DISPLAY: Menu = Menu::new("Display", &[
//!     Item::toggle("Backlight", LIGHT),
//!     Item::choice("Mode", MODE, &["Eco", "Normal", "Boost"]),
//!     Item::back("Back"),
//! ]);
//! static MAIN: Menu = Menu::new("Settings", &[
//!     Item::submenu("Display", &DISPLAY),
//!     Item::number("Period", PERIOD, 1, 60, 1).unit(" min"),
//!     Item::action("Reboot", REBOOT),
//! ]);
//!
//! let mut values = Table::new([(LIGHT, 0), (PERIOD, 15), (MODE, 1)]);
//! let mut menu = MenuState::new(&MAIN);
//! menu.handle(Nav::Enter, &mut values); // into Display
//! assert_eq!(menu.handle(Nav::Enter, &mut values), Some(Outcome::Changed { id: LIGHT, value: 1 }));
//! ```
//!
//! The navigation rules: [`Nav::Prev`] / [`Nav::Next`] move the selection (wrapping around);
//! [`Nav::Enter`] runs an action ([`Outcome::Action`]), opens a submenu, flips a toggle, or starts
//! editing a number or a choice; [`Nav::Back`] leaves a submenu, or the root menu
//! ([`Outcome::Exit`]). While editing, `Prev` / `Next` step the value (a number stops at its
//! limits, a choice wraps around), `Enter` stores it ([`Outcome::Changed`]) and `Back` cancels.

use core::fmt::{self, Write};

use crate::canvas::{Align, Canvas, Color, TextStyle};
use crate::font::Font;
use crate::geom::Rect;

/// One level of the tree: a title and its items.
pub struct Menu {
    pub title: &'static str,
    pub items: &'static [Item],
}

impl Menu {
    pub const fn new(title: &'static str, items: &'static [Item]) -> Self {
        Self { title, items }
    }
}

/// A menu entry: a label and what it does.
#[derive(Clone, Copy)]
pub struct Item {
    pub label: &'static str,
    pub kind: ItemKind,
}

/// What an [`Item`] does.
#[derive(Clone, Copy)]
pub enum ItemKind {
    /// Report [`Outcome::Action`] with this id.
    Action(u16),
    /// Open a submenu.
    Submenu(&'static Menu),
    /// An on/off value (0 = off, anything else = on), flipped by `Enter`.
    Toggle(u16),
    /// An integer value edited in steps.
    Number(Number),
    /// An index into `options`.
    Choice {
        id: u16,
        options: &'static [&'static str],
    },
    /// Go up one level (as [`Nav::Back`]).
    Back,
}

/// The parameters of a numeric item: the range, the step, and how it is shown (`value` with
/// `decimals` implied decimal places, then `unit`: 215 with one decimal and unit `" °C"` shows
/// `21.5 °C`).
#[derive(Clone, Copy)]
pub struct Number {
    pub id: u16,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub decimals: u8,
    pub unit: &'static str,
}

impl Item {
    pub const fn action(label: &'static str, id: u16) -> Self {
        Self {
            label,
            kind: ItemKind::Action(id),
        }
    }

    pub const fn submenu(label: &'static str, menu: &'static Menu) -> Self {
        Self {
            label,
            kind: ItemKind::Submenu(menu),
        }
    }

    pub const fn toggle(label: &'static str, id: u16) -> Self {
        Self {
            label,
            kind: ItemKind::Toggle(id),
        }
    }

    /// A number in `min..=max`, changed by `step` (no unit, no decimals: see
    /// [`unit`](Self::unit) and [`decimals`](Self::decimals)).
    pub const fn number(label: &'static str, id: u16, min: i32, max: i32, step: i32) -> Self {
        Self {
            label,
            kind: ItemKind::Number(Number {
                id,
                min,
                max,
                step,
                decimals: 0,
                unit: "",
            }),
        }
    }

    pub const fn choice(label: &'static str, id: u16, options: &'static [&'static str]) -> Self {
        Self {
            label,
            kind: ItemKind::Choice { id, options },
        }
    }

    pub const fn back(label: &'static str) -> Self {
        Self {
            label,
            kind: ItemKind::Back,
        }
    }

    /// The unit shown after a number's value (include a leading space if you want one).
    pub const fn unit(self, unit: &'static str) -> Self {
        match self.kind {
            ItemKind::Number(mut n) => {
                n.unit = unit;
                Self {
                    label: self.label,
                    kind: ItemKind::Number(n),
                }
            }
            _ => self,
        }
    }

    /// Show a number with `decimals` implied decimal places (the value 215 with 1 shows 21.5).
    pub const fn decimals(self, decimals: u8) -> Self {
        match self.kind {
            ItemKind::Number(mut n) => {
                n.decimals = decimals;
                Self {
                    label: self.label,
                    kind: ItemKind::Number(n),
                }
            }
            _ => self,
        }
    }
}

/// Where the menu's values live. `get` of an id the application doesn't know may return 0.
pub trait Values {
    fn get(&self, id: u16) -> i32;
    fn set(&mut self, id: u16, value: i32);
}

/// A ready-made [`Values`]: a fixed table of `(id, value)` pairs (ids not in it read 0 and
/// ignore writes).
pub struct Table<const N: usize> {
    entries: [(u16, i32); N],
}

impl<const N: usize> Table<N> {
    pub const fn new(entries: [(u16, i32); N]) -> Self {
        Self { entries }
    }
}

impl<const N: usize> Values for Table<N> {
    fn get(&self, id: u16) -> i32 {
        self.entries.iter().find(|e| e.0 == id).map_or(0, |e| e.1)
    }

    fn set(&mut self, id: u16, value: i32) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == id) {
            e.1 = value;
        }
    }
}

/// A navigation command (see the [module docs](self)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Prev,
    Next,
    Enter,
    Back,
}

/// What a [`MenuState::handle`] did beyond moving around the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// An action item was entered.
    Action(u16),
    /// A value was stored (already written through [`Values::set`]).
    Changed { id: u16, value: i32 },
    /// `Back` at the root menu.
    Exit,
}

/// How deep submenus may nest.
pub const MAX_DEPTH: usize = 6;

#[derive(Clone, Copy)]
struct Level {
    menu: &'static Menu,
    selected: u8,
    top: u8,
}

/// The position in a menu tree, and a value being edited (see the [module docs](self)).
pub struct MenuState {
    stack: [Level; MAX_DEPTH],
    depth: usize,
    edit: Option<i32>,
}

impl MenuState {
    pub const fn new(root: &'static Menu) -> Self {
        let l = Level {
            menu: root,
            selected: 0,
            top: 0,
        };
        Self {
            stack: [l; MAX_DEPTH],
            depth: 1,
            edit: None,
        }
    }

    /// Back to the top of the root menu, nothing being edited.
    pub fn reset(&mut self) {
        self.depth = 1;
        self.stack[0].selected = 0;
        self.stack[0].top = 0;
        self.edit = None;
    }

    /// The menu shown now.
    pub fn menu(&self) -> &'static Menu {
        self.level().menu
    }

    /// Index of the selected item.
    pub fn selected(&self) -> usize {
        self.level().selected as usize
    }

    /// The value being edited, if any.
    pub fn editing(&self) -> Option<i32> {
        self.edit
    }

    /// 1 at the root menu, 2 in its submenu, and so on.
    pub fn depth(&self) -> usize {
        self.depth
    }

    fn level(&self) -> &Level {
        &self.stack[self.depth - 1]
    }

    fn level_mut(&mut self) -> &mut Level {
        &mut self.stack[self.depth - 1]
    }

    /// Apply a navigation command. `None` means only the view changed (redraw it).
    pub fn handle(&mut self, nav: Nav, values: &mut impl Values) -> Option<Outcome> {
        let items = self.menu().items;
        let Some(item) = items.get(self.selected()).copied() else {
            return if nav == Nav::Back { self.up() } else { None };
        };
        if let Some(v) = self.edit {
            match (nav, item.kind) {
                (Nav::Prev | Nav::Next, ItemKind::Number(n)) => {
                    let d = if nav == Nav::Next { n.step } else { -n.step };
                    self.edit = Some(v.saturating_add(d).clamp(n.min, n.max));
                }
                (Nav::Prev | Nav::Next, ItemKind::Choice { options, .. }) => {
                    let len = options.len().max(1) as i32;
                    let d = if nav == Nav::Next { 1 } else { -1 };
                    self.edit = Some((v + d).rem_euclid(len));
                }
                (Nav::Enter, ItemKind::Number(Number { id, .. }) | ItemKind::Choice { id, .. }) => {
                    self.edit = None;
                    values.set(id, v);
                    return Some(Outcome::Changed { id, value: v });
                }
                _ => self.edit = None,
            }
            return None;
        }
        let n = items.len() as u8;
        match nav {
            Nav::Prev => {
                let l = self.level_mut();
                l.selected = (l.selected + n - 1) % n;
            }
            Nav::Next => {
                let l = self.level_mut();
                l.selected = (l.selected + 1) % n;
            }
            Nav::Back => return self.up(),
            Nav::Enter => match item.kind {
                ItemKind::Action(id) => return Some(Outcome::Action(id)),
                ItemKind::Submenu(menu) => {
                    if self.depth < MAX_DEPTH {
                        self.depth += 1;
                        *self.level_mut() = Level {
                            menu,
                            selected: 0,
                            top: 0,
                        };
                    }
                }
                ItemKind::Toggle(id) => {
                    let value = (values.get(id) == 0) as i32;
                    values.set(id, value);
                    return Some(Outcome::Changed { id, value });
                }
                ItemKind::Number(num) => self.edit = Some(values.get(num.id).clamp(num.min, num.max)),
                ItemKind::Choice { id, options } => {
                    self.edit = Some(values.get(id).clamp(0, options.len().saturating_sub(1) as i32))
                }
                ItemKind::Back => return self.up(),
            },
        }
        None
    }

    fn up(&mut self) -> Option<Outcome> {
        if self.depth > 1 {
            self.depth -= 1;
            None
        } else {
            Some(Outcome::Exit)
        }
    }

    /// Draw the menu over the whole canvas.
    pub fn render(&mut self, c: &mut Canvas<'_>, values: &impl Values, style: &MenuStyle) {
        let area = c.bounds();
        self.render_in(c, area, values, style);
    }

    /// Draw the menu into `area`: a title bar, then as many rows as fit, scrolled to keep the
    /// selection in view, with a scroll bar when they don't all fit.
    pub fn render_in(&mut self, c: &mut Canvas<'_>, area: Rect, values: &impl Values, style: &MenuStyle) {
        c.with_clip(area, |c| {
            c.clear(Color::White);
            let menu = self.menu();
            let (tf, f) = (style.title_font, style.font);
            let title_h = tf.height as i32 + 2;
            c.fill_rect(Rect::new(area.x, area.y, area.w, title_h), Color::Black);
            let title = TextStyle::new(tf).color(Color::White).align(Align::Center);
            c.text(area.x + area.w / 2, area.y + 1, menu.title, &title);
            if self.depth > 1 {
                // A back marker: a small left-pointing triangle in the title bar.
                let (x, cy) = (area.x + 2, area.y + title_h / 2);
                for i in 0..4 {
                    c.vline(x + i, cy - i, 2 * i + 1, Color::White);
                }
            }

            let row_h = f.height as i32 + style.row_gap;
            let top_y = area.y + title_h + 1;
            let visible = ((area.bottom() - top_y) / row_h).max(1) as usize;
            let n = menu.items.len();
            let sel = self.selected();
            let l = self.level_mut();
            if sel < l.top as usize {
                l.top = sel as u8;
            } else if sel >= l.top as usize + visible {
                l.top = (sel + 1 - visible) as u8;
            }
            let top = l.top as usize;
            let bar = if n > visible { 4 } else { 0 };
            let right = area.right() - style.padding - bar;

            for (i, item) in menu.items.iter().enumerate().skip(top).take(visible) {
                let y = top_y + (i - top) as i32 * row_h;
                let row = Rect::new(area.x, y, area.w - bar, row_h);
                let ty = y + style.row_gap / 2;
                let selected = i == sel;
                let editing = selected && self.edit.is_some();
                let value = Value::of(item, if editing { self.edit } else { None }, values);
                let vw = value.width(f);
                let (fg, bg) = if selected && !editing {
                    (Color::White, Color::Black)
                } else {
                    (Color::Black, Color::White)
                };
                c.fill_rect(row, bg);
                let label_w = right - area.x - style.padding - if vw > 0 { vw + f.width as i32 } else { 0 };
                let label = TextStyle::new(f).color(fg);
                c.with_clip(Rect::new(area.x + style.padding, y, label_w, row_h), |c| {
                    c.text(area.x + style.padding, ty, item.label, &label);
                });
                if editing {
                    c.rect(row, Color::Black);
                    let vr = Rect::new(right - vw - 2, y + 1, vw + 4, row_h - 2);
                    c.fill_rect(vr, Color::Black);
                    let st = TextStyle::new(f).color(Color::White).align(Align::Right);
                    c.text_fmt(right, ty, format_args!("{value}"), &st);
                } else if vw > 0 {
                    let st = TextStyle::new(f).color(fg).align(Align::Right);
                    c.text_fmt(right, ty, format_args!("{value}"), &st);
                }
            }

            if bar > 0 {
                let track = Rect::new(area.right() - 3, top_y, 2, area.bottom() - top_y);
                c.fill_rect(track, Color::White);
                c.vline(track.x + 1, track.y, track.h, Color::Black);
                let thumb_h = (track.h * visible as i32 / n as i32).max(4);
                let thumb_y = track.y + (track.h - thumb_h) * top as i32 / (n - visible).max(1) as i32;
                c.fill_rect(Rect::new(track.x - 1, thumb_y, 3, thumb_h), Color::Black);
            }
        });
    }
}

/// How [`MenuState::render`] looks.
#[derive(Clone, Copy)]
pub struct MenuStyle {
    /// The item rows.
    pub font: &'static Font,
    /// The title bar.
    pub title_font: &'static Font,
    /// Left and right margin of the text, in pixels.
    pub padding: i32,
    /// Extra pixels per row on top of the font height.
    pub row_gap: i32,
}

impl MenuStyle {
    pub const fn new(font: &'static Font) -> Self {
        Self {
            font,
            title_font: font,
            padding: 3,
            row_gap: 2,
        }
    }

    pub const fn title_font(mut self, font: &'static Font) -> Self {
        self.title_font = font;
        self
    }

    pub const fn row_gap(mut self, gap: i32) -> Self {
        self.row_gap = gap;
        self
    }
}

/// The right-hand text of a row.
enum Value {
    None,
    Text(&'static str),
    Number {
        value: i32,
        decimals: u8,
        unit: &'static str,
    },
}

impl Value {
    fn of(item: &Item, edit: Option<i32>, values: &impl Values) -> Value {
        match item.kind {
            ItemKind::Toggle(id) => Value::Text(if values.get(id) != 0 { "On" } else { "Off" }),
            ItemKind::Number(n) => Value::Number {
                value: edit.unwrap_or_else(|| values.get(n.id)),
                decimals: n.decimals,
                unit: n.unit,
            },
            ItemKind::Choice { id, options } => {
                let i = edit.unwrap_or_else(|| values.get(id));
                Value::Text(
                    usize::try_from(i)
                        .ok()
                        .and_then(|i| options.get(i))
                        .copied()
                        .unwrap_or("?"),
                )
            }
            ItemKind::Submenu(_) => Value::Text(">"),
            ItemKind::Action(_) | ItemKind::Back => Value::None,
        }
    }

    fn width(&self, f: &Font) -> i32 {
        let mut n = CharCount(0);
        let _ = write!(n, "{self}");
        n.0 * f.width as i32
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::None => Ok(()),
            Value::Text(t) => f.write_str(t),
            Value::Number {
                value,
                decimals: 0,
                unit,
            } => write!(f, "{value}{unit}"),
            Value::Number {
                value,
                decimals,
                unit,
            } => {
                let scale = 10u32.pow(decimals as u32);
                let sign = if value < 0 { "-" } else { "" };
                let a = value.unsigned_abs();
                let d = decimals as usize;
                write!(f, "{sign}{}.{:0d$}{unit}", a / scale, a % scale)
            }
        }
    }
}

struct CharCount(i32);

impl Write for CharCount {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.chars().count() as i32;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::SPLEEN_6X12;
    use crate::frame::Frame;

    const A: u16 = 1;
    const B: u16 = 2;
    const C: u16 = 3;
    static SUB: Menu = Menu::new("Sub", &[Item::toggle("Flag", A), Item::back("Back")]);
    static ROOT: Menu = Menu::new(
        "Root",
        &[
            Item::submenu("Sub", &SUB),
            Item::number("Num", B, 0, 10, 3).unit(" s"),
            Item::choice("Pick", C, &["x", "y", "z"]),
            Item::action("Go", 42),
        ],
    );

    #[test]
    fn navigate_and_edit() {
        let mut v = Table::new([(A, 0), (B, 5), (C, 0)]);
        let mut m = MenuState::new(&ROOT);
        assert_eq!(m.handle(Nav::Prev, &mut v), None);
        assert_eq!(m.selected(), 3); // wrapped
        assert_eq!(m.handle(Nav::Enter, &mut v), Some(Outcome::Action(42)));
        m.handle(Nav::Next, &mut v); // wrap to 0
        m.handle(Nav::Enter, &mut v); // into Sub
        assert_eq!((m.depth(), m.menu().title), (2, "Sub"));
        assert_eq!(
            m.handle(Nav::Enter, &mut v),
            Some(Outcome::Changed { id: A, value: 1 })
        );
        assert_eq!(v.get(A), 1);
        m.handle(Nav::Next, &mut v);
        assert_eq!(m.handle(Nav::Enter, &mut v), None); // the Back item
        assert_eq!(m.depth(), 1);
        assert_eq!(m.handle(Nav::Back, &mut v), Some(Outcome::Exit));

        m.handle(Nav::Next, &mut v); // Num
        m.handle(Nav::Enter, &mut v);
        assert_eq!(m.editing(), Some(5));
        m.handle(Nav::Next, &mut v);
        m.handle(Nav::Next, &mut v);
        assert_eq!(m.editing(), Some(10)); // clamped
        m.handle(Nav::Back, &mut v); // cancel
        assert_eq!((m.editing(), v.get(B)), (None, 5));
        m.handle(Nav::Enter, &mut v);
        m.handle(Nav::Prev, &mut v);
        assert_eq!(
            m.handle(Nav::Enter, &mut v),
            Some(Outcome::Changed { id: B, value: 2 })
        );

        m.handle(Nav::Next, &mut v); // Pick
        m.handle(Nav::Enter, &mut v);
        m.handle(Nav::Prev, &mut v); // wraps to "z"
        assert_eq!(
            m.handle(Nav::Enter, &mut v),
            Some(Outcome::Changed { id: C, value: 2 })
        );
    }

    #[test]
    fn value_text() {
        let v = Value::Number {
            value: -5,
            decimals: 1,
            unit: " °C",
        };
        assert_eq!(std::format!("{v}"), "-0.5 °C");
        let v = Value::Number {
            value: 1234,
            decimals: 2,
            unit: "",
        };
        assert_eq!(std::format!("{v}"), "12.34");
        assert_eq!(v.width(&SPLEEN_6X12), 5 * 6);
    }

    #[test]
    fn render_highlights_selection() {
        let v = Table::new([(A, 0), (B, 5), (C, 1)]);
        let mut m = MenuState::new(&ROOT);
        let mut f: Frame<16, 128> = Frame::new();
        let style = MenuStyle::new(&SPLEEN_6X12);
        m.render(&mut f.canvas(), &v, &style);
        // Title bar (rows 0..14) is ink at the left edge; the first item row is selected (ink
        // background), the second is not.
        let c = f.canvas();
        assert!(c.get_pixel(0, 0));
        let row_h = 12 + 2;
        let first = 14 + 1;
        assert!(c.get_pixel(1, first + 1));
        assert!(!c.get_pixel(1, first + row_h + 1));
    }
}
