//! gh-style tables: two-space gutters, dim underlined header on a terminal,
//! `...` truncation that shrinks the widest flexible column first so rows fit
//! the terminal width; tab-separated with no header when piped.
//!
//! The width algorithm is a port of go-gh's `ttyTablePrinter`.

use super::color::Palette;
use super::text::{pad_right, truncate, width};

const GUTTER: usize = 2;

type ColorFn = fn(&Palette, &str) -> String;

#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    /// Applied after padding, only on a terminal.
    pub color: Option<ColorFn>,
    /// May be shortened with `...` to fit; identifiers set this to false.
    pub truncatable: bool,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: None,
            truncatable: true,
        }
    }

    pub fn colored(text: impl Into<String>, color: ColorFn) -> Self {
        Self {
            text: text.into(),
            color: Some(color),
            truncatable: true,
        }
    }

    /// Never truncated, like gh's issue numbers.
    pub fn fixed(text: impl Into<String>, color: ColorFn) -> Self {
        Self {
            text: text.into(),
            color: Some(color),
            truncatable: false,
        }
    }
}

pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<Cell>>,
    is_tty: bool,
    max_width: usize,
    palette: Palette,
}

impl Table {
    pub fn new(headers: &[&str], is_tty: bool, max_width: usize, palette: Palette) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
            is_tty,
            max_width,
            palette,
        }
    }

    pub fn add_row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    /// Render the table. On a terminal: header + aligned, truncated rows.
    /// Piped: one tab-separated line per row, no header, no truncation.
    pub fn render(&self) -> String {
        if !self.is_tty {
            return self
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .map(|l| format!("{l}\n"))
                .collect();
        }
        let mut all: Vec<Vec<(String, bool)>> = Vec::with_capacity(self.rows.len() + 1);
        if !self.headers.is_empty() {
            all.push(self.headers.iter().map(|h| (h.clone(), true)).collect());
        }
        for r in &self.rows {
            all.push(r.iter().map(|c| (c.text.clone(), c.truncatable)).collect());
        }
        if all.is_empty() {
            return String::new();
        }
        let widths = column_widths(&all, self.max_width);
        let ncols = widths.len();
        let mut out = String::new();
        if !self.headers.is_empty() {
            let line: Vec<String> = self
                .headers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    self.palette
                        .header(&pad_right(&truncate(widths[i], h), widths[i]))
                })
                .collect();
            out.push_str(&line.join(&" ".repeat(GUTTER)));
            out.push('\n');
        }
        for row in &self.rows {
            let mut parts = Vec::with_capacity(ncols);
            for (i, cell) in row.iter().enumerate().take(ncols) {
                let mut s = if cell.truncatable {
                    truncate(widths[i], &cell.text)
                } else {
                    cell.text.clone()
                };
                if i + 1 < ncols {
                    s = pad_right(&s, widths[i]);
                }
                if let Some(f) = cell.color {
                    s = f(&self.palette, &s);
                }
                parts.push(s);
            }
            out.push_str(&parts.join(&" ".repeat(GUTTER)));
            out.push('\n');
        }
        out
    }
}

/// Port of go-gh's `calculateColumnWidths`. `cells` are `(text, truncatable)`
/// pairs: a column containing a non-truncatable cell (the ID column) is
/// always wide enough for it; short columns keep their natural width; the
/// remaining space is split evenly between the long ("flex") columns, and
/// any remainder is handed out left to right.
pub fn column_widths(rows: &[Vec<(String, bool)>], max_width: usize) -> Vec<usize> {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if ncols == 0 {
        return Vec::new();
    }
    let mut max_col = vec![0usize; ncols];
    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, (text, truncatable)) in row.iter().enumerate() {
            let w = width(text);
            max_col[i] = max_col[i].max(w);
            if !truncatable && w > widths[i] {
                widths[i] = w;
            }
        }
    }
    let avail = |widths: &[usize]| -> isize {
        let set: usize = widths.iter().sum();
        max_width as isize - (GUTTER * (ncols - 1)) as isize - set as isize
    };
    let flex_count = |widths: &[usize]| ncols - widths.iter().filter(|w| **w > 0).count();

    // set the widths of short columns
    let w = avail(&widths);
    if w > 0 {
        let flex = flex_count(&widths);
        if flex > 0 {
            let per = w as usize / flex;
            for i in 0..ncols {
                if max_col[i] < per {
                    widths[i] = max_col[i];
                }
            }
        }
    }

    // truncate long columns to the remaining available width
    let flex = flex_count(&widths);
    if flex > 0 {
        let per = avail(&widths).max(0) as usize / flex;
        for i in 0..ncols {
            if widths[i] == 0 {
                if max_col[i] < per {
                    widths[i] = max_col[i];
                } else if per > 0 {
                    widths[i] = per;
                }
            }
        }
    }

    // add the remainder to truncated columns
    let mut w = avail(&widths);
    if w > 0 {
        for i in 0..ncols {
            let d = max_col[i].saturating_sub(widths[i]) as isize;
            let to_add = d.min(w);
            widths[i] += to_add as usize;
            w -= to_add;
            if w <= 0 {
                break;
            }
        }
    }

    // a column can never be narrower than one cell
    for w in widths.iter_mut() {
        if *w == 0 {
            *w = 1;
        }
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[&[&str]]) -> Vec<Vec<(String, bool)>> {
        data.iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .map(|(i, s)| (s.to_string(), i != 0))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn natural_widths_when_room() {
        let r = rows(&[
            &["ID", "TITLE", "UPDATED"],
            &["#1", "short title", "about 1 day ago"],
        ]);
        assert_eq!(column_widths(&r, 120), vec![2, 11, 15]);
    }

    #[test]
    fn matches_gh_at_sixty_columns() {
        // reference/gh/issue-list-narrow.txt: ID 6, TITLE 17, LABELS 15, UPDATED 16
        let long = "gh pr merge NNN --delete-branch fails to delete branch unless \"automatically delete head branches\" is toggled on";
        let r = rows(&[
            &["ID", "TITLE", "LABELS", "UPDATED"],
            &["#14404", long, "needs-triage", "about 9 hours ago"],
            &[
                "#14394",
                "`gh pr list` with flags fails JSON parsing",
                "bug, priority-3",
                "about 3 hours ago",
            ],
        ]);
        assert_eq!(column_widths(&r, 60), vec![6, 17, 15, 16]);
    }

    #[test]
    fn renders_tsv_when_piped() {
        let mut t = Table::new(&["ID", "TITLE"], false, 80, Palette::off());
        t.add_row(vec![Cell::plain("1"), Cell::plain("a\tb")]);
        assert_eq!(t.render(), "1\ta\tb\n");
    }

    #[test]
    fn renders_aligned_when_tty() {
        let mut t = Table::new(&["ID", "TITLE"], true, 80, Palette::off());
        t.add_row(vec![Cell::plain("1"), Cell::plain("first")]);
        t.add_row(vec![Cell::plain("22"), Cell::plain("second")]);
        assert_eq!(t.render(), "ID  TITLE \n1   first\n22  second\n");
    }

    #[test]
    fn truncates_rows_to_width() {
        let mut t = Table::new(&["ID", "TITLE", "UPDATED"], true, 40, Palette::off());
        t.add_row(vec![
            Cell::plain("#14404"),
            Cell::plain("gh pr merge NNN --delete-branch fails to delete"),
            Cell::plain("about 9 hours ago"),
        ]);
        for line in t.render().lines() {
            assert!(width(line) <= 40, "{line:?}");
        }
        assert!(t.render().contains("..."));
    }
}
