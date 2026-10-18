//! Shell command/response syntax highlighting, shared by the `tower console` TUI
//! (`tui.rs`) and the `tower gateway` remote-shell pane (`gateway/tui.rs`) so the two
//! never drift. Pure `&str -> ratatui spans`, no state.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const COL_PATH: Color = Color::Cyan;
const COL_CMD: Color = Color::Yellow;
const COL_KEY: Color = Color::Magenta;
const COL_VAL: Color = Color::Green;
const COL_PUNCT: Color = Color::DarkGray;

/// A word of the command tree (a menu, a command or a setting name): lowercase letters,
/// digits and hyphens, starting with a letter.
fn is_word(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_lowercase())
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A `key=value` argument (both sides non-empty).
fn is_assignment(w: &str) -> bool {
    w.split_once('=')
        .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty())
}

/// Highlight a shell command line: `system settings set addr=random` → the leading words
/// (the path into the tree) cyan, the last of them (the command) yellow, `key=value`
/// magenta/green, any other argument green.
pub(crate) fn command(line: &str) -> Vec<Span<'static>> {
    let words: Vec<&str> = line.split(' ').collect();
    let path_len = words.iter().take_while(|w| is_word(w)).count();
    let mut spans = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        if word.is_empty() {
            // a run of spaces
        } else if is_assignment(word) {
            let (k, v) = word.split_once('=').unwrap();
            spans.push(Span::styled(k.to_string(), Style::new().fg(COL_KEY)));
            spans.push(Span::styled("=".to_string(), Style::new().fg(COL_PUNCT)));
            spans.push(Span::styled(v.to_string(), Style::new().fg(COL_VAL)));
        } else {
            let col = match i + 1 {
                n if n < path_len => COL_PATH,
                n if n == path_len => COL_CMD,
                _ => COL_VAL,
            };
            spans.push(Span::styled(word.to_string(), Style::new().fg(col)));
        }
    }
    spans
}

/// Whether a response line reads as a command (an `export` line): a `key=value` argument
/// after nothing but tree words.
fn looks_like_command(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.iter().position(|w| is_assignment(w)) {
        Some(at) => at > 0 && words[..at].iter().all(|w| is_word(w)),
        None => false,
    }
}

/// Highlight a shell-response line: command-syntax lines (`export` output) reuse [`command`];
/// `key: value` / `key = value` lines split into a magenta key, dim separator, and green value;
/// anything else renders raw.
pub(crate) fn response(line: &str) -> Line<'static> {
    let l = line;
    if looks_like_command(l) {
        return Line::from(command(l));
    }
    if l.starts_with('>') {
        // The local echo of the command the user sent.
        let mut spans = vec![Span::styled("> ".to_string(), Style::new().fg(COL_PUNCT))];
        spans.extend(command(l.trim_start_matches("> ")));
        return Line::from(spans);
    }
    for sep in [" = ", ": "] {
        if let Some((k, v)) = l.split_once(sep) {
            // Only treat it as key/value when the key looks like one (single-ish word).
            if !k.is_empty() && k.len() <= 24 && !k.contains("  ") {
                return Line::from(vec![
                    Span::styled(k.to_string(), Style::new().fg(COL_KEY)),
                    Span::styled(sep.to_string(), Style::new().fg(COL_PUNCT)),
                    Span::styled(v.to_string(), Style::new().fg(COL_VAL)),
                ]);
            }
        }
    }
    Line::raw(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The color of each non-space span of `line` as `command` paints it.
    fn colors(line: &str) -> Vec<(String, Option<Color>)> {
        command(line)
            .into_iter()
            .filter(|s| !s.content.trim().is_empty())
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    #[test]
    fn command_paints_path_command_and_arguments() {
        let joined: String = command("system settings set addr=random")
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            joined, "system settings set addr=random",
            "text survives verbatim"
        );
        let c = |s: &str, col| (s.to_string(), Some(col));
        assert_eq!(
            colors("system settings set addr=random"),
            [
                c("system", COL_PATH),
                c("settings", COL_PATH),
                c("set", COL_CMD),
                c("addr", COL_KEY),
                c("=", COL_PUNCT),
                c("random", COL_VAL)
            ]
        );
        assert_eq!(
            colors("button simulate 1500"),
            [c("button", COL_PATH), c("simulate", COL_CMD), c("1500", COL_VAL)]
        );
        assert_eq!(colors("export"), [c("export", COL_CMD)]);
    }

    /// `export` output reads as commands; prose and `key: value` lines don't.
    #[test]
    fn response_spots_command_lines() {
        assert!(looks_like_command("system settings set identity=tower"));
        assert!(!looks_like_command("rebooting"));
        assert!(!looks_like_command("therm-period = 60"));
        assert!(!looks_like_command("uid: 01473930-36353931-00340027"));
        assert!(!looks_like_command("x=1"), "a bare assignment is no command");
    }

    #[test]
    fn response_splits_key_value_and_passes_raw() {
        let kv = response("therm-period = 60");
        let txt: String = kv.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "therm-period = 60");
        assert!(kv.spans.len() >= 3, "key / sep / value");
        // A plain line with no separator survives verbatim as one raw span.
        let raw = response("just some text");
        let txt: String = raw.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "just some text");
    }
}
