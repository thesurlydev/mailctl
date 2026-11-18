//! Links in an HTML message, for `read --links`.
//!
//! `mail-parser` renders HTML to text and drops every `href`, so a job alert's
//! body names the roles but not where they are. `from_raw` reads the anchors
//! from the HTML part instead. Everything returned is untrusted text written
//! by the sender, like the body: only `http` and `https` URLs are kept, and
//! nothing here fetches or opens one.

use crate::model::{Link, truncate_chars};

/// Most links returned for one message, so a newsletter cannot flood the caller.
pub const MAX_LINKS: usize = 200;
/// Longest anchor text kept, in characters.
const MAX_TEXT_CHARS: usize = 200;
/// Longest URL kept, in bytes. Tracking URLs are long, but not this long.
const MAX_URL_BYTES: usize = 4096;

/// The links in the HTML parts of a raw message, in document order. Plain-text
/// messages have none: their URLs are already in the body.
pub fn from_raw(raw: &[u8]) -> Vec<Link> {
    let Some(parsed) = mail_parser::MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for html in parsed.html_bodies() {
        extract_into(&html, &mut links);
    }
    links
}

/// Add the anchors of `html` to `out`. A URL seen again keeps its place, and
/// takes the new anchor's text when the first one had none (an image link
/// followed by a text link to the same job, for example).
fn extract_into(html: &str, out: &mut Vec<Link>) {
    // ASCII lowercasing keeps every byte offset, so a position found in
    // `lower` is a valid position in `html`.
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let after = pos + rel + 2;
        pos = after;
        // `<a` must be followed by whitespace: `<abbr` and `<article` are not anchors.
        if !lower
            .as_bytes()
            .get(after)
            .is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }
        let Some(tag_end) = tag_end(html, after) else {
            break;
        };
        let body_start = tag_end + 1;
        let close = lower[body_start..].find("</a").map(|i| body_start + i);
        pos = close.unwrap_or(body_start);

        let Some(url) = attr(&html[after..tag_end], "href").and_then(clean_url) else {
            continue;
        };
        let text = close
            .map(|c| visible_text(&html[body_start..c]))
            .unwrap_or_default();
        let (text, _) = truncate_chars(text, MAX_TEXT_CHARS);

        if let Some(seen) = out.iter_mut().find(|l| l.url == url) {
            if seen.text.is_empty() {
                seen.text = text;
            }
        } else if out.len() < MAX_LINKS {
            out.push(Link { text, url });
        }
    }
}

/// The URL when it is a plain `http` or `https` link and nothing odd.
fn clean_url(url: String) -> Option<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let web = lower.starts_with("http://") || lower.starts_with("https://");
    (web && url.len() <= MAX_URL_BYTES && !url.chars().any(|c| c.is_control() || c == ' '))
        .then(|| url.to_string())
}

/// The index of the `>` that ends the tag whose attributes start at `from`,
/// skipping any `>` inside a quoted value.
fn tag_end(s: &str, from: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &c) in s.as_bytes().iter().enumerate().skip(from) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(c),
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// The decoded value of attribute `name` in the text between `<a` and `>`.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let before = i;
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let key_start = i;
        // Only ASCII bytes stop these scans, so every slice is on a character boundary.
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let key = &attrs[key_start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = None;
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                let start = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = Some(&attrs[start..i]);
                i = (i + 1).min(b.len());
            } else {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = Some(&attrs[start..i]);
            }
        }
        if key.eq_ignore_ascii_case(name) {
            return value.map(decode_entities);
        }
        if i == before {
            i += 1;
        }
    }
    None
}

