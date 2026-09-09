//! Render Asana rich text (`html_notes` on a task or project, `html_text` on
//! a story) or plain `notes` for the terminal the way gh renders markdown
//! through glamour: a two-space margin, paragraphs wrapped to the width,
//! `•` bullets and `1.` numbers, headings in bold, inline code as a chip,
//! links as `text url`, and every line optionally padded to one width.
//!
//! Asana's `html_notes` is a `<body>` with a small tag set (`strong`, `em`,
//! `u`, `s`, `code`, `a`, `ul`, `ol`, `li`, `h1`, `h2`, `blockquote`, `pre`,
//! `hr`, `table`, `img`) and *literal newlines* for line breaks, which is why
//! a generic HTML-to-text pass (which collapses whitespace) is not good
//! enough: it runs every line of a description into one paragraph. This
//! renderer keeps every newline, keeps link URLs, and keeps the indentation
//! of plain-text lines.
//!
//! Entry points: [`render_lines`] with a [`Layout`], and three shapes on top
//! of it: [`render`] (margin, joined), [`render_body`] (gh's issue-body
//! shape: wrapped to `width - 4`, margin 2, padded to `width - 2`), and
//! `text::render_notes` (no margin, no colour).

// Offered to the pieces; nothing shared calls it until they adopt it.
#![allow(dead_code)]

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::color::Palette;

/// How to lay the rendered text out.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Width the content (excluding the margin) is wrapped to.
    pub width: usize,
    /// Emit glamour's colour sequences.
    pub color: bool,
    /// Spaces before every non-blank line.
    pub margin: usize,
    /// Pad every non-blank line (margin included) to this many cells, the
    /// way glamour does; rules and blank lines are left alone.
    pub pad_to: Option<usize>,
}

/// Inline styling of one run of text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    code: bool,
    link_text: bool,
    link_url: bool,
    muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Inline {
    Text(String, Style),
    /// A hard line break (a literal newline in the source).
    Break,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Code(String),
    Quote(Vec<Block>),
    Rule,
    Table(Vec<Vec<Vec<Inline>>>),
}

// ---------------------------------------------------------------- parsing

#[derive(Debug)]
enum Token {
    Text(String),
    Open(String, Vec<(String, String)>),
    Close(String),
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
        let Some(end) = rest.find(';').filter(|e| *e <= 12) else {
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
            "nbsp" => Some('\u{a0}'),
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

fn parse_tag(raw: &str) -> Option<Token> {
    let inner = raw.trim();
    if inner.starts_with('!') || inner.starts_with('?') {
        return None;
    }
    if let Some(name) = inner.strip_prefix('/') {
        return Some(Token::Close(name.trim().to_ascii_lowercase()));
    }
    let inner = inner.trim_end_matches('/').trim();
    let mut parts = inner.splitn(2, |c: char| c.is_whitespace());
    let name = parts.next()?.to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut rest = parts.next().unwrap_or("").trim();
    while !rest.is_empty() {
        let eq = rest.find('=');
        let ws = rest.find(char::is_whitespace);
        match (eq, ws) {
            (Some(e), w) if w.is_none_or(|w| e < w) => {
                let key = rest[..e].trim().to_ascii_lowercase();
                let after = rest[e + 1..].trim_start();
                let (value, remainder) = match after.chars().next() {
                    Some(q @ ('"' | '\'')) => {
                        let body = &after[1..];
                        match body.find(q) {
                            Some(close) => (&body[..close], &body[close + 1..]),
                            None => (body, ""),
                        }
                    }
                    _ => match after.find(char::is_whitespace) {
                        Some(w) => (&after[..w], &after[w..]),
                        None => (after, ""),
                    },
                };
                attrs.push((key, decode_entities(value)));
                rest = remainder.trim_start();
            }
            (_, Some(w)) => {
                attrs.push((rest[..w].to_ascii_lowercase(), String::new()));
                rest = rest[w..].trim_start();
            }
            (_, None) => {
                attrs.push((rest.to_ascii_lowercase(), String::new()));
                rest = "";
            }
        }
    }
    Some(Token::Open(name, attrs))
}

fn tokenize(html: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find('<') {
            None => {
                tokens.push(Token::Text(decode_entities(rest)));
                break;
            }
            Some(pos) => {
                if pos > 0 {
                    tokens.push(Token::Text(decode_entities(&rest[..pos])));
                }
                rest = &rest[pos..];
                let Some(end) = rest.find('>') else {
                    tokens.push(Token::Text(decode_entities(rest)));
                    break;
                };
                if let Some(tok) = parse_tag(&rest[1..end]) {
                    tokens.push(tok);
                }
                rest = &rest[end + 1..];
            }
        }
    }
    tokens
}

/// Builds the block tree from the token stream.
struct Builder {
    /// Nested containers: the last one receives finished blocks.
    stack: Vec<Container>,
    /// The paragraph being accumulated.
    inline: Vec<Inline>,
    styles: Vec<Style>,
    /// Open `<a>` tags: `(href, is_asana_mention, text_start)`.
    links: Vec<(Option<String>, bool, usize)>,
    pre: Option<String>,
}

enum Container {
    Root(Vec<Block>),
    Item(Vec<Block>),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Heading(u8),
    Table(Vec<Vec<Vec<Inline>>>),
    Row(Vec<Vec<Inline>>),
    Cell,
}

impl Builder {
    fn new() -> Self {
        Self {
            stack: vec![Container::Root(Vec::new())],
            inline: Vec::new(),
            styles: vec![Style::default()],
            links: Vec::new(),
            pre: None,
        }
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap_or(&Style::default())
    }

