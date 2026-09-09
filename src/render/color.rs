//! ANSI styling that switches itself off when colour is disabled.
//!
//! The sequences mirror gh's: bold `\e[0;1;39m`, muted grey 242, dim underlined
//! table headers, green/red/yellow/cyan/magenta accents.

use anstyle::{Ansi256Color, AnsiColor, Color, Style};

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

    fn paint(&self, style: Style, text: &str) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        format!("{}{}{}", style.render(), text, style.render_reset())
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint(Style::new().bold(), s)
    }

    #[allow(dead_code)]
    pub fn dim(&self, s: &str) -> String {
        self.paint(Style::new().dimmed(), s)
    }

    #[allow(dead_code)]
    pub fn underline(&self, s: &str) -> String {
        self.paint(Style::new().underline(), s)
    }

    /// Table header style: dim + underline, like gh.
    pub fn header(&self, s: &str) -> String {
        self.paint(
            Style::new()
                .dimmed()
                .underline()
                .fg_color(Some(Color::Ansi(AnsiColor::White))),
            s,
        )
    }

    /// Grey 242, used for timestamps, identifiers and footers.
    pub fn muted(&self, s: &str) -> String {
        self.paint(
            Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(242)))),
            s,
        )
    }

    pub fn green(&self, s: &str) -> String {
        self.paint(
            Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))),
            s,
        )
    }

    pub fn red(&self, s: &str) -> String {
        self.paint(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))), s)
    }

    pub fn yellow(&self, s: &str) -> String {
        self.paint(
            Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
            s,
        )
    }

    pub fn cyan(&self, s: &str) -> String {
        self.paint(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan))), s)
    }

    pub fn magenta(&self, s: &str) -> String {
        self.paint(
            Style::new().fg_color(Some(Color::Ansi(AnsiColor::Magenta))),
            s,
        )
    }

    /// Bold cyan, used for "Newest comment".
    pub fn bold_cyan(&self, s: &str) -> String {
        self.paint(
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Cyan))),
            s,
        )
    }

    /// ✓ in green.
    pub fn success_icon(&self) -> String {
        self.green("✓")
    }

    /// ! in yellow.
    pub fn warning_icon(&self) -> String {
        self.yellow("!")
    }

    /// X in red.
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
    }

    #[test]
    fn wraps_when_on() {
        let p = Palette::new(true);
        assert!(p.bold("x").starts_with("\x1b["));
        assert!(p.bold("x").ends_with("\x1b[0m"));
        assert!(p.muted("x").contains("38;5;242"));
    }
}
