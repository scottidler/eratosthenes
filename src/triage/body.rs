//! Body extraction for triage: the MIME walk, html->text fallback, and
//! quoted-reply stripping the design doc's Data Plumbing section calls for.
//!
//! The aging engine never needed a body, so none of this existed. Everything
//! here is a pure function over data the `format=full` fetch already returned:
//! no I/O, so every branch is unit-testable against a hand-built `MessagePart`.

use google_gmail1::api::MessagePart;
use log::trace;

/// Appended when a body is cut off, so a truncated excerpt never reads to the
/// classifier as a complete message that simply ended mid-sentence.
pub const TRUNCATION_MARKER: &str = "\n[truncated]";

/// The smallest budget that can carry a MARKED fragment. Below it, `truncate`
/// can only cut bare, and an unmarked sliver reads as a COMPLETE short message
/// -- so a caller spending a budget message-by-message should stop rather than
/// emit one. ASCII marker, so `len()` is the char count (asserted in tests)
/// and this stays a `const`.
pub const MIN_MARKED_FRAGMENT_CHARS: usize = TRUNCATION_MARKER.len() + 1;

/// Walk a message payload and return its text, preferring `text/plain` over
/// `text/html` ANYWHERE in the tree. Preference is by mime type, not by
/// position: `multipart/alternative` puts the html sibling last about as often
/// as first, so a first-match-wins walk would pick html for half of all mail.
pub fn extract_body(payload: &MessagePart) -> String {
    trace!(
        "extract_body: mime_type={:?}",
        payload.mime_type.as_deref().unwrap_or("")
    );

    if let Some(text) = collect_by_mime(payload, "text/plain") {
        return normalize(&text);
    }
    if let Some(html) = collect_by_mime(payload, "text/html") {
        return normalize(&html_to_text(&html));
    }
    String::new()
}

/// Depth-first search for the first part of `want`, skipping attachments.
/// A part with a `filename` is an attachment even when its mime type is
/// `text/plain` (a `.log` or `.txt` file), and attachment text is not the
/// message.
fn collect_by_mime(part: &MessagePart, want: &str) -> Option<String> {
    let is_attachment = part.filename.as_deref().is_some_and(|f| !f.is_empty());
    let mime = part.mime_type.as_deref().unwrap_or("");

    if !is_attachment
        && mime.starts_with(want)
        && let Some(data) = part.body.as_ref().and_then(|b| b.data.as_ref())
        && !data.is_empty()
    {
        return Some(String::from_utf8_lossy(data).into_owned());
    }

    for child in part.parts.iter().flatten() {
        if let Some(found) = collect_by_mime(child, want) {
            return Some(found);
        }
    }
    None
}

/// Strip tags to plain text. Deliberately NOT an html parser: the consumer is
/// an LLM classifier that needs the words, not the document structure, and a
/// parser dependency would be carried for a fallback path most mail never
/// takes.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut skip_until: Option<&str> = None;

    while let Some(c) = chars.next() {
        if c != '<' {
            if skip_until.is_none() {
                out.push(c);
            }
            continue;
        }

        let mut tag = String::new();
        for tc in chars.by_ref() {
            if tc == '>' {
                break;
            }
            tag.push(tc);
        }
        let lower = tag.trim().to_lowercase();

        // <script> and <style> bodies are code, not prose: drop the whole
        // element rather than emitting minified javascript at the classifier.
        if let Some(end) = skip_until {
            if lower.starts_with(end) {
                skip_until = None;
            }
            continue;
        }
        if lower.starts_with("script") {
            skip_until = Some("/script");
            continue;
        }
        if lower.starts_with("style") {
            skip_until = Some("/style");
            continue;
        }

        if is_block_tag(&lower) {
            out.push('\n');
        }
    }

    decode_entities(&out)
}