    fn in_inline_context(&self) -> bool {
        !matches!(
            self.stack.last(),
            Some(Container::List { .. } | Container::Table(_) | Container::Row(_))
        )
    }

    fn push_text(&mut self, text: &str) {
        if let Some(pre) = self.pre.as_mut() {
            pre.push_str(text);
            return;
        }
        if !self.in_inline_context() {
            // Whitespace between <li>s or <tr>s is markup, not content.
            if text.trim().is_empty() {
                return;
            }
            self.open_item();
        }
        let style = self.style();
        let mut first = true;
        for line in text.replace('\r', "").split('\n') {
            if !first {
                self.inline.push(Inline::Break);
            }
            first = false;
            if !line.is_empty() {
                self.inline.push(Inline::Text(line.to_string(), style));
            }
        }
    }

    /// Finish the paragraph being accumulated (if any) into the current container.
    fn flush(&mut self) {
        let mut inline = std::mem::take(&mut self.inline);
        // One newline at a block edge is the break the block provides.
        if inline.first() == Some(&Inline::Break) {
            inline.remove(0);
        }
        if inline.last() == Some(&Inline::Break) {
            inline.pop();
        }
        let heading = matches!(self.stack.last(), Some(Container::Heading(_)));
        if inline.is_empty() && !heading {
            return;
        }
        match self.stack.last_mut() {
            Some(Container::Heading(level)) => {
                let level = *level;
                self.stack.pop();
                self.push_block(Block::Heading(level, inline));
            }
            Some(Container::Cell) => {
                self.stack.pop();
                if let Some(Container::Row(cells)) = self.stack.last_mut() {
                    cells.push(inline);
                }
            }
            _ => self.push_block(Block::Paragraph(inline)),
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Container::Root(blocks) | Container::Item(blocks) | Container::Quote(blocks)) => {
                blocks.push(block)
            }
            Some(Container::List { items, .. }) => items.push(vec![block]),
            Some(Container::Table(_) | Container::Row(_) | Container::Cell) => {
                // Block content inside a table cell: keep its text.
                if let Block::Paragraph(inline) | Block::Heading(_, inline) = block {
                    self.inline.extend(inline);
                }
            }
            Some(Container::Heading(_)) | None => {}
        }
    }

    fn open_item(&mut self) {
        match self.stack.last() {
            Some(Container::List { .. }) => self.stack.push(Container::Item(Vec::new())),
            Some(Container::Table(_)) => {
                self.stack.push(Container::Row(Vec::new()));
                self.stack.push(Container::Cell);
            }
            Some(Container::Row(_)) => self.stack.push(Container::Cell),
            _ => {}
        }
    }

    /// Close containers down to (and including) the nearest one `matches`.
    fn close_to(&mut self, matches: impl Fn(&Container) -> bool) {
        if !self.stack.iter().skip(1).any(&matches) {
            return;
        }
        loop {
            self.flush();
            let Some(top) = self.stack.pop() else { break };
            let done = matches(&top);
            match top {
                Container::Root(blocks) => {
                    self.stack.push(Container::Root(blocks));
                    break;
                }
                Container::Item(blocks) => {
                    if let Some(Container::List { items, .. }) = self.stack.last_mut() {
                        items.push(blocks);
                    }
                }
                Container::List { ordered, items } => {
                    self.push_block(Block::List { ordered, items });
                }
                Container::Quote(blocks) => self.push_block(Block::Quote(blocks)),
                Container::Heading(level) => self.push_block(Block::Heading(level, Vec::new())),
                Container::Table(rows) => self.push_block(Block::Table(rows)),
                Container::Row(cells) => {
                    if let Some(Container::Table(rows)) = self.stack.last_mut() {
                        rows.push(cells);
                    }
                }
                Container::Cell => {}
            }
            if done {
                break;
            }
        }
    }

