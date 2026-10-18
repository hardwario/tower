//! The shell command history shared by the console TUI (`tower`), the shell pane of
//! `tower gateway` and the line-based `tower shell`: they speak one command language, so they
//! share one history, kept across sessions in a plain text file (one command per line, oldest
//! first). Up/Down browse it, Ctrl-R searches it ([`Search`]).
//!
//! The file is a convenience, never a reason to fail: one that can't be read or written just
//! leaves the session with its in-memory history. Every command is appended as it is run, so a
//! crash loses nothing and two TUIs running side by side both keep their commands.

use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Commands kept. Loading a longer file keeps the newest and rewrites the file.
pub(crate) const MAX_ENTRIES: usize = 1000;
/// Longest command kept (a shell line is far shorter; anything longer is not one).
const MAX_LEN: usize = 1024;
/// Overrides the history file's path; set but empty, nothing is read or written.
pub(crate) const FILE_ENV: &str = "TOWER_HISTORY_FILE";

/// The command history: the entries (oldest first), the file behind them, and the Up/Down
/// browsing position.
pub(crate) struct History {
    entries: Vec<String>,
    file: Option<PathBuf>,
    /// The entry shown while browsing (Up/Down); `None` = editing a fresh line.
    nav: Option<usize>,
    /// The line being typed when browsing started, given back by Down past the newest entry.
    draft: String,
}

impl History {
    /// The history of this user: the file at [`FILE_ENV`], else the platform's per-user state
    /// directory (`$XDG_STATE_HOME/tower/history` or `~/.local/state/tower/history` on Linux,
    /// `~/Library/Application Support/tower/history` on macOS, `%LOCALAPPDATA%\tower\history`
    /// on Windows).
    pub(crate) fn load() -> Self {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        match default_file(env(FILE_ENV), env) {
            Some(file) => Self::from_file(file),
            None => Self::in_memory(Vec::new()),
        }
    }

    /// A history that is never written anywhere (tests, or persistence turned off).
    pub(crate) fn in_memory(entries: Vec<String>) -> Self {
        Self {
            entries,
            file: None,
            nav: None,
            draft: String::new(),
        }
    }