/// Both `<p>` and `</p>` count: the goal is a line break where the rendered
/// document had one, not correct html semantics.
fn is_block_tag(lower: &str) -> bool {
    let name = lower
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("");
    matches!(
        name,
        "br" | "p"
            | "div"
            | "tr"
            | "li"
            | "ul"
            | "ol"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "table"
            | "blockquote"
            | "hr"
    )
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Collapse the whitespace mail clients scatter through bodies: trailing
/// spaces, `\r\n`, and runs of blank lines that would otherwise eat a large
/// share of the per-thread char budget.
fn normalize(s: &str) -> String {
    let cleaned = s.replace('\r', "");
    let mut lines: Vec<&str> = Vec::new();
    let mut blank_run = 0;
    for raw in cleaned.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

/// Drop quoted reply chains and signatures. What survives is what the sender
/// actually wrote in THIS message, which is the only part that carries the
/// signal the classifier needs; the quoted chain is the previous messages,
/// already in the payload under their own ids.
pub fn strip_quotes(text: &str) -> String {
    trace!("strip_quotes: chars={}", text.len());
    let mut kept: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        if is_quote_boundary(trimmed) {
            break;
        }
        // `-- ` is the RFC 3676 signature delimiter. Everything after it is a
        // signature block: contact details, legal boilerplate, nothing about
        // the message.
        if trimmed == "--" || line == "-- " {
            break;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        kept.push(line);
    }

    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    kept.join("\n").trim().to_string()
}

/// The line that starts a quoted chain. Covers the three shapes seen in
/// practice: Gmail/Apple attribution (`On <date>, <name> wrote:`), Outlook's
/// `-----Original Message-----`, and Outlook-web's horizontal rule of
/// underscores.
fn is_quote_boundary(trimmed: &str) -> bool {
    if trimmed.starts_with("-----Original Message") {
        return true;
    }
    if trimmed.len() >= 10 && trimmed.chars().all(|c| c == '_') {
        return true;
    }
    // Covers both the one-line Gmail attribution (`On <date>, <name> wrote:`)
    // and the wrapped two-line form, whose second line is just
    // `<name> wrote:`. The length bound is what keeps a prose sentence that
    // happens to end in "wrote:" from truncating a real body.
    trimmed.ends_with(" wrote:") && trimmed.len() < 120
}

/// Truncate to `max` CHARACTERS, not bytes: `body-chars` is a config number a
/// human picked, and a byte cut would split a multi-byte character and panic
/// on the slice.
///
/// `max` is a HARD cap on the returned length. An earlier version cut to `max`
/// and THEN appended the marker, so every truncated body overshot the config
/// number by the marker's length (audit C4) and a per-thread budget spent
/// message-by-message could overshoot once per cut. The marker is now paid for
/// out of the budget, matching `digest::bullets::shrink`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let marker = TRUNCATION_MARKER.chars().count();
    // No room to both cut and mark: a bare cut is the only thing that fits.
    if max <= marker {
        return text.chars().take(max).collect();
    }
    let head: String = text.chars().take(max - marker).collect();
    format!("{}{}", head.trim_end(), TRUNCATION_MARKER)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use google_gmail1::api::MessagePartBody;

    fn part(mime: &str, data: &str) -> MessagePart {
        MessagePart {
            mime_type: Some(mime.to_string()),
            body: Some(MessagePartBody {
                data: Some(data.as_bytes().to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn container(mime: &str, parts: Vec<MessagePart>) -> MessagePart {
        MessagePart {
            mime_type: Some(mime.to_string()),
            parts: Some(parts),
            ..Default::default()
        }
    }

    #[test]
    fn test_extract_body_single_plain_part() {
        let payload = part("text/plain", "hello there");
        assert_eq!(extract_body(&payload), "hello there");
    }

    /// text/plain wins even when the html sibling comes FIRST in the tree.
    #[test]
    fn test_extract_body_prefers_plain_over_html_regardless_of_order() {
        let payload = container(
            "multipart/alternative",
            vec![
                part("text/html", "<p>html version</p>"),
                part("text/plain", "plain version"),
            ],
        );
        assert_eq!(extract_body(&payload), "plain version");
    }

    #[test]
    fn test_extract_body_falls_back_to_html() {
        let payload = container(
            "multipart/alternative",
            vec![part("text/html", "<p>only html</p>")],
        );
        assert_eq!(extract_body(&payload), "only html");
    }

    #[test]
    fn test_extract_body_walks_nested_containers() {
        let payload = container(
            "multipart/mixed",
            vec![
                container(
                    "multipart/alternative",
                    vec![part("text/plain", "nested plain")],
                ),
                part("application/pdf", "binary"),
            ],
        );
        assert_eq!(extract_body(&payload), "nested plain");
    }

    /// A `.txt` ATTACHMENT is not the message body.
    #[test]
    fn test_extract_body_skips_attachment_text_parts() {
        let mut attachment = part("text/plain", "attached log contents");
        attachment.filename = Some("server.log".to_string());
        let payload = container(
            "multipart/mixed",
            vec![attachment, part("text/plain", "real body")],
        );
        assert_eq!(extract_body(&payload), "real body");
    }

    #[test]
    fn test_extract_body_empty_when_no_text_parts() {
        let payload = container("multipart/mixed", vec![part("application/pdf", "x")]);
        assert_eq!(extract_body(&payload), "");
    }

    #[test]
    fn test_html_to_text_drops_script_and_style() {
        let html = "<style>body{color:red}</style><p>visible</p><script>alert(1)</script>";
        let text = normalize(&html_to_text(html));
        assert_eq!(text, "visible");
    }

    #[test]
    fn test_html_to_text_decodes_entities() {
        let text = normalize(&html_to_text("<p>A&nbsp;&amp;&nbsp;B &lt;tag&gt;</p>"));
        assert_eq!(text, "A & B <tag>");
    }

    #[test]
    fn test_html_to_text_breaks_lines_on_block_tags() {
        let text = normalize(&html_to_text("one<br>two<br/>three"));
        assert_eq!(text, "one\ntwo\nthree");
    }

    #[test]
    fn test_strip_quotes_removes_gmail_attribution_and_chain() {
        let body = "Sure, that works for me.\n\nOn Mon, Sep 1, 2026 at 9:00 AM Bob <bob@x.com> wrote:\n> can you review this?\n> thanks";
        assert_eq!(strip_quotes(body), "Sure, that works for me.");
    }

    #[test]
    fn test_strip_quotes_removes_outlook_original_message() {
        let body = "Approved.\n\n-----Original Message-----\nFrom: Bob\nSubject: thing";
        assert_eq!(strip_quotes(body), "Approved.");
    }

    #[test]
    fn test_strip_quotes_removes_signature_block() {
        let body = "Ship it.\n-- \nScott Idler\nHead of Security";
        assert_eq!(strip_quotes(body), "Ship it.");
    }

    #[test]
    fn test_strip_quotes_drops_bare_quoted_lines() {
        let body = "> old text\nnew text\n> more old";
        assert_eq!(strip_quotes(body), "new text");
    }

    #[test]
    fn test_strip_quotes_keeps_unquoted_body_untouched() {
        let body = "line one\n\nline two";
        assert_eq!(strip_quotes(body), "line one\n\nline two");
    }

    #[test]
    fn test_truncate_under_budget_is_unchanged() {
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn test_truncate_marks_the_cut() {
        let out = truncate(&"a".repeat(40), 20);
        let keep = 20 - TRUNCATION_MARKER.chars().count();
        assert_eq!(out, format!("{}{}", "a".repeat(keep), TRUNCATION_MARKER));
    }

    /// Audit C4: `max` is a hard cap, so the marker comes out of the budget
    /// rather than being added on top of it.
    #[test]
    fn test_truncate_never_exceeds_max() {
        for max in 1..=64 {
            let out = truncate(&"x".repeat(500), max);
            assert!(
                out.chars().count() <= max,
                "max={} produced {} chars",
                max,
                out.chars().count()
            );
        }
    }

    /// A budget smaller than the marker cannot carry one, so it cuts bare
    /// rather than blowing the cap to make room for the annotation.
    #[test]
    fn test_truncate_below_marker_length_cuts_bare() {
        let out = truncate("abcdefghij", 4);
        assert_eq!(out, "abcd");
    }

    /// Char-based, so a multi-byte body cannot panic on a byte-boundary slice.
    #[test]
    fn test_truncate_is_char_safe() {
        let out = truncate("ααααα", 2);
        assert!(out.starts_with("αα"), "got: {}", out);
    }
}

#[cfg(test)]
mod fragment_floor_tests {
    use super::*;

    /// `MIN_MARKED_FRAGMENT_CHARS` is built from `len()`, which is only the
    /// char count while the marker stays ASCII.
    #[test]
    fn test_marker_is_ascii_so_len_is_the_char_count() {
        assert_eq!(TRUNCATION_MARKER.len(), TRUNCATION_MARKER.chars().count());
        assert_eq!(
            MIN_MARKED_FRAGMENT_CHARS,
            TRUNCATION_MARKER.chars().count() + 1
        );
    }

    /// The floor is exactly the point where a cut can still be announced.
    #[test]
    fn test_at_the_floor_the_cut_is_marked_and_below_it_is_not() {
        let long = "z".repeat(500);
        let at = truncate(&long, MIN_MARKED_FRAGMENT_CHARS);
        assert!(at.contains("[truncated]"), "{}", at);
        assert!(at.chars().count() <= MIN_MARKED_FRAGMENT_CHARS);

        let below = truncate(&long, MIN_MARKED_FRAGMENT_CHARS - 1);
        assert!(!below.contains("[truncated]"), "{}", below);
    }
}
