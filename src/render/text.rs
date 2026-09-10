//! Display-width aware truncation, indentation, prose wrapping, and the
//! plain shape of Asana notes (`render_notes`; the full renderer is
//! `render::notes`).
//!
//! Wrapping follows one rule everywhere: greedy at ASCII spaces, no
//! hyphenation, never inside a word, so a URL wider than the limit stays
//! whole on a line of its own; a `code span` in backticks counts as one
//! word, so `asana help formatting` never breaks at the `help`.
//! [`wrap_prose`] lays out running text (a help
//! description, a multi-line message): every line keeps the indent it
//! started with, a `- ` bullet's continuation hangs under its text, blank
//! lines survive. [`wrap_sentences`] is the variant error reports use: a
//! wide line is broken between sentences first, so `what happened. what to
//! do` becomes two whole lines rather than a line and an orphan. Both are
//! built on [`wrap_line`], and both stay inside [`PROSE_WIDTH`] on an
//! 80-column terminal through [`prose_width`].

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The widest a line of our own prose may be: under 80 columns, so an
/// 80-column terminal never breaks a sentence mid-word or a URL mid-path.
/// Help prose is hand-wrapped short of this (`cmd::help::PROSE_WIDTH`, 78
/// with the indent); the wrappers here break anything wider at run time.
pub const PROSE_WIDTH: usize = 79;

/// The terminal's width in columns: the size of stdout, stderr or stdin
/// (whichever is a terminal, so `asana --help | less` still sees the screen
/// it is read on), else `COLUMNS`, else 80.
pub fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .filter(|w| *w > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|c| c.trim().parse::<usize>().ok())
                .filter(|w| *w > 0)
        })
        .unwrap_or(80)
}

/// The width to wrap prose to: the terminal's, capped at [`PROSE_WIDTH`].
#[allow(dead_code)]
pub fn prose_width() -> usize {
    terminal_width().min(PROSE_WIDTH)
}

/// Display width in terminal cells.
pub fn width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

/// The columns one grapheme cluster occupies in a terminal.
///
/// `unicode-width` measures scalar by scalar, which is wrong for the two
/// shapes that turn up in real names. A cluster joined by U+200D, like the
/// judge emoji, is drawn as one glyph but measures as the sum of its parts;
/// and a character followed by U+FE0F, like the gear, is drawn as an emoji
/// two cells wide but measures as the narrow text form. Both are counted
/// here as a single wide cell, which is what a terminal draws and therefore
/// what a table has to pad against.
fn grapheme_width(g: &str) -> usize {
    const VARIATION_SELECTOR_16: char = '\u{fe0f}';
    const ZERO_WIDTH_JOINER: char = '\u{200d}';
    if g.contains(VARIATION_SELECTOR_16) || g.contains(ZERO_WIDTH_JOINER) {
        return 2;
    }
    match UnicodeWidthStr::width(g) {
        // A lone combining mark still belongs to whatever it follows.
        0 if g.chars().count() > 1 => 1,
        w => w,
    }
}

/// Truncate to `max` cells with a `...` suffix, like gh's `text.Truncate`.
pub fn truncate(max: usize, s: &str) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max < 5 {
        return take_width(s, max);
    }
    format!("{}...", take_width(s, max - 3))
}

/// The longest prefix that fits in `max` cells.
fn take_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    // Whole clusters only: half of a joined emoji is not a character.
    for g in s.graphemes(true) {
        let w = grapheme_width(g);
        if used + w > max {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out
}

/// Pad on the right with spaces to `w` cells.
pub fn pad_right(s: &str, w: usize) -> String {
    let cur = width(s);
    if cur >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - cur))
    }
}

/// Prefix every line with `n` spaces (blank lines stay blank).
#[allow(dead_code)]
pub fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Break one logical line into lines no wider than `limit`, the first
/// prefixed with `first` and the rest with `rest` (both count towards the
/// limit). Greedy, at ASCII spaces only, like a terminal would if it broke
/// at words: no hyphenation, and a word wider than the limit (a URL) stays
/// whole on a line of its own. A span in backticks (`asana help
/// formatting`) is one word: it moves whole to the next line rather than
/// breaking inside. A line that already fits comes back as it is, spacing
/// and all; no line ends in whitespace.
pub fn wrap_line(text: &str, limit: usize, first: &str, rest: &str) -> Vec<String> {
    let text = text.trim_end();
    if text.is_empty() {
        return vec![first.trim_end().to_string()];
    }
    if width(first) + width(text) <= limit {
        return vec![format!("{first}{text}")];
    }
    // The spaces inside a code span are made non-breaking while textwrap
    // lays the line out, then put back.
    let glued = glue_code_spans(text);
    let opts = textwrap::Options::new(limit)
        .initial_indent(first)
        .subsequent_indent(rest)
        .break_words(false)
        .word_separator(textwrap::WordSeparator::AsciiSpace)
        .word_splitter(textwrap::WordSplitter::NoHyphenation)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
    textwrap::wrap(&glued, &opts)
        .into_iter()
        .map(|l| l.replace(GLUE, " "))
        .collect()
}