    /// The history kept in `file` (missing = empty). Lines that are not commands are dropped;
    /// a file past [`MAX_ENTRIES`] is cut to the newest and rewritten.
    fn from_file(file: PathBuf) -> Self {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let mut entries: Vec<String> = text.lines().filter_map(clean).collect();
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
            let _ = rewrite(&file, &entries);
        }
        Self {
            entries,
            file: Some(file),
            nav: None,
            draft: String::new(),
        }
    }

    /// The entries, oldest first.
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record a command that was run: kept unless it is empty, not a single line of text, or
    /// the same as the previous one, and appended to the file at once. Ends any browsing.
    pub(crate) fn push(&mut self, line: &str) {
        self.stop_browsing();
        let Some(line) = clean(line) else {
            return;
        };
        if self.entries.last() == Some(&line) {
            return;
        }
        if let Some(file) = &self.file {
            let _ = append(file, &line);
        }
        self.entries.push(line);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
    }

    /// Up: the next older entry, or `None` at the oldest (or with no history). `current` is the
    /// line on screen, kept as the draft when browsing starts.
    pub(crate) fn up(&mut self, current: &str) -> Option<&str> {
        let i = match self.nav {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.nav = Some(i);
        Some(&self.entries[i])
    }

    /// Down: the next newer entry, the draft past the newest, `None` when not browsing.
    pub(crate) fn down(&mut self) -> Option<String> {
        let i = self.nav?;
        if i + 1 < self.entries.len() {
            self.nav = Some(i + 1);
            Some(self.entries[i + 1].clone())
        } else {
            self.nav = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// Editing the shown entry makes it a fresh line: the next Up starts from the newest again.
    pub(crate) fn stop_browsing(&mut self) {
        self.nav = None;
        self.draft.clear();
    }
}

/// The history file's path: `env_file` ([`FILE_ENV`]) when set (empty = none), else the
/// platform's per-user state directory, from `env` (a variable lookup, injected for tests).
fn default_file(env_file: Option<PathBuf>, env: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(file) = env_file {
        return (!file.as_os_str().is_empty()).then_some(file);
    }
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library").join("Application Support")
    } else {
        env("XDG_STATE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| env("HOME").map(|h| h.join(".local").join("state")))?
    };
    Some(base.join("tower").join("history"))
}

/// A command as kept: trimmed, one line of printable text, not empty, not absurdly long.
fn clean(line: &str) -> Option<String> {
    let line = line.trim();
    (!line.is_empty() && line.len() <= MAX_LEN && !line.chars().any(char::is_control))
        .then(|| line.to_string())
}

/// Append one command (one `write`, so two sessions appending at once don't interleave).
fn append(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    open_private(std::fs::OpenOptions::new().create(true).append(true), file)?
        .write_all(format!("{line}\n").as_bytes())
}

/// Replace the file with `entries`, through a temporary file renamed over it, so a crash
/// mid-write never leaves half a history.
fn rewrite(file: &Path, entries: &[String]) -> std::io::Result<()> {
    let tmp = file.with_extension("tmp");
    let mut text = entries.join("\n");
    text.push('\n');
    open_private(
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true),
        &tmp,
    )?
    .write_all(text.as_bytes())?;
    std::fs::rename(&tmp, file)
}

/// Open with owner-only permissions where the platform has them: the history holds what the
/// user typed, which is nobody else's business.
fn open_private(opts: &mut std::fs::OpenOptions, path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(opts, 0o600);
    opts.open(path)
}

/// Ctrl-R: an incremental search of the history, newest first. The query's words must all
/// appear in a command, in any order, ignoring ASCII case; each distinct command is offered
/// once, at its newest use.
pub(crate) struct Search {
    query: String,
    /// The entry shown, if any matches.
    hit: Option<usize>,
    /// The newer hits stepped past with [`older`](Self::older), for [`newer`](Self::newer).
    trail: Vec<usize>,
    /// The line being edited when the search started (given back on cancel).
    pub(crate) original: String,
    pub(crate) original_cursor: usize,
}

impl Search {
    pub(crate) fn new(original: &str, original_cursor: usize) -> Self {
        Self {
            query: String::new(),
            hit: None,
            trail: Vec::new(),
            original: original.to_string(),
            original_cursor,
        }
    }

    /// The command shown, if the query matches one.
    pub(crate) fn hit<'a>(&self, history: &'a History) -> Option<&'a str> {
        self.hit.map(|i| history.entries[i].as_str())
    }

    pub(crate) fn push_char(&mut self, history: &History, c: char) {
        self.query.push(c);
        self.restart(history);
    }

    pub(crate) fn pop_char(&mut self, history: &History) {
        self.query.pop();
        self.restart(history);
    }

    /// Ctrl-R again (or Up): the next older matching command.
    pub(crate) fn older(&mut self, history: &History) {
        let Some(cur) = self.hit else {
            return;
        };
        let seen = |i: usize| {
            let e = &history.entries[i];
            *e == history.entries[cur] || self.trail.iter().any(|&t| history.entries[t] == *e)
        };
        if let Some(i) = (0..cur)
            .rev()
            .find(|&i| matches(&history.entries[i], &self.query) && !seen(i))
        {
            self.trail.push(cur);
            self.hit = Some(i);
        }
    }

    /// Down: back to the newer match [`older`](Self::older) stepped past.
    pub(crate) fn newer(&mut self) {
        if let Some(i) = self.trail.pop() {
            self.hit = Some(i);
        }
    }

    /// A changed query searches from the newest command again.
    fn restart(&mut self, history: &History) {
        self.trail.clear();
        self.hit = if self.query.trim().is_empty() {
            None
        } else {
            (0..history.entries.len())
                .rev()
                .find(|&i| matches(&history.entries[i], &self.query))
        };
    }
}

