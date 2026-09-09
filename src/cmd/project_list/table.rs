//! The terminal table for `project list`, byte-for-byte in gh's form.
//!
//! The width algorithm is the shared port of go-gh's `ttyTablePrinter`
//! (`render::table::column_widths`); what is local here is the styling. gh
//! paints each cell with one combined sequence (`\e[0;1;39m` bold,
//! `\e[0;2;4;37m` dim underlined header, `\e[38;5;242m` muted grey,
//! `\e[0;33m` yellow) closed by `\e[0m`, and the shared `Palette` emits the
//! equivalent as several sequences. Same look, different bytes; the reference
//! transcripts are compared in bytes, so this copy emits gh's.

use crate::render::table::column_widths;
use crate::render::text::{pad_right, truncate};

const GUTTER: &str = "  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    /// `\e[0;1;39m`: gh's bold, used for the NAME column.
    Bold,
    /// `\e[0;2;4;37m`: gh's dim underlined table header.
    Header,
    /// `\e[38;5;242m`: gh's muted grey, used for INFO and UPDATED.
    Muted,
    /// `\e[0;33m`: gh's yellow, used for INFO when the project is private.
    Yellow,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Plain => "",
            Style::Bold => "\x1b[0;1;39m",
            Style::Header => "\x1b[0;2;4;37m",
            Style::Muted => "\x1b[38;5;242m",
            Style::Yellow => "\x1b[0;33m",
        }
    }
}

/// Wrap `s` in the style's sequence and gh's `\e[0m` reset when colour is on.
pub fn paint(style: Style, s: &str, color: bool) -> String {
    if !color || style == Style::Plain {
        return s.to_string();
    }
    format!("{}{s}\x1b[0m", style.code())
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    pub style: Style,
}

impl Cell {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// A gh-style table for a terminal: dim underlined header (every column
/// padded), two-space gutters, rows padded in every column but the last,
/// `...` truncation that shrinks the widest column first so each line fits
/// `width`. Colour is applied after padding, as gh does.
pub struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<Cell>>,
    width: usize,
    color: bool,
}