/// The character that stands in for a space that must not break: U+00A0,
/// one cell wide like the space it replaces, so widths are unchanged.
pub const GLUE: char = '\u{a0}';

/// `text` with the spaces inside each pair of backticks replaced by
/// [`GLUE`]. A backtick without a partner (or a span with a newline in it)
/// is left alone.
pub fn glue_code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) if !after[..close].contains('\n') => {
                out.push_str(&rest[..=open]);
                out.push_str(&after[..close].replace(' ', &GLUE.to_string()));
                out.push('`');
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Lay out running text for `width` columns. Each line of `text` is a
/// paragraph of its own: one that fits is left exactly as written; a wider
/// one is broken at spaces by [`wrap_line`], its continuation lines keeping
/// the indent it started with, and a `- ` or `* ` bullet's continuation
/// hanging under the bullet's text. Blank lines stay blank (never
/// indent-only), trailing spaces are dropped, and a trailing newline is
/// kept.
pub fn wrap_prose(text: &str, width: usize) -> String {
    layout(text, width, |body, room, hang| {
        wrap_line(body, room, "", hang)
    })
}

/// [`wrap_prose`] for error reports: a line wider than `width` is broken at
/// its strongest punctuation first, between sentences and `;`-joined
/// clauses, so `what happened. what to do` becomes two whole lines rather
/// than a line and an orphan; then after a `:`, so a message keeps its
/// label (`invalid argument "..." for "--limit" flag:` over `number is too
/// large`); and only then at spaces, never inside a word. The pieces are
/// packed back onto lines greedily, so nothing is broken that did not have
/// to be. Indents, bullets and blank lines are handled as `wrap_prose`
/// handles them.
pub fn wrap_sentences(text: &str, width: usize) -> String {
    layout(text, width, |body, room, hang| {
        break_at(body, room, "", hang, 0)
    })
}

/// The paragraph loop shared by the wrappers: `break_body` lays out one
/// over-wide line's body (its indent stripped) inside `room` columns, with
/// `hang` as the prefix of its continuation lines; the indent goes back on
/// every line here.
fn layout(
    text: &str,
    limit: usize,
    break_body: impl Fn(&str, usize, &str) -> Vec<String>,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let body = line.trim_start();
        let lead = &line[..line.len() - body.len()];
        if width(lead) + width(body) <= limit {
            out.push(line.to_string());
            continue;
        }
        let hang = if body.starts_with("- ") || body.starts_with("* ") {
            "  "
        } else {
            ""
        };
        let room = limit.saturating_sub(width(lead));
        for piece in break_body(body, room, hang) {
            out.push(format!("{lead}{piece}"));
        }
    }
    if text.ends_with('\n') {
        out.push(String::new());
    }
    out.join("\n")
}

/// The marks a wide line is broken after, strongest first: each mark is
/// kept on the line it ends.
const BREAKS: [&[u8]; 2] = [b".;", b":"];

/// `text` broken inside `limit` at the marks of `level` (then the weaker
/// ones, then spaces), the first line prefixed with `first` and the rest
/// with `rest`.
fn break_at(text: &str, limit: usize, first: &str, rest: &str, level: usize) -> Vec<String> {
    let Some(marks) = BREAKS.get(level) else {
        return wrap_line(text, limit, first, rest);
    };
    let pieces = split_after(text, marks);
    if pieces.len() < 2 {
        return break_at(text, limit, first, rest, level + 1);
    }
    let mut packed: Vec<String> = Vec::new();
    for piece in pieces {
        match packed.last_mut() {
            Some(last) if width(last) + 1 + width(piece) <= limit => {
                last.push(' ');
                last.push_str(piece);
            }
            _ => {
                let prefix = if packed.is_empty() { first } else { rest };
                packed.push(format!("{prefix}{piece}"));
            }
        }
    }
    packed
        .into_iter()
        .enumerate()
        .flat_map(|(i, line)| {
            if width(&line) <= limit {
                return vec![line];
            }
            let prefix = if i == 0 { first } else { rest };
            break_at(&line[prefix.len()..], limit, prefix, rest, level + 1)
        })
        .collect()
}