/// The keys shown while a history search runs (both TUIs show it under the prompt).
pub(crate) const SEARCH_HINT: &str = "Ctrl-R / ↑ older · ↓ newer · Enter run · ←/→ edit · Esc cancel";
/// The prompt's label while a history search runs.
pub(crate) const SEARCH_LABEL: &str = "search: ";

/// What a key does to a running search, for the TUI to apply to its own line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SearchStep {
    /// The search goes on (the query or the match changed, or the key does nothing here).
    Continue,
    /// End it and run the line [`Search::finish`] gives (Enter).
    Run,
    /// End it with the match in the line for editing, then apply this key to the line, if any
    /// (Left/Right/Home/End move the cursor as usual; Tab only takes the match).
    Edit(Option<KeyCode>),
    /// End it and give back the line from before it (Esc, Ctrl-G).
    Cancel,
}

impl Search {
    /// Apply one key. `ctrl` = a Control/Alt chord (not AltGr text).
    pub(crate) fn key(&mut self, history: &History, code: KeyCode, ctrl: bool) -> SearchStep {
        match code {
            KeyCode::Char('r') if ctrl => self.older(history),
            KeyCode::Char('g') if ctrl => return SearchStep::Cancel,
            KeyCode::Char(c) if !ctrl => self.push_char(history, c),
            KeyCode::Up => self.older(history),
            KeyCode::Down => self.newer(),
            KeyCode::Backspace => self.pop_char(history),
            KeyCode::Esc => return SearchStep::Cancel,
            KeyCode::Enter => return SearchStep::Run,
            KeyCode::Tab => return SearchStep::Edit(None),
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {
                return SearchStep::Edit(Some(code));
            }
            _ => {}
        }
        SearchStep::Continue
    }

    /// The line and cursor the search leaves: the match (cursor at its end), or, with none,
    /// the line from before the search.
    pub(crate) fn finish(self, history: &History) -> (String, usize) {
        match self.hit(history) {
            Some(hit) => (hit.to_string(), hit.len()),
            None => (self.original, self.original_cursor),
        }
    }

    /// The prompt while the search runs: the label, the query, then the matching command with
    /// the query's words highlighted (or that nothing matches). The cursor belongs after the
    /// query, [`cursor_col`](Self::cursor_col) columns in.
    pub(crate) fn prompt(&self, history: &History) -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::styled(
                SEARCH_LABEL,
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.query.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("  ›  ", Style::new().fg(Color::DarkGray)),
        ];
        let Some(hit) = self.hit(history) else {
            if !self.query.trim().is_empty() {
                spans.push(Span::styled("no match", Style::new().fg(Color::Red)));
            }
            return spans;
        };
        let mark = Style::new()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let mut at = 0;
        for r in highlights(hit, &self.query) {
            spans.push(Span::raw(hit[at..r.start].to_string()));
            spans.push(Span::styled(hit[r.clone()].to_string(), mark));
            at = r.end;
        }
        spans.push(Span::raw(hit[at..].to_string()));
        spans
    }

    /// The prompt column (from its left edge) where the cursor goes: right after the query.
    pub(crate) fn cursor_col(&self) -> u16 {
        (SEARCH_LABEL.chars().count() + self.query.chars().count()) as u16
    }
}

/// Whether `entry` contains every whitespace-separated word of `query` (ASCII case ignored).
pub(crate) fn matches(entry: &str, query: &str) -> bool {
    query.split_whitespace().all(|w| find_ci(entry, w).is_some())
}

