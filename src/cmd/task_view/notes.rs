//! Render Asana rich text (`html_notes` on a task, `html_text` on a story) or
//! plain `notes` for the terminal the way gh renders an issue body: wrapped
//! to the terminal width, a two-space margin, every line padded to the same
//! width, inline styles for bold / code / links / headings.
//!
//! Asana's HTML is a small fixed set of tags (`body`, `strong`, `em`, `u`,
//! `s`, `code`, `a`, `ul`, `ol`, `li`, `h1`, `h2`, `blockquote`, `hr`,
//! `img`, `table`) and line breaks are literal newlines, which is why the
//! shared html2text-based `render::text::render_notes` flattens paragraphs.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::Palette;

/// Inline formatting of one run of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    code: bool,
    link: bool,
    heading: bool,
}

impl Style {
    fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// The SGR parameters for this style, in glamour's dark-theme colours.
    fn sgr(&self) -> String {
        if self.code {
            return "38;5;203;48;5;236".to_string();
        }
        if self.link {
            return "38;5;30;4".to_string();
        }
        if self.heading {
            return "38;5;39;1".to_string();
        }
        let mut parts: Vec<&str> = Vec::new();
        if self.bold {
            parts.push("1");
        }
        if self.italic {
            parts.push("3");
        }
        if self.underline {
            parts.push("4");
        }
        if self.strike {
            parts.push("9");
        }
        parts.join(";")
    }
}

#[derive(Debug, Clone)]
struct Frag {
    style: Style,
    text: String,
}

/// One logical line before wrapping: a list item, a paragraph line, a rule.
#[derive(Debug, Clone, Default)]
struct Block {
    /// Printed before the first wrapped line (`• `, `3. `, `│ `, indentation).
    prefix: String,
    /// Printed before every continuation line (spaces, or the quote bar).
    hang: String,
    frags: Vec<Frag>,
    rule: bool,
}

impl Block {
    fn is_empty(&self) -> bool {
        !self.rule && self.frags.iter().all(|f| f.text.trim().is_empty())
    }
}

#[derive(Debug, Clone, Copy)]
enum List {
    Bullet,
    Number(u32),
}

/// Builds blocks from text and tags.
struct Builder {
    blocks: Vec<Block>,
    cur: Block,
    styles: Vec<Style>,
    lists: Vec<List>,
    quote: usize,
    /// Pad code spans with a space on each side, like glamour (colour only).
    pad_code: bool,
    /// A block-level tag just closed: the newline Asana puts after it is a
    /// separator, not a blank line.
    after_block: bool,
}

/// Placeholder for the padding space inside a code chip; it must not be a
/// word separator, and it becomes a real space when the line is rendered.
const CODE_PAD: char = '\u{a0}';

impl Builder {
    fn new(pad_code: bool) -> Self {
        Self {
            blocks: Vec::new(),
            cur: Block::default(),
            styles: vec![Style::default()],
            lists: Vec::new(),
            quote: 0,
            pad_code,
            after_block: false,
        }
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap_or(&Style::default())
    }