    fn open(&mut self, name: &str, attrs: &[(String, String)]) {
        if self.pre.is_some() {
            if name == "br" {
                self.push_text("\n");
            }
            return;
        }
        let attr = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        match name {
            "body" | "html" | "span" | "div" | "p" => {
                if name == "p" || name == "div" {
                    self.flush();
                }
            }
            "br" => self.push_text("\n"),
            "strong" | "b" => self.push_style(|s| s.bold = true),
            "em" | "i" => self.push_style(|s| s.italic = true),
            "u" => self.push_style(|s| s.underline = true),
            "s" | "strike" | "del" => self.push_style(|s| s.strike = true),
            "code" => self.push_style(|s| s.code = true),
            "a" => {
                let mention = attrs.iter().any(|(k, _)| k == "data-asana-type");
                if !self.in_inline_context() {
                    self.open_item();
                }
                let start = self.inline.len();
                self.links.push((attr("href"), mention, start));
                self.push_style(|s| s.link_text = true);
            }
            "img" => {
                let alt = attr("alt").filter(|a| !a.trim().is_empty());
                let src = attr("src").filter(|a| !a.trim().is_empty());
                let mut label = format!("Image: {}", alt.as_deref().unwrap_or("attachment"));
                if src.is_some() {
                    label.push_str(" →");
                }
                let style = Style {
                    muted: true,
                    ..Style::default()
                };
                self.inline.push(Inline::Text(label, style));
                if let Some(src) = src {
                    self.inline
                        .push(Inline::Text(" ".to_string(), Style::default()));
                    self.inline.push(Inline::Text(
                        src,
                        Style {
                            link_url: true,
                            ..Style::default()
                        },
                    ));
                }
            }
            "ul" | "ol" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                self.stack.push(Container::List {
                    ordered: name == "ol",
                    items: Vec::new(),
                });
            }
            "li" => {
                // A stray <li> closes the previous item.
                if matches!(self.stack.last(), Some(Container::Item(_))) {
                    self.close_to(|c| matches!(c, Container::Item(_)));
                } else {
                    self.flush();
                }
                if matches!(self.stack.last(), Some(Container::List { .. })) {
                    self.stack.push(Container::Item(Vec::new()));
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                let level = name[1..].parse::<u8>().unwrap_or(1);
                self.stack.push(Container::Heading(level));
            }
            "blockquote" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                self.stack.push(Container::Quote(Vec::new()));
            }
            "pre" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                self.pre = Some(String::new());
            }
            "hr" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                self.push_block(Block::Rule);
            }
            "table" => {
                self.flush();
                if !self.in_inline_context() {
                    self.open_item();
                }
                self.stack.push(Container::Table(Vec::new()));
            }
            "tr" => {
                if matches!(self.stack.last(), Some(Container::Cell | Container::Row(_))) {
                    self.close_to(|c| matches!(c, Container::Row(_)));
                }
                if matches!(self.stack.last(), Some(Container::Table(_))) {
                    self.stack.push(Container::Row(Vec::new()));
                }
            }
            "td" | "th" => {
                if matches!(self.stack.last(), Some(Container::Cell)) {
                    self.flush();
                }
                if matches!(self.stack.last(), Some(Container::Table(_))) {
                    self.stack.push(Container::Row(Vec::new()));
                }
                if matches!(self.stack.last(), Some(Container::Row(_))) {
                    self.stack.push(Container::Cell);
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if self.pre.is_some() {
            if name == "pre" {
                let code = self.pre.take().unwrap_or_default();
                let code = code.trim_matches('\n').to_string();
                self.push_block(Block::Code(code));
            }
            return;
        }
        match name {
            "strong" | "b" | "em" | "i" | "u" | "s" | "strike" | "del" | "code" => {
                self.pop_style();
            }
            "a" => {
                self.pop_style();
                if let Some((href, mention, start)) = self.links.pop() {
                    self.finish_link(href, mention, start);
                }
            }
            "p" | "div" => self.flush(),
            "ul" | "ol" => self.close_to(|c| matches!(c, Container::List { .. })),
            "li" => self.close_to(|c| matches!(c, Container::Item(_))),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if matches!(self.stack.last(), Some(Container::Heading(_))) {
                    self.flush();
                }
            }
            "blockquote" => self.close_to(|c| matches!(c, Container::Quote(_))),
            "table" => self.close_to(|c| matches!(c, Container::Table(_))),
            "tr" => self.close_to(|c| matches!(c, Container::Row(_))),
            "td" | "th" => {
                if matches!(self.stack.last(), Some(Container::Cell)) {
                    self.flush();
                }
            }
            _ => {}
        }
    }

    fn push_style(&mut self, change: impl Fn(&mut Style)) {
        let mut s = self.style();
        change(&mut s);
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    /// Append the URL after a link's text (`text url`), unless the text is
    /// the URL itself or the link is an Asana @-mention.
    fn finish_link(&mut self, href: Option<String>, mention: bool, start: usize) {
        let Some(href) = href.filter(|h| !h.trim().is_empty()) else {
            return;
        };
        let text: String = self.inline[start.min(self.inline.len())..]
            .iter()
            .filter_map(|i| match i {
                Inline::Text(t, _) => Some(t.as_str()),
                Inline::Break => None,
            })
            .collect();
        let text = text.trim();
        let same = text == href
            || text == href.trim_end_matches('/')
            || text
                == href
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
            || format!("{text}/") == href;
        if same {
            // Show it as a URL, not as link text.
            for item in self.inline.iter_mut().skip(start) {
                if let Inline::Text(_, style) = item {
                    style.link_text = false;
                    style.link_url = true;
                }
            }
            return;
        }
        if mention {
            return;
        }
        if text.is_empty() {
            self.inline.truncate(start);
        } else {
            self.inline
                .push(Inline::Text(" ".to_string(), Style::default()));
        }
        self.inline.push(Inline::Text(
            href,
            Style {
                link_url: true,
                ..Style::default()
            },
        ));
    }

    fn finish(mut self) -> Vec<Block> {
        if let Some(code) = self.pre.take() {
            self.push_block(Block::Code(code.trim_matches('\n').to_string()));
        }
        self.close_to(|_| false);
        self.flush();
        match self.stack.pop() {
            Some(Container::Root(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

fn parse_html(html: &str) -> Vec<Block> {
    let mut b = Builder::new();
    for token in tokenize(html) {
        match token {
            Token::Text(t) => b.push_text(&t),
            Token::Open(name, attrs) => b.open(&name, &attrs),
            Token::Close(name) => b.close(&name),
        }
    }
    b.finish()
}

fn parse_plain(text: &str) -> Vec<Block> {
    let mut b = Builder::new();
    b.push_text(text);
    b.finish()
}

// --------------------------------------------------------------- layout

/// Glamour's dark theme, as gh shows it.
#[derive(Debug, Clone, Copy)]
struct Theme {
    color: bool,
}

const RESET: &str = "\x1b[0m";

impl Theme {
    fn seq(&self, style: Style) -> Option<String> {
        if !self.color {
            return None;
        }
        let mut codes: Vec<String> = Vec::new();
        if style.code {
            codes.push("38;5;203;48;5;236".into());
        } else if style.link_url {
            codes.push("38;5;30;4".into());
        } else if style.link_text {
            codes.push("38;5;35;1".into());
        } else if style.muted {
            codes.push("38;5;243".into());
        } else {
            codes.push("38;5;252".into());
        }
        if style.bold && !style.link_text {
            codes.push("1".into());
        }
        if style.italic {
            codes.push("3".into());
        }
        if style.underline && !style.link_url {
            codes.push("4".into());
        }
        if style.strike {
            codes.push("9".into());
        }
        Some(format!("\x1b[{}m", codes.join(";")))
    }

    fn paint(&self, text: &str, style: Style) -> String {
        if text.is_empty() {
            return String::new();
        }
        match self.seq(style) {
            Some(seq) => format!("{seq}{text}{RESET}"),
            None => text.to_string(),
        }
    }

    fn paint_raw(&self, text: &str, codes: &str) -> String {
        if !self.color || text.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{codes}m{text}{RESET}")
    }
}

/// One unbreakable piece of a word with its style.
#[derive(Debug, Clone)]
struct Frag {
    text: String,
    style: Style,
}

fn frag_width(f: &Frag) -> usize {
    UnicodeWidthStr::width(f.text.as_str())
}

/// Split a line of inline content into words (each a run of fragments with
/// no whitespace between them).
fn words(line: &[&Inline], theme: &Theme) -> Vec<Vec<Frag>> {
    let mut out: Vec<Vec<Frag>> = Vec::new();
    let mut open = false; // the last word can still grow
    for item in line {
        let Inline::Text(text, style) = item else {
            continue;
        };
        if style.code {
            let chip = if theme.color {
                format!(" {} ", text.trim())
            } else {
                format!("`{}`", text.trim())
            };
            let frag = Frag {
                text: chip,
                style: *style,
            };
            match out.last_mut() {
                Some(word) if open => word.push(frag),
                _ => out.push(vec![frag]),
            }
            open = true;
            continue;
        }
        let starts_ws = text.starts_with(char::is_whitespace);
        let ends_ws = text.ends_with(char::is_whitespace);
        let mut first = true;
        for piece in text.split_whitespace() {
            let frag = Frag {
                text: piece.to_string(),
                style: *style,
            };
            match out.last_mut() {
                Some(word) if first && open && !starts_ws => word.push(frag),
                _ => out.push(vec![frag]),
            }
            first = false;
        }
        if text.trim().is_empty() {
            open = open && !starts_ws && !ends_ws && text.is_empty();
        } else {
            open = !ends_ws;
        }
    }
    out
}

fn word_width(word: &[Frag]) -> usize {
    word.iter().map(frag_width).sum()
}

/// Cut a word that does not fit into pieces no wider than `max`.
fn split_word(word: &[Frag], max: usize) -> Vec<Vec<Frag>> {
    let max = max.max(1);
    let mut pieces: Vec<Vec<Frag>> = vec![Vec::new()];
    let mut used = 0;
    for frag in word {
        let mut cur = String::new();
        for ch in frag.text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > max && used > 0 {
                if !cur.is_empty() {
                    pieces.last_mut().unwrap().push(Frag {
                        text: std::mem::take(&mut cur),
                        style: frag.style,
                    });
                }
                pieces.push(Vec::new());
                used = 0;
            }
            cur.push(ch);
            used += w;
        }
        if !cur.is_empty() {
            pieces.last_mut().unwrap().push(Frag {
                text: cur,
                style: frag.style,
            });
        }
    }
    pieces.retain(|p| p.iter().any(|f| !f.text.trim().is_empty()));
    pieces
}

/// Paint a line of words: adjacent fragments with the same style become one
/// run, and the space between two words takes their style when it is shared,
/// so `the docs site` is one green run rather than three.
fn render_words(words: &[Vec<Frag>], theme: &Theme) -> String {
    let mut runs: Vec<Frag> = Vec::new();
    let mut push = |text: &str, style: Style| {
        if text.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => runs.push(Frag {
                text: text.to_string(),
                style,
            }),
        }
    };
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            let prev = words[i - 1].last().map(|f| f.style);
            let next = word.first().map(|f| f.style);
            let style = match (prev, next) {
                (Some(a), Some(b)) if a == b && !a.code => a,
                _ => Style::default(),
            };
            push(" ", style);
        }
        for frag in word {
            push(&frag.text, frag.style);
        }
    }
    runs.iter().map(|f| theme.paint(&f.text, f.style)).collect()
}

/// Wrap one hard line of inline content to `width` cells.
fn wrap_line(line: &[&Inline], width: usize, theme: &Theme) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur: Vec<Vec<Frag>> = Vec::new();
    let mut cur_w = 0usize;
    let flush = |cur: &mut Vec<Vec<Frag>>, lines: &mut Vec<String>| {
        if cur.is_empty() {
            return;
        }
        lines.push(render_words(cur, theme));
        cur.clear();
    };
    for word in words(line, theme) {
        let w = word_width(&word);
        if w > width {
            flush(&mut cur, &mut lines);
            cur_w = 0;
            let pieces = split_word(&word, width);
            let n = pieces.len();
            for (i, piece) in pieces.into_iter().enumerate() {
                if i + 1 == n {
                    cur_w = word_width(&piece);
                    cur.push(piece);
                } else {
                    lines.push(render_words(std::slice::from_ref(&piece), theme));
                }
            }
            continue;
        }
        let needed = if cur.is_empty() { w } else { cur_w + 1 + w };
        if needed > width && !cur.is_empty() {
            flush(&mut cur, &mut lines);
            cur_w = w;
            cur.push(word);
        } else {
            cur_w = needed;
            cur.push(word);
        }
    }
    flush(&mut cur, &mut lines);
    lines
}