/// `text` split after each of `marks` that is followed by a space (the
/// mark stays with its piece; `app.asana.com` has no space after its dots
/// and is never split). Pieces are trimmed and empty ones dropped.
fn split_after<'a>(text: &'a str, marks: &[u8]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if i >= start && marks.contains(&bytes[i]) && bytes[i + 1] == b' ' {
            out.push(text[start..=i].trim());
            start = i + 2;
        }
    }
    let rest = text[start.min(text.len())..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out.retain(|s| !s.is_empty());
    out
}

/// `<https://...>` written as a bare URL: the angle brackets are markdown's
/// autolink syntax and mean nothing on a terminal.
pub fn unbracket_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("<http") {
        let after = &rest[i + 1..];
        match after.find('>') {
            Some(j)
                if (after.starts_with("http://") || after.starts_with("https://"))
                    && !after[..j].contains(char::is_whitespace) =>
            {
                out.push_str(&rest[..i]);
                out.push_str(&after[..j]);
                rest = &after[j + 1..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render Asana notes for a terminal: prefer `html_notes`, else plain
/// `notes`; no margin, no colour, wrapped to `w`. Literal newlines survive,
/// link URLs are kept (`text url`), lists get `•` and `1.` markers. See
/// `render::notes` for the coloured and margined shapes. Returns an empty
/// string when there is nothing.
#[allow(dead_code)]
pub fn render_notes(html_notes: Option<&str>, notes: Option<&str>, w: usize) -> String {
    let layout = super::notes::Layout {
        width: w.max(20),
        color: false,
        margin: 0,
        pad_to: None,
    };
    super::notes::render_lines(html_notes, notes, &layout).join("\n")
}

/// gh shows URLs without the scheme in "Opening ... in your browser."
pub fn display_url(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
}

#[cfg(test)]
mod tests {
    /// A terminal draws one glyph per grapheme cluster, so the table pads
    /// against clusters, not scalars: the gear carries a presentation
    /// selector and the judge is three scalars joined by ZWJ, yet each
    /// occupies one wide cell.
    #[test]
    fn emoji_clusters_measure_one_wide_cell() {
        use super::width;
        assert_eq!(width("PR"), 2);
        assert_eq!(width("\u{2699}\u{fe0f} UTL"), 2 + 1 + 3);
        assert_eq!(width("\u{1f47e} FUN"), 2 + 1 + 3);
        assert_eq!(width("\u{1f468}\u{200d}\u{2696}\u{fe0f} LEGAL"), 2 + 1 + 5);
        assert_eq!(width("caf\u{e9}"), 4);
        assert_eq!(width("cafe\u{301}"), 4);
    }

    /// Truncation never splits a cluster in half.
    #[test]
    fn truncation_keeps_clusters_whole() {
        use super::truncate;
        let judge = "\u{1f468}\u{200d}\u{2696}\u{fe0f} LEGAL";
        assert_eq!(truncate(2, judge), "\u{1f468}\u{200d}\u{2696}\u{fe0f}");
        assert_eq!(truncate(1, judge), "");
        assert_eq!(truncate(99, judge), judge);
    }

    use super::*;

    #[test]
    fn truncates_on_display_width() {
        assert_eq!(truncate(10, "short"), "short");
        assert_eq!(truncate(10, "a much longer title"), "a much ...");
        assert_eq!(truncate(4, "abcdefgh"), "abcd");
        assert_eq!(truncate(5, "abcdefgh"), "ab...");
        // wide characters count double
        assert_eq!(truncate(7, "日本語のテキスト"), "日本...");
        assert_eq!(width("日本"), 4);
    }

    #[test]
    fn indents_and_pads() {
        assert_eq!(indent("a\n\nb", 2), "  a\n\n  b");
        assert_eq!(pad_right("ab", 4), "ab  ");
        assert_eq!(pad_right("abcd", 2), "abcd");
    }

    #[test]
    fn renders_html_notes() {
        let out = render_notes(
            Some("<body>Hello <b>world</b><ul><li>one</li></ul></body>"),
            None,
            80,
        );
        assert_eq!(out, "Hello world\n• one");
        assert_eq!(render_notes(None, Some("  plain  "), 80), "plain");
        assert_eq!(
            render_notes(Some("<body></body>"), Some("fallback"), 80),
            "fallback"
        );
        assert_eq!(render_notes(None, None, 80), "");
    }

    #[test]
    fn notes_keep_newlines_and_urls() {
        let html = "<body>Step one.\nStep two, see <a href=\"https://x.y/doc\">the doc</a>.\n\nDone.</body>";
        assert_eq!(
            render_notes(Some(html), None, 80),
            "Step one.\nStep two, see the doc https://x.y/doc.\n\nDone."
        );
        assert_eq!(render_notes(None, Some("a\nb\n\nc"), 80), "a\nb\n\nc");
        assert!(!render_notes(Some(html), None, 80).contains('\x1b'));
    }

    /// The wrapper is greedy, breaks only at spaces, hangs bullets and
    /// flag descriptions under their column, and leaves a lone long word (a
    /// URL) whole.
    #[test]
    fn wrap_line_keeps_columns_and_words() {
        assert_eq!(wrap_line("short", 20, "  ", "  "), vec!["  short"]);
        assert_eq!(wrap_line("", 20, "  ", "  "), vec![""]);
        assert_eq!(
            wrap_line("one two three four five", 14, "  ", "    "),
            vec!["  one two", "    three four", "    five"]
        );
        assert_eq!(
            wrap_line(
                "see https://example.com/a/very/long/path/indeed now",
                20,
                "",
                ""
            ),
            vec!["see", "https://example.com/a/very/long/path/indeed", "now"]
        );
        // No hyphenation: `case-insensitive` never splits at its hyphen.
        assert_eq!(
            wrap_line("a case-insensitive name", 17, "", ""),
            vec!["a", "case-insensitive", "name"]
        );
        // A line that fits keeps its spacing; a broken one drops the
        // spaces it broke at.
        assert_eq!(
            wrap_line("please run:  asana auth login", 40, "", ""),
            vec!["please run:  asana auth login"]
        );
        assert_eq!(
            wrap_line("please run:  asana auth login", 14, "", ""),
            vec!["please run:", "asana auth", "login"]
        );
    }

    /// A backtick span is one word: the formatting sentence on an 80-column
    /// terminal (79 usable) moves `asana help formatting` whole to the next
    /// line instead of breaking after `help`, and a span wider than the
    /// limit stays whole like a URL. Widths are unchanged: the glue is one
    /// cell, like the space it stands in for.
    #[test]
    fn code_spans_never_break_inside() {
        let sentence =
            "For more information about output formatting flags, see `asana help formatting`.";
        assert_eq!(width(sentence), 80);
        assert_eq!(
            wrap_line(sentence, 79, "", ""),
            vec![
                "For more information about output formatting flags, see",
                "`asana help formatting`."
            ]
        );
        assert_eq!(
            wrap_prose(sentence, 79),
            "For more information about output formatting flags, see\n`asana help formatting`."
        );
        assert_eq!(
            wrap_line("run `asana auth login --with-token` now", 12, "", ""),
            vec!["run", "`asana auth login --with-token`", "now"]
        );
        // The glue never reaches the output, and a stray backtick is plain.
        for line in wrap_line("a `b c` d `e f g", 5, "", "") {
            assert!(!line.contains(GLUE), "{line:?}");
        }
        assert_eq!(
            wrap_line("a `b c` d `e f g", 5, "", ""),
            vec!["a", "`b c`", "d `e", "f g"]
        );
        assert_eq!(glue_code_spans("`a b` c `d"), "`a\u{a0}b` c `d");
        assert_eq!(glue_code_spans("no spans"), "no spans");
        assert_eq!(glue_code_spans("`x\ny z`"), "`x\ny z`");
        // Error prose gets the same treatment through `wrap_sentences`.
        assert_eq!(
            wrap_sentences("To get started, run `asana auth login` now", 30),
            "To get started, run\n`asana auth login` now"
        );
    }

    /// Running text: lines that fit are untouched, wide ones keep their
    /// indent and hang their bullets, blank lines and a trailing newline
    /// survive.
    #[test]
    fn prose_keeps_indents_bullets_and_blank_lines() {
        assert_eq!(wrap_prose("short", 20), "short");
        assert_eq!(wrap_prose("a\n\nb\n", 20), "a\n\nb\n");
        assert_eq!(wrap_prose("", 20), "");
        assert_eq!(wrap_prose("trailing   \n", 20), "trailing\n");
        assert_eq!(
            wrap_prose(
                "- by gid, e.g. \"1211234567890123\"; or\n\nplain https://x.y/z end",
                22
            ),
            "- by gid, e.g.\n  \"1211234567890123\";\n  or\n\nplain https://x.y/z\nend"
        );
        assert_eq!(
            wrap_prose("  one two three four", 13),
            "  one two\n  three four"
        );
        assert_eq!(
            wrap_prose("  * one two three four", 15),
            "  * one two\n    three four"
        );
        // The greedy rule, not the sentence rule: a wide sentence pair is
        // filled to the width.
        assert_eq!(
            wrap_prose("One two three. Four five six.", 20),
            "One two three. Four\nfive six."
        );
    }

    /// Error prose is wrapped under the limit: a line that fits is
    /// untouched (double spaces and all), a wide one breaks between
    /// sentences or clauses before it breaks at spaces, and a word wider
    /// than the limit (a URL) is never split.
    #[test]
    fn sentences_break_between_sentences_then_at_spaces() {
        let short = "To get started, please run:  asana auth login";
        assert_eq!(wrap_sentences(short, PROSE_WIDTH), short);
        assert_eq!(wrap_sentences("a\n\nb\n", PROSE_WIDTH), "a\n\nb\n");
        assert_eq!(wrap_sentences("", PROSE_WIDTH), "");
        let two = format!("{} {}", "x".repeat(40), "y".repeat(40));
        assert_eq!(
            wrap_sentences(&two, PROSE_WIDTH),
            format!("{}\n{}", "x".repeat(40), "y".repeat(40))
        );
        let three = format!(
            "{}. {}. {}.",
            "a".repeat(30),
            "b".repeat(30),
            "c".repeat(30)
        );
        assert_eq!(
            wrap_sentences(&three, PROSE_WIDTH),
            format!(
                "{}. {}.\n{}.",
                "a".repeat(30),
                "b".repeat(30),
                "c".repeat(30)
            )
        );
        let clause = format!(
            "invalid task format: \"{}\"; expected a gid",
            "z".repeat(50)
        );
        assert_eq!(
            wrap_sentences(&clause, PROSE_WIDTH),
            format!(
                "invalid task format: \"{}\";\nexpected a gid",
                "z".repeat(50)
            )
        );
        // A clause still too wide breaks after its colon, then at spaces;
        // the strongest mark available wins each time.
        let colon = format!(
            "invalid argument \"{}\" for \"--limit\" flag: number is too large",
            "9".repeat(30)
        );
        assert_eq!(
            wrap_sentences(&colon, PROSE_WIDTH),
            format!(
                "invalid argument \"{}\" for \"--limit\" flag:\nnumber is too large",
                "9".repeat(30)
            )
        );
        let prose = "Alternatively, populate the ASANA_TOKEN environment variable with a personal access token from https://app.asana.com/0/my-apps.";
        assert_eq!(
            wrap_sentences(prose, PROSE_WIDTH),
            "Alternatively, populate the ASANA_TOKEN environment variable with a personal\naccess token from https://app.asana.com/0/my-apps."
        );
        let url = format!("see https://example.com/{}", "p".repeat(90));
        assert_eq!(
            wrap_sentences(&url, PROSE_WIDTH),
            format!("see\nhttps://example.com/{}", "p".repeat(90))
        );
        // An indented line keeps its indent on every line it becomes.
        assert_eq!(
            wrap_sentences("  Task 1 was not found. Check the gid.", 30),
            "  Task 1 was not found.\n  Check the gid."
        );
        assert_eq!(
            split_after("Task 1 was not found. Check the gid. app.asana.com", b".;"),
            vec!["Task 1 was not found.", "Check the gid.", "app.asana.com"]
        );
        assert_eq!(split_after("one; two", b".;"), vec!["one;", "two"]);
        assert_eq!(split_after("a: b", b".;"), vec!["a: b"]);
        assert_eq!(split_after("", b".;"), Vec::<&str>::new());
    }

    #[test]
    fn prose_width_never_passes_the_limit() {
        assert!(prose_width() <= PROSE_WIDTH);
        assert!(terminal_width() > 0);
    }

    #[test]
    fn unbrackets_autolinks_only() {
        assert_eq!(
            unbracket_urls("at <https://a.b/c>, then"),
            "at https://a.b/c, then"
        );
        assert_eq!(unbracket_urls("<https://a.b/c>"), "https://a.b/c");
        assert_eq!(
            unbracket_urls("key:<TAB>value <not a url>"),
            "key:<TAB>value <not a url>"
        );
        assert_eq!(unbracket_urls("<https://a.b/c d>"), "<https://a.b/c d>");
        assert_eq!(unbracket_urls("<http"), "<http");
    }

    #[test]
    fn strips_scheme() {
        assert_eq!(
            display_url("https://app.asana.com/0/1/2"),
            "app.asana.com/0/1/2"
        );
    }
}