    fn push_style(&mut self, f: impl FnOnce(&mut Style)) {
        let mut s = self.style();
        f(&mut s);
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn context_prefix(&self) -> (String, String) {
        let bars = "│ ".repeat(self.quote);
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        (format!("{bars}{indent}"), format!("{bars}{indent}"))
    }

    /// Finish the current line unconditionally (a newline or `<br>`); an
    /// empty block becomes a blank line.
    fn end_line(&mut self) {
        let (prefix, hang) = self.context_prefix();
        let done = std::mem::replace(
            &mut self.cur,
            Block {
                prefix: prefix.clone(),
                hang,
                ..Block::default()
            },
        );
        // A list item that wraps onto a new line keeps its hanging indent.
        if !done.hang.is_empty() && done.hang != done.prefix {
            self.cur.prefix = done.hang.clone();
            self.cur.hang = done.hang.clone();
        }
        self.blocks.push(done);
    }

    /// Finish the current line only if something is on it (structural tags).
    fn end_line_if_used(&mut self) {
        if !self.cur.is_empty() {
            self.end_line();
        } else {
            let (prefix, hang) = self.context_prefix();
            self.cur = Block {
                prefix,
                hang,
                ..Block::default()
            };
        }
    }

    fn text(&mut self, raw: &str) {
        let raw = raw.replace('\r', "");
        for (i, piece) in raw.split('\n').enumerate() {
            if i > 0 {
                let separator = i == 1 && self.after_block && self.cur.is_empty();
                if !separator {
                    self.end_line();
                }
                self.after_block = false;
            } else if !piece.trim().is_empty() {
                self.after_block = false;
            }
            if piece.trim().is_empty() {
                // Whitespace between tags still separates words.
                if !self.cur.is_empty() {
                    self.cur.frags.push(Frag {
                        style: self.style(),
                        text: " ".to_string(),
                    });
                }
                continue;
            }
            // Leading spaces on a fresh line are indentation: keep them.
            if self.cur.is_empty() && self.cur.frags.is_empty() {
                let lead = piece.len() - piece.trim_start_matches([' ', '\t']).len();
                if lead > 0 {
                    let spaces = " ".repeat(lead.min(16));
                    self.cur.prefix.push_str(&spaces);
                    self.cur.hang.push_str(&spaces);
                }
            }
            self.cur.frags.push(Frag {
                style: self.style(),
                text: piece.to_string(),
            });
        }
    }

    fn open(&mut self, name: &str) {
        match name {
            "strong" | "b" => self.push_style(|s| s.bold = true),
            "em" | "i" => self.push_style(|s| s.italic = true),
            "u" => self.push_style(|s| s.underline = true),
            "s" | "strike" | "del" => self.push_style(|s| s.strike = true),
            "code" | "pre" => {
                self.push_style(|s| s.code = true);
                if self.pad_code {
                    self.cur.frags.push(Frag {
                        style: self.style(),
                        text: CODE_PAD.to_string(),
                    });
                }
            }
            "a" => self.push_style(|s| s.link = true),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.end_line_if_used();
                self.push_style(|s| s.heading = true);
            }
            "ul" => {
                self.end_line_if_used();
                self.lists.push(List::Bullet);
                self.after_block = true;
            }
            "ol" => {
                self.end_line_if_used();
                self.lists.push(List::Number(0));
                self.after_block = true;
            }
            "li" => {
                self.end_line_if_used();
                self.after_block = false;
                let marker = match self.lists.last_mut() {
                    Some(List::Number(n)) => {
                        *n += 1;
                        format!("{n}. ")
                    }
                    _ => "• ".to_string(),
                };
                self.cur.prefix.push_str(&marker);
                self.cur
                    .hang
                    .push_str(&" ".repeat(UnicodeWidthStr::width(marker.as_str())));
            }
            "blockquote" => {
                self.end_line_if_used();
                self.quote += 1;
                let (prefix, hang) = self.context_prefix();
                self.cur.prefix = prefix;
                self.cur.hang = hang;
            }
            "br" => self.end_line(),
            "hr" => {
                self.end_line_if_used();
                self.cur.rule = true;
                self.end_line();
                self.after_block = true;
            }
            "p" | "div" | "table" | "tr" => self.end_line_if_used(),
            "td" | "th" => {
                if !self.cur.is_empty() {
                    self.cur.frags.push(Frag {
                        style: Style::default(),
                        text: "  ".to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "strong" | "b" | "em" | "i" | "u" | "s" | "strike" | "del" | "a" => self.pop_style(),
            "code" | "pre" => {
                if self.pad_code {
                    self.cur.frags.push(Frag {
                        style: self.style(),
                        text: CODE_PAD.to_string(),
                    });
                }
                self.pop_style();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.pop_style();
                self.end_line_if_used();
                self.after_block = true;
            }
            "ul" | "ol" => {
                self.end_line_if_used();
                self.lists.pop();
                let (prefix, hang) = self.context_prefix();
                self.cur.prefix = prefix;
                self.cur.hang = hang;
                self.after_block = true;
            }
            "li" => {
                self.end_line_if_used();
                self.after_block = true;
            }
            "blockquote" => {
                self.end_line_if_used();
                self.quote = self.quote.saturating_sub(1);
                let (prefix, hang) = self.context_prefix();
                self.cur.prefix = prefix;
                self.cur.hang = hang;
                self.after_block = true;
            }
            "p" | "div" | "table" | "tr" => {
                self.end_line_if_used();
                self.after_block = true;
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.end_line_if_used();
        let mut blocks = self.blocks;
        while blocks.first().is_some_and(Block::is_empty) {
            blocks.remove(0);
        }
        while blocks.last().is_some_and(Block::is_empty) {
            blocks.pop();
        }
        blocks
    }
}

/// Decode the entities Asana emits (`&amp;`, `&lt;`, `&gt;`, `&quot;`,
/// `&#39;`, `&nbsp;`, numeric references).
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Walk Asana HTML, feeding text and tags to the builder.
fn parse_html(html: &str, b: &mut Builder) {
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            b.text(&decode_entities(rest));
            break;
        };
        if lt > 0 {
            b.text(&decode_entities(&rest[..lt]));
        }
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            b.text(&decode_entities(&rest[lt..]));
            break;
        };
        let inner = after[..gt].trim();
        rest = &after[gt + 1..];
        if inner.starts_with('!') {
            continue;
        }
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_end_matches('/').trim();
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if closing {
            b.close(&name);
        } else {
            b.open(&name);
        }
    }
}

/// A wrapped word: fragments with their styles, no spaces.
type Word = Vec<(Style, String)>;

fn word_width(w: &[(Style, String)]) -> usize {
    w.iter()
        .map(|(_, t)| UnicodeWidthStr::width(t.as_str()))
        .sum()
}

/// Split a block's fragments into words on ASCII whitespace; a word may span
/// several styles (`foo<b>bar</b>`).
fn words(frags: &[Frag]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    let mut cur: Word = Vec::new();
    for frag in frags {
        for (i, piece) in frag.text.split([' ', '\t']).enumerate() {
            if i > 0 && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if !piece.is_empty() {
                cur.push((frag.style, piece.to_string()));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Take the first `cells` columns of a word (for words wider than a line).
fn split_word(word: &[(Style, String)], cells: usize) -> (Word, Word) {
    let mut head: Word = Vec::new();
    let mut tail: Word = Vec::new();
    let mut used = 0;
    for (style, text) in word {
        if !tail.is_empty() {
            tail.push((*style, text.clone()));
            continue;
        }
        let mut h = String::new();
        let mut t = String::new();
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if t.is_empty() && used + w <= cells {
                used += w;
                h.push(ch);
            } else {
                t.push(ch);
            }
        }
        if !h.is_empty() {
            head.push((*style, h));
        }
        if !t.is_empty() {
            tail.push((*style, t));
        }
    }
    (head, tail)
}

/// Greedy word wrap of one block into lines of at most `width` cells,
/// including the prefix.
fn wrap_block(block: &Block, width: usize) -> Vec<(String, Vec<Word>)> {
    let mut lines: Vec<(String, Vec<Word>)> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    let mut cur_width = 0usize;
    let mut prefix = block.prefix.clone();
    let avail_for = |p: &str| width.saturating_sub(UnicodeWidthStr::width(p)).max(1);
    let mut avail = avail_for(&prefix);
    let mut queue: std::collections::VecDeque<Word> = words(&block.frags).into();
    while let Some(word) = queue.pop_front() {
        let w = word_width(&word);
        if cur.is_empty() {
            if w > avail {
                let (head, tail) = split_word(&word, avail);
                if head.is_empty() {
                    // avail is smaller than one wide character: emit it anyway.
                    cur.push(word);
                } else {
                    cur.push(head);
                    if !tail.is_empty() {
                        queue.push_front(tail);
                    }
                }
                lines.push((prefix.clone(), std::mem::take(&mut cur)));
                prefix = block.hang.clone();
                avail = avail_for(&prefix);
                cur_width = 0;
                continue;
            }
            cur_width = w;
            cur.push(word);
        } else if cur_width + 1 + w <= avail {
            cur_width += 1 + w;
            cur.push(word);
        } else {
            lines.push((prefix.clone(), std::mem::take(&mut cur)));
            prefix = block.hang.clone();
            avail = avail_for(&prefix);
            cur_width = 0;
            queue.push_front(word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push((prefix, cur));
    }
    lines
}

/// Paint one wrapped line; returns the text and its visible width.
fn paint_line(prefix: &str, line: &[Word], p: &Palette) -> (String, usize) {
    let mut out = String::new();
    let mut visible = 0usize;
    // The prefix (bullets, quote bars, indentation) is muted like glamour's.
    let bar = prefix.trim_end();
    if !bar.is_empty() && bar.chars().all(|c| c == '│' || c == ' ') {
        out.push_str(&p.muted(bar));
        out.push_str(&prefix[bar.len()..]);
    } else {
        out.push_str(prefix);
    }
    visible += UnicodeWidthStr::width(prefix);

    // Flatten words into styled runs with the separating spaces, giving a
    // space the style of its neighbours when they agree (code chips).
    let mut runs: Vec<(Style, String)> = Vec::new();
    for (i, word) in line.iter().enumerate() {
        if i > 0 {
            let prev = runs.last().map(|r| r.0).unwrap_or_default();
            let next = word.first().map(|f| f.0).unwrap_or_default();
            let style = if prev == next && prev.code {
                prev
            } else {
                Style::default()
            };
            runs.push((style, " ".to_string()));
        }
        for (style, text) in word {
            runs.push((*style, text.clone()));
        }
    }
    let mut merged: Vec<(Style, String)> = Vec::new();
    for (style, text) in runs {
        match merged.last_mut() {
            Some((s, t)) if *s == style => t.push_str(&text),
            _ => merged.push((style, text)),
        }
    }
    for (style, text) in merged {
        let text = text.replace(CODE_PAD, " ");
        visible += UnicodeWidthStr::width(text.as_str());
        if style.is_plain() || !p.enabled {
            out.push_str(&text);
        } else {
            out.push_str(&format!("\x1b[{}m{text}\x1b[0m", style.sgr()));
        }
    }
    (out, visible)
}

/// Render rich text (`html`), else plain `text`, into lines ready to print:
/// wrapped to `term_width - 4`, indented two spaces, padded to
/// `term_width - 2` like gh's markdown output. Empty when there is nothing.
pub fn render_body(
    html: Option<&str>,
    text: Option<&str>,
    term_width: usize,
    p: &Palette,
) -> Vec<String> {
    let content = term_width.saturating_sub(4).max(20);
    let pad_to = content + 2;
    let mut b = Builder::new(p.enabled);
    let html = html.map(str::trim).filter(|h| !h.is_empty());
    match html {
        Some(h) => parse_html(h, &mut b),
        None => b.text(text.unwrap_or("").trim_matches('\n')),
    }
    let mut blocks = b.finish();
    if blocks.is_empty() {
        // `html_notes` was there but held nothing visible (`<body></body>`).
        if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
            let mut b = Builder::new(p.enabled);
            b.text(t);
            blocks = b.finish();
        }
    }
    let mut out = Vec::new();
    for block in &blocks {
        if block.rule {
            let rule = "─".repeat(content);
            out.push(format!("  {}", p.muted(&rule)));
            continue;
        }
        if block.is_empty() {
            out.push(String::new());
            continue;
        }
        for (prefix, line) in wrap_block(block, content) {
            let (painted, visible) = paint_line(&prefix, &line, p);
            let fill = pad_to.saturating_sub(visible + 2);
            out.push(format!("  {painted}{}", " ".repeat(fill)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| l.trim_end().to_string()).collect()
    }

    #[test]
    fn keeps_paragraphs_and_numbered_steps() {
        let html = "<body>Intro line.\n\nSteps:\n1. First step\n2. Second step\n\nDone.</body>";
        let out = plain(&render_body(Some(html), None, 80, &Palette::new(false)));
        assert_eq!(
            out,
            vec![
                "  Intro line.",
                "",
                "  Steps:",
                "  1. First step",
                "  2. Second step",
                "",
                "  Done.",
            ]
        );
    }

    #[test]
    fn wraps_and_pads_to_width_minus_two() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let out = render_body(None, Some(text), 30, &Palette::new(false));
        assert!(out
            .iter()
            .all(|l| l.is_empty() || UnicodeWidthStr::width(l.as_str()) == 28));
        assert_eq!(out[0].trim_end(), "  one two three four five");
        assert_eq!(out[1].trim_end(), "  six seven eight nine ten");
        assert_eq!(out[2].trim_end(), "  eleven twelve");
    }

    #[test]
    fn lists_headings_quotes_and_rules() {
        let html = "<body><h1>Title</h1>text\n<ul><li>one</li><li>two is a rather long item that needs to wrap around</li></ul><ol><li>first</li><li>second</li></ol><blockquote>quoted</blockquote><hr>end</body>";
        let out = plain(&render_body(Some(html), None, 40, &Palette::new(false)));
        assert_eq!(
            out,
            vec![
                "  Title",
                "  text",
                "  • one",
                "  • two is a rather long item that",
                "    needs to wrap around",
                "  1. first",
                "  2. second",
                "  │ quoted",
                &format!("  {}", "─".repeat(36)),
                "  end",
            ]
        );
    }

    #[test]
    fn newline_after_a_block_is_a_separator_not_a_blank_line() {
        let html = "<body>Intro:\n<ul>\n<li>a</li>\n<li>b</li>\n</ul>\nAfter\n\n<h1>H</h1>\n\nPara\r\nnext</body>";
        let out = plain(&render_body(Some(html), None, 80, &Palette::new(false)));
        assert_eq!(
            out,
            vec!["  Intro:", "  • a", "  • b", "  After", "", "  H", "", "  Para", "  next"]
        );
    }

    #[test]
    fn styles_only_with_colour() {
        let html = "<body>run <code>gh auth logout</code> and <strong>bold</strong> <a href=\"https://x.y\">https://x.y</a></body>";
        let off = render_body(Some(html), None, 80, &Palette::new(false));
        assert_eq!(
            off[0].trim_end(),
            "  run gh auth logout and bold https://x.y"
        );
        let on = render_body(Some(html), None, 80, &Palette::new(true));
        assert!(on[0].contains("\x1b[38;5;203;48;5;236m gh auth logout \x1b[0m"));
        assert!(on[0].contains("\x1b[1mbold\x1b[0m"));
        assert!(on[0].contains("\x1b[38;5;30;4mhttps://x.y\x1b[0m"));
        assert_eq!(UnicodeWidthStr::width(strip(&on[0]).as_str()), 78);
    }

    #[test]
    fn falls_back_to_plain_and_handles_empty() {
        assert!(render_body(Some("<body></body>"), None, 80, &Palette::new(false)).is_empty());
        assert!(render_body(None, Some("  \n"), 80, &Palette::new(false)).is_empty());
        let out = render_body(
            Some("<body></body>"),
            Some("fallback"),
            80,
            &Palette::new(false),
        );
        assert_eq!(out[0].trim_end(), "  fallback");
        let out = render_body(None, Some("a < b && c"), 80, &Palette::new(false));
        assert_eq!(out[0].trim_end(), "  a < b && c");
    }

    #[test]
    fn decodes_entities_and_breaks_long_words() {
        let html = "<body>Tom &amp; Jerry &lt;3 &#39;quoted&#39; &#x41;</body>";
        let out = render_body(Some(html), None, 80, &Palette::new(false));
        assert_eq!(out[0].trim_end(), "  Tom & Jerry <3 'quoted' A");
        let long = "x".repeat(50);
        let out = render_body(None, Some(&long), 30, &Palette::new(false));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].trim_end(), format!("  {}", "x".repeat(26)));
        assert_eq!(out[1].trim_end(), format!("  {}", "x".repeat(24)));
    }

    #[test]
    fn keeps_indentation_and_collapses_runs_of_spaces() {
        let text = "code:\n    indented line\nback   to   normal";
        let out = plain(&render_body(None, Some(text), 80, &Palette::new(false)));
        assert_eq!(
            out,
            vec!["  code:", "      indented line", "  back to normal"]
        );
    }

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for d in chars.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