/// Leading spaces of a hard line are indentation (a code sample typed into
/// the notes): keep them, on the first wrapped line and its continuations.
fn indentation(segment: &[Inline]) -> usize {
    let Some(Inline::Text(first, style)) = segment.first() else {
        return 0;
    };
    if style.code {
        return 0;
    }
    let lead: usize = first
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t' || *c == '\u{a0}')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    lead.min(16)
}

/// Wrap inline content (with hard breaks) into lines.
fn wrap_inline(inline: &[Inline], width: usize, theme: &Theme) -> Vec<String> {
    let mut lines = Vec::new();
    for segment in inline.split(|i| *i == Inline::Break) {
        let lead = indentation(segment);
        let refs: Vec<&Inline> = segment.iter().collect();
        let wrapped = wrap_line(&refs, width.saturating_sub(lead).max(1), theme);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            let pad = " ".repeat(lead);
            lines.extend(wrapped.into_iter().map(|l| format!("{pad}{l}")));
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn plain_inline(inline: &[Inline]) -> String {
    inline
        .iter()
        .map(|i| match i {
            Inline::Text(t, _) => t.as_str(),
            Inline::Break => " ",
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_blocks(blocks: &[Block], width: usize, theme: &Theme, out: &mut Vec<String>) {
    let width = width.max(10);
    for block in blocks {
        match block {
            Block::Paragraph(inline) => out.extend(wrap_inline(inline, width, theme)),
            Block::Heading(level, inline) => {
                let text = plain_inline(inline);
                let line = match level {
                    1 => theme.paint_raw(&format!(" {text} "), "38;5;228;48;5;63;1"),
                    _ => theme.paint_raw(
                        &format!("{} {text}", "#".repeat((*level).into())),
                        "38;5;39;1",
                    ),
                };
                out.push(line);
            }
            Block::List { ordered, items } => {
                for (i, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", i + 1)
                    } else {
                        "• ".to_string()
                    };
                    let marker_w = UnicodeWidthStr::width(marker.as_str());
                    let mut inner = Vec::new();
                    render_blocks(item, width.saturating_sub(marker_w), theme, &mut inner);
                    if inner.is_empty() {
                        inner.push(String::new());
                    }
                    let pad = " ".repeat(marker_w);
                    for (j, line) in inner.into_iter().enumerate() {
                        if j == 0 {
                            out.push(format!("{}{line}", theme.paint(&marker, Style::default())));
                        } else if line.is_empty() {
                            out.push(String::new());
                        } else {
                            out.push(format!("{pad}{line}"));
                        }
                    }
                }
            }
            Block::Code(code) => {
                for line in code.lines() {
                    out.push(format!("  {}", theme.paint_raw(line, "38;5;251")));
                }
            }
            Block::Quote(blocks) => {
                let mut inner = Vec::new();
                render_blocks(blocks, width.saturating_sub(2), theme, &mut inner);
                for line in inner {
                    out.push(format!("{}{line}", theme.paint("│ ", Style::default())));
                }
            }
            Block::Rule => out.push(theme.paint_raw("--------", "38;5;240")),
            Block::Table(rows) => {
                let cells: Vec<Vec<String>> = rows
                    .iter()
                    .map(|r| r.iter().map(|c| plain_inline(c)).collect())
                    .collect();
                let cols = cells.iter().map(Vec::len).max().unwrap_or(0);
                let mut widths = vec![0usize; cols];
                for row in &cells {
                    for (i, c) in row.iter().enumerate() {
                        widths[i] = widths[i].max(UnicodeWidthStr::width(c.as_str()));
                    }
                }
                for row in &cells {
                    let mut parts = Vec::new();
                    for (i, c) in row.iter().enumerate() {
                        let padded = if i + 1 == row.len() {
                            c.clone()
                        } else {
                            format!(
                                "{c}{}",
                                " ".repeat(widths[i] - UnicodeWidthStr::width(c.as_str()))
                            )
                        };
                        parts.push(theme.paint(&padded, Style::default()));
                    }
                    out.push(parts.join("  "));
                }
            }
        }
    }
}

/// Display width of a line, ignoring SGR escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Render notes into lines: `html` when it holds anything visible, else
/// plain `text`. Blank lines stay empty; other lines carry the margin and,
/// with `pad_to`, trailing spaces up to that width. Empty when there is
/// nothing to show.
pub fn render_lines(html: Option<&str>, text: Option<&str>, layout: &Layout) -> Vec<String> {
    let theme = Theme {
        color: layout.color,
    };
    let mut blocks = match html.map(str::trim).filter(|h| !h.is_empty()) {
        Some(html) => parse_html(html),
        None => Vec::new(),
    };
    if blocks.is_empty() {
        // No HTML, or HTML that held nothing visible (`<body></body>`).
        // Surrounding whitespace goes; indentation inside is kept.
        blocks = match text.map(str::trim).filter(|t| !t.is_empty()) {
            Some(text) => parse_plain(text),
            None => return Vec::new(),
        };
    }
    let mut lines = Vec::new();
    render_blocks(&blocks, layout.width, &theme, &mut lines);
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let margin = " ".repeat(layout.margin);
    lines
        .into_iter()
        .map(|l| {
            if l.trim().is_empty() {
                return String::new();
            }
            let mut line = format!("{margin}{l}");
            if let Some(pad_to) = layout.pad_to {
                let fill = pad_to.saturating_sub(visible_width(&line));
                line.push_str(&" ".repeat(fill));
            }
            line
        })
        .collect()
}

/// Notes with a two-space margin, wrapped so no line exceeds `width`, joined
/// with newlines. Empty when there is nothing to show.
pub fn render(html_notes: Option<&str>, notes: Option<&str>, width: usize, color: bool) -> String {
    render_lines(
        html_notes,
        notes,
        &Layout {
            width: width.saturating_sub(2),
            color,
            margin: 2,
            pad_to: None,
        },
    )
    .join("\n")
}

/// gh's issue-body shape: wrapped to `term_width - 4`, a two-space margin,
/// and every non-blank line padded to `term_width - 2`, as glamour does.
pub fn render_body(
    html: Option<&str>,
    text: Option<&str>,
    term_width: usize,
    palette: &Palette,
) -> Vec<String> {
    let content = term_width.saturating_sub(4).max(20);
    render_lines(
        html,
        text,
        &Layout {
            width: content,
            color: palette.enabled,
            margin: 2,
            pad_to: Some(content + 2),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(html: &str, width: usize) -> String {
        render(Some(html), None, width, false)
    }

    #[test]
    fn keeps_asana_line_breaks() {
        let html = "<body>Roadmap for <strong>Q4</strong>.\nSecond line.\n\nGoals:\n<ul><li>Ship <code>asana task list</code></li>\n<li>Docs at <a href=\"https://example.com/docs\">the docs site</a></li></ul>\nDone.</body>";
        let out = plain(html, 80);
        assert_eq!(
            out,
            "  Roadmap for Q4.\n  Second line.\n\n  Goals:\n  • Ship `asana task list`\n  • Docs at the docs site https://example.com/docs\n  Done."
        );
    }

    #[test]
    fn keeps_paragraphs_and_numbered_steps() {
        let html = "<body>Intro line.\n\nSteps:\n1. First step\n2. Second step\n\nDone.</body>";
        let out = plain(html, 80);
        assert_eq!(
            out,
            "  Intro line.\n\n  Steps:\n  1. First step\n  2. Second step\n\n  Done."
        );
    }

    #[test]
    fn wraps_to_width_with_margin() {
        let html = "<body>one two three four five six seven eight nine ten</body>";
        let out = plain(html, 22);
        for line in out.lines() {
            assert!(UnicodeWidthStr::width(line) <= 22, "{line:?}");
            assert!(line.starts_with("  "));
        }
        assert_eq!(
            out,
            "  one two three four\n  five six seven eight\n  nine ten"
        );
    }

    #[test]
    fn long_words_are_cut() {
        let html =
            "<body>see https://example.com/a/very/long/path/that/never/ends/at/all ok</body>";
        let out = plain(html, 30);
        for line in out.lines() {
            assert!(UnicodeWidthStr::width(line) <= 30, "{line:?}");
        }
        assert!(out.contains("https://example.com/a/very/l"));
    }

    #[test]
    fn urls_as_link_text_are_not_doubled() {
        let html =
            "<body>See <a href=\"https://example.com/x\">https://example.com/x</a> now</body>";
        assert_eq!(plain(html, 80), "  See https://example.com/x now");
        let mention = "<body>Ask <a data-asana-gid=\"1\" data-asana-type=\"user\" href=\"https://app.asana.com/0/1\">Div</a> today</body>";
        assert_eq!(plain(mention, 80), "  Ask Div today");
    }

    #[test]
    fn headings_lists_quotes_and_code() {
        let html = "<body><h1>Title</h1>\n<h2>Sub</h2>\nText\n<ol><li>first</li><li>second<ul><li>nested</li></ul></li></ol>\n<blockquote>quoted\nline</blockquote>\n<pre>let x = 1;\n  y</pre>\n<hr>\nend</body>";
        let out = plain(html, 80);
        assert_eq!(
            out,
            "   Title \n  ## Sub\n  Text\n  1. first\n  2. second\n     • nested\n  │ quoted\n  │ line\n    let x = 1;\n      y\n  --------\n  end"
        );
    }

    #[test]
    fn entities_and_plain_fallback() {
        assert_eq!(
            plain("<body>a &amp; b &lt;c&gt; &#39;d&#39;</body>", 80),
            "  a & b <c> 'd'"
        );
        assert_eq!(
            render(None, Some("hello\nworld\n\nagain"), 80, false),
            "  hello\n  world\n\n  again"
        );
        assert_eq!(render(None, None, 80, false), "");
        assert_eq!(render(Some("<body></body>"), Some("  "), 80, false), "");
        assert_eq!(render(Some("<body>\n\n</body>"), None, 80, false), "");
        assert_eq!(
            render(Some("<body></body>"), Some("fallback"), 80, false),
            "  fallback"
        );
        assert_eq!(render(None, Some("a < b && c"), 80, false), "  a < b && c");
    }

    #[test]
    fn tables_align() {
        let html = "<body><table><tr><td>Name</td><td>Owner</td></tr><tr><td>Roadmap</td><td>Div</td></tr></table></body>";
        assert_eq!(plain(html, 80), "  Name     Owner\n  Roadmap  Div");
    }

    #[test]
    fn colour_uses_glamour_sequences() {
        let html = "<body>use <code>gh</code> and <strong>bold</strong> <a href=\"https://x.y\">link</a></body>";
        let out = render(Some(html), None, 80, true);
        assert!(
            out.contains("\x1b[38;5;203;48;5;236m gh \x1b[0m"),
            "{out:?}"
        );
        assert!(out.contains("\x1b[38;5;252;1mbold\x1b[0m"), "{out:?}");
        assert!(out.contains("\x1b[38;5;35;1mlink\x1b[0m"), "{out:?}");
        assert!(out.contains("\x1b[38;5;30;4mhttps://x.y\x1b[0m"), "{out:?}");
    }

    #[test]
    fn styled_fragments_stay_one_word() {
        let html = "<body>un<strong>break</strong>able word</body>";
        assert_eq!(plain(html, 80), "  unbreakable word");
        let out = render(Some(html), None, 80, true);
        assert_eq!(
            out,
            "  \x1b[38;5;252mun\x1b[0m\x1b[38;5;252;1mbreak\x1b[0m\x1b[38;5;252mable word\x1b[0m"
        );
    }

    #[test]
    fn runs_share_style_across_spaces() {
        let html = "<body>see <a href=\"https://x.y\">the docs site</a> now</body>";
        let out = render(Some(html), None, 80, true);
        assert_eq!(
            out,
            "  \x1b[38;5;252msee \x1b[0m\x1b[38;5;35;1mthe docs site\x1b[0m\x1b[38;5;252m \x1b[0m\x1b[38;5;30;4mhttps://x.y\x1b[0m\x1b[38;5;252m now\x1b[0m"
        );
    }

    #[test]
    fn list_continuations_hang_under_the_text() {
        let html = "<body><ul><li>alpha beta gamma delta epsilon zeta eta theta</li></ul></body>";
        let out = render(Some(html), None, 30, false);
        assert_eq!(
            out,
            "  • alpha beta gamma delta\n    epsilon zeta eta theta"
        );
        for line in out.lines() {
            assert!(UnicodeWidthStr::width(line) <= 30, "{line:?}");
        }
    }

    #[test]
    fn images_and_breaks() {
        let html = "<body>Look:<br><img alt=\"diagram\" src=\"https://img/1.png\"></body>";
        assert_eq!(
            plain(html, 80),
            "  Look:\n  Image: diagram → https://img/1.png"
        );
    }

    #[test]
    fn keeps_indentation_and_collapses_runs_of_spaces() {
        let text = "code:\n    indented line\nback   to   normal";
        assert_eq!(
            render(None, Some(text), 80, false),
            "  code:\n      indented line\n  back to normal"
        );
        let out = render(None, Some("x:\n    one two three four"), 16, false);
        assert_eq!(out, "  x:\n      one two\n      three four");
    }

    #[test]
    fn newline_after_a_block_is_a_separator_not_a_blank_line() {
        let html = "<body>Intro:\n<ul>\n<li>a</li>\n<li>b</li>\n</ul>\nAfter\n\n<h1>H</h1>\n\nPara\r\nnext</body>";
        assert_eq!(
            plain(html, 80),
            "  Intro:\n  • a\n  • b\n  After\n\n   H \n\n  Para\n  next"
        );
    }

    #[test]
    fn body_shape_pads_to_width_minus_two() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let out = render_body(None, Some(text), 30, &Palette::new(false));
        assert!(out
            .iter()
            .all(|l| l.is_empty() || UnicodeWidthStr::width(l.as_str()) == 28));
        assert_eq!(out[0].trim_end(), "  one two three four five");
        assert_eq!(out[1].trim_end(), "  six seven eight nine ten");
        assert_eq!(out[2].trim_end(), "  eleven twelve");
        let on = render_body(
            Some("<body>a <strong>b</strong></body>"),
            None,
            30,
            &Palette::new(true),
        );
        assert_eq!(visible_width(&on[0]), 28);
        assert!(render_body(Some("<body></body>"), None, 80, &Palette::new(false)).is_empty());
        assert!(render_body(None, Some("  \n"), 80, &Palette::new(false)).is_empty());
    }

    #[test]
    fn no_margin_shape_for_piped_output() {
        let layout = Layout {
            width: 80,
            color: false,
            margin: 0,
            pad_to: None,
        };
        let out = render_lines(
            Some("<body>Line one\nLine two <a href=\"https://x.y/z\">docs</a></body>"),
            None,
            &layout,
        );
        assert_eq!(out, vec!["Line one", "Line two docs https://x.y/z"]);
    }

    #[test]
    fn visible_width_ignores_escapes() {
        assert_eq!(visible_width("\x1b[38;5;252mabc\x1b[0m d"), 5);
        assert_eq!(visible_width("日本"), 4);
    }
}