impl Table {
    pub fn new(headers: &[&'static str], width: usize, color: bool) -> Self {
        Self {
            headers: headers.to_vec(),
            rows: Vec::new(),
            width,
            color,
        }
    }

    pub fn add_row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let mut all: Vec<Vec<(String, bool)>> = Vec::with_capacity(self.rows.len() + 1);
        all.push(self.headers.iter().map(|h| (h.to_string(), true)).collect());
        all.extend(
            self.rows
                .iter()
                .map(|r| r.iter().map(|c| (c.text.clone(), true)).collect()),
        );
        let widths = column_widths(&all, self.width);
        let ncols = widths.len();
        let mut out = String::new();

        let header: Vec<String> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let text = pad_right(&truncate(widths[i], h), widths[i]);
                paint(Style::Header, &text, self.color)
            })
            .collect();
        out.push_str(&header.join(GUTTER));
        out.push('\n');

        for row in &self.rows {
            let parts: Vec<String> = row
                .iter()
                .enumerate()
                .take(ncols)
                .map(|(i, cell)| {
                    let mut text = truncate(widths[i], &cell.text);
                    if i + 1 < ncols {
                        text = pad_right(&text, widths[i]);
                    }
                    paint(cell.style, &text, self.color)
                })
                .collect();
            out.push_str(&parts.join(GUTTER));
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows of reference/gh/repo-list.txt, pushed through our table at
    /// the same 120 columns, must come out as the very bytes gh printed.
    fn repo_list_table(color: bool) -> Table {
        let rows: [(&str, &str, &str, &str); 10] = [
            ("cli/go-gh", "A Go module for interacting with gh and the GitHub API from Go.", "public", "about 39 minutes ago"),
            ("cli/cli", "GitHub’s official command line tool", "public", "about 1 hour ago"),
            ("cli/browser", "Go helpers to open URLs, files, or readers in a web browser", "public, fork", "about 8 hours ago"),
            ("cli/oauth", "A library for performing OAuth Device flow and Web application flow in Go client apps.", "public", "about 2 days ago"),
            ("cli/go-internal", "Selected Go-internal packages factored out from the standard library", "public, fork", "about 7 days ago"),
            ("cli/gh-webhook", "", "public", "about 15 days ago"),
            ("cli/gh-extension-precompile", "Action for publishing binary GitHub CLI extensions", "public", "about 29 days ago"),
            ("cli/safeexec", "A safer version of exec.LookPath on Windows", "public", "about 2 years ago"),
            ("cli/shurcooL-graphql", "Package graphql provides a GraphQL client implementation.", "public, fork", "about 2 years ago"),
            ("cli/scoop-gh", "[DEPRECATED] scoop packaging for the github cli", "public, archived", "about 3 years ago"),
        ];
        let mut t = Table::new(&["NAME", "DESCRIPTION", "INFO", "UPDATED"], 120, color);
        for (name, desc, info, updated) in rows {
            t.add_row(vec![
                Cell::new(name, Style::Bold),
                Cell::new(desc, Style::Plain),
                Cell::new(info, Style::Muted),
                Cell::new(updated, Style::Muted),
            ]);
        }
        t
    }

    #[test]
    fn matches_repo_list_reference_plain() {
        let expected = "\
NAME                         DESCRIPTION                                          INFO              UPDATED             
cli/go-gh                    A Go module for interacting with gh and the GitH...  public            about 39 minutes ago
cli/cli                      GitHub’s official command line tool                  public            about 1 hour ago
cli/browser                  Go helpers to open URLs, files, or readers in a ...  public, fork      about 8 hours ago
cli/oauth                    A library for performing OAuth Device flow and W...  public            about 2 days ago
cli/go-internal              Selected Go-internal packages factored out from ...  public, fork      about 7 days ago
cli/gh-webhook                                                                    public            about 15 days ago
cli/gh-extension-precompile  Action for publishing binary GitHub CLI extensions   public            about 29 days ago
cli/safeexec                 A safer version of exec.LookPath on Windows          public            about 2 years ago
cli/shurcooL-graphql         Package graphql provides a GraphQL client implem...  public, fork      about 2 years ago
cli/scoop-gh                 [DEPRECATED] scoop packaging for the github cli      public, archived  about 3 years ago
";
        assert_eq!(repo_list_table(false).render(), expected);
    }

    #[test]
    fn matches_repo_list_reference_ansi() {
        let out = repo_list_table(true).render();
        let mut lines = out.lines();
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;2;4;37mNAME                       \x1b[0m  \x1b[0;2;4;37mDESCRIPTION                                        \x1b[0m  \x1b[0;2;4;37mINFO            \x1b[0m  \x1b[0;2;4;37mUPDATED             \x1b[0m"
        );
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;1;39mcli/go-gh                  \x1b[0m  A Go module for interacting with gh and the GitH...  \x1b[38;5;242mpublic          \x1b[0m  \x1b[38;5;242mabout 39 minutes ago\x1b[0m"
        );
        // the last column is not padded on data rows
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;1;39mcli/cli                    \x1b[0m  GitHub’s official command line tool                  \x1b[38;5;242mpublic          \x1b[0m  \x1b[38;5;242mabout 1 hour ago\x1b[0m"
        );
    }

    fn long_name_table(width: usize) -> Table {
        let mut t = Table::new(&["NAME", "DESCRIPTION", "INFO", "UPDATED"], width, false);
        t.add_row(vec![
            Cell::new(
                "Q4 marketing launch plan and campaign calendar for all regions and every channel we own",
                Style::Bold,
            ),
            Cell::new("Notes about the launch.", Style::Plain),
            Cell::new("public, at risk", Style::Muted),
            Cell::new("about 1 hour ago", Style::Muted),
        ]);
        t
    }

    #[test]
    fn long_name_gives_way_not_the_timestamp() {
        // 120 columns: 114 to share, the three short columns keep their
        // natural width (23 + 15 + 16) and NAME gets the remaining 60.
        let out = long_name_table(120).render();
        let row = out.lines().nth(1).unwrap();
        assert_eq!(
            row,
            "Q4 marketing launch plan and campaign calendar for all re...  Notes about the launch.  public, at risk  about 1 hour ago"
        );
        for line in out.lines() {
            assert!(crate::render::text::width(line) <= 120, "{line:?}");
        }
    }

    #[test]
    fn narrow_terminal_truncates_every_column_like_gh() {
        // reference/gh/issue-list-narrow.txt: at 60 columns even UPDATED
        // becomes `about 9 hours...`; the row still fits.
        let out = long_name_table(60).render();
        let row = out.lines().nth(1).unwrap();
        assert_eq!(
            row,
            "Q4 marketing...  Notes abou...  public, at...  about 1 ho..."
        );
        for line in out.lines() {
            assert!(crate::render::text::width(line) <= 60, "{line:?}");
        }
    }

    #[test]
    fn paint_is_a_no_op_without_colour() {
        assert_eq!(paint(Style::Bold, "x", false), "x");
        assert_eq!(paint(Style::Plain, "x", true), "x");
        assert_eq!(paint(Style::Yellow, "x", true), "\x1b[0;33mx\x1b[0m");
    }
}