/// The text of an HTML fragment: tags removed, entities decoded, whitespace collapsed.
fn visible_text(fragment: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in fragment.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode the named and numeric entities mail uses. An unknown `&name;` is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest
            .char_indices()
            .take(12)
            .find(|(_, c)| *c == ';')
            .map(|(j, _)| j);
        match semi.and_then(|j| entity(&rest[1..j]).map(|c| (c, j))) {
            Some((c, j)) => {
                out.push(c);
                rest = &rest[j + 1..];
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

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(html: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        extract_into(html, &mut out);
        out.into_iter().map(|l| (l.text, l.url)).collect()
    }

    fn pair(text: &str, url: &str) -> (String, String) {
        (text.to_string(), url.to_string())
    }

    #[test]
    fn an_anchor_gives_its_text_and_decoded_url() {
        assert_eq!(
            links(
                r#"<p><a href="https://example.com/jobs/1?a=1&amp;b=2">Staff <b>Engineer</b></a></p>"#
            ),
            vec![pair("Staff Engineer", "https://example.com/jobs/1?a=1&b=2")]
        );
    }

    #[test]
    fn tag_and_attribute_names_are_case_insensitive_and_quotes_optional() {
        assert_eq!(
            links(
                "<A CLASS=x HREF='http://a.example/x'>one</A><a href=https://b.example/y>two</a>"
            ),
            vec![
                pair("one", "http://a.example/x"),
                pair("two", "https://b.example/y")
            ]
        );
    }

    #[test]
    fn only_web_links_are_kept() {
        let html = r#"<a href="mailto:a@b.c">m</a><a href="javascript:alert(1)">j</a>
            <a href="/relative">r</a><a href="tel:123">t</a><a href="https://ok.example/">ok</a>"#;
        assert_eq!(links(html), vec![pair("ok", "https://ok.example/")]);
    }

    #[test]
    fn other_tags_that_start_with_a_are_not_anchors() {
        let html =
            r#"<abbr title="x">A</abbr><article>B</article><a href="https://ok.example/">ok</a>"#;
        assert_eq!(links(html), vec![pair("ok", "https://ok.example/")]);
    }

    #[test]
    fn a_greater_than_inside_a_quoted_value_does_not_end_the_tag() {
        assert_eq!(
            links(r#"<a title="a > b" href="https://ok.example/p">go</a>"#),
            vec![pair("go", "https://ok.example/p")]
        );
    }

    #[test]
    fn a_repeated_url_keeps_its_place_and_gains_the_first_text() {
        let html = r#"<a href="https://x.example/1"><img src="i.png"></a>
            <a href="https://x.example/2">Second</a>
            <a href="https://x.example/1">First</a>
            <a href="https://x.example/2">Again</a>"#;
        assert_eq!(
            links(html),
            vec![
                pair("First", "https://x.example/1"),
                pair("Second", "https://x.example/2")
            ]
        );
    }

    #[test]
    fn anchor_text_has_entities_decoded_and_whitespace_collapsed() {
        assert_eq!(
            links(
                "<a href=\"https://x.example/\">\n  Caf&eacute; &amp;&nbsp;Co &#8212; &#x41;\n</a>"
            ),
            vec![pair("Caf&eacute; & Co \u{2014} A", "https://x.example/")]
        );
    }

    #[test]
    fn long_text_is_cut_by_characters() {
        let html = format!(
            "<a href=\"https://x.example/\">{}</a>",
            "\u{20ac}".repeat(500)
        );
        let got = links(&html);
        assert_eq!(got[0].0.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn the_number_of_links_is_capped() {
        let html: String = (0..MAX_LINKS + 50)
            .map(|i| format!("<a href=\"https://x.example/{i}\">n</a>"))
            .collect();
        assert_eq!(links(&html).len(), MAX_LINKS);
    }

    #[test]
    fn broken_html_does_not_panic() {
        for html in [
            "<a",
            "<a ",
            "<a href",
            "<a href=",
            "<a href=\"",
            "<a href=\"https://x.example/\"",
            "<a href=\"https://x.example/\">never closed",
            "<a =>x</a>",
            "<a \u{20ac}=\u{20ac}>x</a>",
            "&",
            "&#;",
            "&#xZZ;",
            "&amp",
        ] {
            let _ = links(html);
        }
    }

    #[test]
    fn a_url_with_a_control_character_or_space_is_dropped() {
        let html =
            "<a href=\"https://x.example/a b\">s</a><a href=\"https://x.example/a\u{7}b\">c</a>";
        assert!(links(html).is_empty());
    }

    #[test]
    fn from_raw_reads_the_html_part_of_a_message() {
        let raw = b"Subject: alert\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nStaff Engineer, Acme\r\n\
--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<p><a href=\"https://www.example.com/jobs/view/42?trk=a&amp;ref=b\">Staff Engineer</a> Acme</p>\r\n\
--b--\r\n";
        let got = from_raw(raw);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Staff Engineer");
        assert_eq!(
            got[0].url,
            "https://www.example.com/jobs/view/42?trk=a&ref=b"
        );
    }
}