/// The byte ranges of `entry` to highlight for `query`: the first occurrence of each word,
/// sorted and merged.
pub(crate) fn highlights(entry: &str, query: &str) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = query
        .split_whitespace()
        .filter_map(|w| find_ci(entry, w).map(|at| at..at + w.len()))
        .collect();
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// The byte offset of `needle` in `hay`, ignoring ASCII case. Non-ASCII bytes compare exactly,
/// so a match starts and ends on char boundaries (UTF-8 is self-synchronizing).
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn h(entries: &[&str]) -> History {
        History::in_memory(entries.iter().map(|s| s.to_string()).collect())
    }

    /// A history file in a directory of its own, removed on drop.
    struct TempFile(PathBuf);
    impl TempFile {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tower-history-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            Self(dir.join("nested").join("history"))
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap().parent().unwrap());
        }
    }

    /// Commands survive the session: each is appended as it runs (the directory is created
    /// on first use), and the next session loads them, oldest first.
    #[test]
    fn commands_persist_across_sessions() {
        let f = TempFile::new();
        let mut a = History::from_file(f.0.clone());
        assert!(a.entries().is_empty(), "no file yet: empty");
        a.push("system resource print");
        a.push("  export  ");
        a.push("export"); // the same as the previous one: once
        a.push("");
        a.push("/bad\u{1b}[2J"); // not a line of text
        let b = History::from_file(f.0.clone());
        assert_eq!(b.entries(), ["system resource print", "export"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&f.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "owner-only");
        }
    }

    /// A file past the cap loads its newest MAX_ENTRIES and is rewritten to them.
    #[test]
    fn a_long_file_is_cut_to_the_newest() {
        let f = TempFile::new();
        std::fs::create_dir_all(f.0.parent().unwrap()).unwrap();
        let lines: Vec<String> = (0..MAX_ENTRIES + 5).map(|i| format!("/c{i}")).collect();
        std::fs::write(&f.0, lines.join("\n")).unwrap();
        let a = History::from_file(f.0.clone());
        assert_eq!(a.entries().len(), MAX_ENTRIES);
        assert_eq!(a.entries()[0], "/c5");
        let on_disk = std::fs::read_to_string(&f.0).unwrap();
        assert_eq!(on_disk.lines().count(), MAX_ENTRIES);
    }

    #[test]
    fn the_file_location() {
        let env = |vars: &'static [(&str, &str)]| {
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| PathBuf::from(v))
        };
        assert_eq!(
            default_file(Some("/tmp/h".into()), env(&[("HOME", "/home/u")])),
            Some(PathBuf::from("/tmp/h"))
        );
        assert_eq!(
            default_file(Some("".into()), env(&[("HOME", "/home/u")])),
            None,
            "off"
        );
        if cfg!(target_os = "macos") {
            assert_eq!(
                default_file(None, env(&[("HOME", "/Users/u")])),
                Some(PathBuf::from(
                    "/Users/u/Library/Application Support/tower/history"
                ))
            );
        } else if cfg!(unix) {
            assert_eq!(
                default_file(None, env(&[("HOME", "/home/u")])),
                Some(PathBuf::from("/home/u/.local/state/tower/history"))
            );
            assert_eq!(
                default_file(None, env(&[("HOME", "/home/u"), ("XDG_STATE_HOME", "/s")])),
                Some(PathBuf::from("/s/tower/history"))
            );
        }
        assert_eq!(default_file(None, env(&[])), None, "no home: in memory");
    }

    /// Up walks back from the newest, Down forward, and past the newest gives back the line
    /// that was being typed.
    #[test]
    fn up_and_down_browse_and_keep_the_draft() {
        let mut a = h(&["/a", "/b", "/c"]);
        assert_eq!(a.up("/dra"), Some("/c"));
        assert_eq!(a.up(""), Some("/b"));
        assert_eq!(a.up(""), Some("/a"));
        assert_eq!(a.up(""), None, "the oldest");
        assert_eq!(a.down().as_deref(), Some("/b"));
        assert_eq!(a.down().as_deref(), Some("/c"));
        assert_eq!(a.down().as_deref(), Some("/dra"), "the draft");
        assert_eq!(a.down(), None, "not browsing");
        assert_eq!(h(&[]).up("/x"), None);
        // Running a command starts the next browse from the newest again.
        let _ = a.up("");
        a.push("/d");
        assert_eq!(a.up(""), Some("/d"));
    }

    /// Ctrl-R: every word must appear (any order, any ASCII case), newest first, each distinct
    /// command once; Down steps back; a changed query restarts from the newest.
    #[test]
    fn search_finds_by_all_words_newest_first() {
        let hist = h(&[
            "system settings set therm-period=60",
            "system resource print",
            "system settings set accel=medium",
            "system resource print",
            "export",
        ]);
        let mut s = Search::new("/ty", 3);
        assert_eq!(s.hit(&hist), None, "no query, no hit");
        for c in "SET sys".chars() {
            s.push_char(&hist, c);
        }
        assert_eq!(s.hit(&hist), Some("system settings set accel=medium"));
        s.older(&hist);
        assert_eq!(s.hit(&hist), Some("system settings set therm-period=60"));
        s.older(&hist);
        assert_eq!(
            s.hit(&hist),
            Some("system settings set therm-period=60"),
            "the oldest stays"
        );
        s.newer();
        assert_eq!(s.hit(&hist), Some("system settings set accel=medium"));
        // `print` matches one distinct command twice: offered once.
        let mut p = Search::new("", 0);
        for c in "print".chars() {
            p.push_char(&hist, c);
        }
        assert_eq!(p.hit(&hist), Some("system resource print"));
        p.older(&hist);
        assert_eq!(p.hit(&hist), Some("system resource print"));
        assert!(p.trail.is_empty(), "no older distinct match");
        // Backspace re-searches; a query that matches nothing has no hit.
        p.push_char(&hist, 'z');
        assert_eq!(p.hit(&hist), None);
        p.pop_char(&hist);
        assert_eq!(p.hit(&hist), Some("system resource print"));
        assert_eq!((s.original.as_str(), s.original_cursor), ("/ty", 3));
    }

    /// The keys of a running search: typing and Ctrl-R / Up / Down move the match, Enter
    /// runs it, the cursor keys take it for editing, Esc / Ctrl-G give the old line back.
    #[test]
    fn search_keys() {
        let hist = h(&["/a one", "/b two"]);
        let mut s = Search::new("draft", 2);
        assert_eq!(s.key(&hist, KeyCode::Char('o'), false), SearchStep::Continue);
        assert_eq!(s.hit(&hist), Some("/b two"));
        assert_eq!(s.key(&hist, KeyCode::Char('r'), true), SearchStep::Continue);
        assert_eq!(s.hit(&hist), Some("/a one"));
        assert_eq!(s.key(&hist, KeyCode::Down, false), SearchStep::Continue);
        assert_eq!(s.hit(&hist), Some("/b two"));
        assert_eq!(s.key(&hist, KeyCode::Enter, false), SearchStep::Run);
        assert_eq!(
            s.key(&hist, KeyCode::Left, false),
            SearchStep::Edit(Some(KeyCode::Left))
        );
        assert_eq!(s.key(&hist, KeyCode::Tab, false), SearchStep::Edit(None));
        assert_eq!(s.key(&hist, KeyCode::Esc, false), SearchStep::Cancel);
        assert_eq!(s.key(&hist, KeyCode::Char('g'), true), SearchStep::Cancel);
        assert_eq!(s.cursor_col(), (SEARCH_LABEL.len() + 1) as u16);
        assert_eq!(s.finish(&hist), ("/b two".to_string(), 6));
        let none = Search::new("draft", 2);
        assert_eq!(
            none.finish(&hist),
            ("draft".to_string(), 2),
            "no match: the old line"
        );
    }

    #[test]
    fn highlights_cover_each_word() {
        let e = "system settings set accel=medium";
        assert_eq!(highlights(e, "SET acc"), vec![7..10, 20..23]);
        assert_eq!(highlights(e, "sett set"), vec![7..11], "overlaps merge");
        assert!(highlights(e, "zzz").is_empty());
        assert_eq!(find_ci("č/a", "/A"), Some(2), "boundaries stay on chars");
    }
}
