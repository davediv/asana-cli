//! Display-width aware truncation, indentation, wrapping, and the plain
//! shape of Asana notes (`render_notes`; the full renderer is `render::notes`).

use unicode_width::UnicodeWidthStr;

/// Display width in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
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
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(ch);
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

/// Wrap paragraphs to `w` cells, preserving blank lines between them.
#[allow(dead_code)]
pub fn wrap(s: &str, w: usize) -> String {
    let opts = textwrap::Options::new(w.max(20)).break_words(false);
    s.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                textwrap::wrap(line, &opts).join("\n")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn strips_scheme() {
        assert_eq!(
            display_url("https://app.asana.com/0/1/2"),
            "app.asana.com/0/1/2"
        );
    }
}
