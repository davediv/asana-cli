//! ANSI styling that switches itself off when colour is disabled.
//!
//! The bytes are gh's, exactly as they appear in `reference/**/*.ansi.txt`:
//! gh paints each run with one combined SGR sequence and closes it with
//! `\e[0m`, so `bold` is `\e[0;1;39m` (reset, bold, default foreground),
//! `green` is `\e[0;32m`, the table header is `\e[0;2;4;37m` (dim, underline,
//! white), and the muted grey is `\e[38;5;242m`. The transcripts are compared
//! byte for byte, so every piece should paint through here rather than keep
//! a private copy.

/// gh's sequences, one per style. `Palette` wraps text in these and `RESET`.
pub mod seq {
    pub const RESET: &str = "\x1b[0m";
    /// Bold: reset, bold, default foreground.
    pub const BOLD: &str = "\x1b[0;1;39m";
    pub const DIM: &str = "\x1b[0;2;39m";
    pub const UNDERLINE: &str = "\x1b[0;4;39m";
    /// Table header: dim, underlined, white, in one sequence.
    pub const HEADER: &str = "\x1b[0;2;4;37m";
    /// Grey 242: timestamps, identifiers, footers.
    pub const MUTED: &str = "\x1b[38;5;242m";
    pub const GREEN: &str = "\x1b[0;32m";
    pub const RED: &str = "\x1b[0;31m";
    pub const YELLOW: &str = "\x1b[0;33m";
    pub const CYAN: &str = "\x1b[0;36m";
    pub const MAGENTA: &str = "\x1b[0;35m";
    /// Bold cyan, gh's "Newest comment" heading.
    pub const BOLD_CYAN: &str = "\x1b[0;1;36m";
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub enabled: bool,
}

impl Palette {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    #[cfg(test)]
    pub fn off() -> Self {
        Self { enabled: false }
    }

    /// Wrap `text` in `open` and gh's reset; unchanged when colour is off or
    /// the text is empty.
    pub fn paint(&self, open: &str, text: &str) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        format!("{open}{text}{}", seq::RESET)
    }

    /// `\e[0;1;39m`: help headings, titles, `Assignees:` labels.
    pub fn bold(&self, s: &str) -> String {
        self.paint(seq::BOLD, s)
    }

    #[allow(dead_code)]
    pub fn dim(&self, s: &str) -> String {
        self.paint(seq::DIM, s)
    }

    #[allow(dead_code)]
    pub fn underline(&self, s: &str) -> String {
        self.paint(seq::UNDERLINE, s)
    }

    /// `\e[0;2;4;37m`: the table header row.
    pub fn header(&self, s: &str) -> String {
        self.paint(seq::HEADER, s)
    }

    /// `\e[38;5;242m`: timestamps, identifiers and footers.
    pub fn muted(&self, s: &str) -> String {
        self.paint(seq::MUTED, s)
    }

    pub fn green(&self, s: &str) -> String {
        self.paint(seq::GREEN, s)
    }

    pub fn red(&self, s: &str) -> String {
        self.paint(seq::RED, s)
    }

    pub fn yellow(&self, s: &str) -> String {
        self.paint(seq::YELLOW, s)
    }

    pub fn cyan(&self, s: &str) -> String {
        self.paint(seq::CYAN, s)
    }

    pub fn magenta(&self, s: &str) -> String {
        self.paint(seq::MAGENTA, s)
    }

    /// `\e[0;1;36m`: bold cyan, used for "Newest comment".
    pub fn bold_cyan(&self, s: &str) -> String {
        self.paint(seq::BOLD_CYAN, s)
    }

    /// `✓` in green: created, completed, reopened, logged in.
    pub fn success_icon(&self) -> String {
        self.green("✓")
    }

    /// `✓` in red: gh paints the tick red when it closes something.
    #[allow(dead_code)]
    pub fn success_icon_red(&self) -> String {
        self.red("✓")
    }

    /// `!` in yellow: already in that state, a warning.
    pub fn warning_icon(&self) -> String {
        self.yellow("!")
    }

    /// `X` in red: a failure line.
    pub fn failure_icon(&self) -> String {
        self.red("X")
    }
}

/// Decide whether stdout gets colour: a terminal, `NO_COLOR` unset,
/// `TERM` not `dumb`, and `--no-color` absent.
pub fn should_color(is_tty: bool, no_color_flag: bool) -> bool {
    if no_color_flag || !is_tty {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_ops_when_off() {
        let p = Palette::off();
        assert_eq!(p.bold("x"), "x");
        assert_eq!(p.muted("x"), "x");
        assert_eq!(p.header("ID"), "ID");
        assert_eq!(p.success_icon(), "✓");
        assert_eq!(p.success_icon_red(), "✓");
        assert_eq!(p.warning_icon(), "!");
        assert_eq!(p.failure_icon(), "X");
        assert_eq!(Palette::new(true).bold(""), "");
    }

    /// The bytes gh emits, taken from reference/gh/*.ansi.txt.
    #[test]
    fn emits_gh_sequences() {
        let p = Palette::new(true);
        assert_eq!(p.bold("USAGE"), "\x1b[0;1;39mUSAGE\x1b[0m");
        assert_eq!(p.green("Open"), "\x1b[0;32mOpen\x1b[0m");
        assert_eq!(p.red("Closed"), "\x1b[0;31mClosed\x1b[0m");
        assert_eq!(p.yellow("!"), "\x1b[0;33m!\x1b[0m");
        assert_eq!(p.cyan("c"), "\x1b[0;36mc\x1b[0m");
        assert_eq!(p.magenta("m"), "\x1b[0;35mm\x1b[0m");
        assert_eq!(
            p.bold_cyan("Newest comment"),
            "\x1b[0;1;36mNewest comment\x1b[0m"
        );
        assert_eq!(
            p.muted("about 9 hours ago"),
            "\x1b[38;5;242mabout 9 hours ago\x1b[0m"
        );
        assert_eq!(p.header("ID  "), "\x1b[0;2;4;37mID  \x1b[0m");
        assert_eq!(p.success_icon(), "\x1b[0;32m✓\x1b[0m");
        assert_eq!(p.success_icon_red(), "\x1b[0;31m✓\x1b[0m");
        assert_eq!(p.warning_icon(), "\x1b[0;33m!\x1b[0m");
        assert_eq!(p.failure_icon(), "\x1b[0;31mX\x1b[0m");
    }
}
