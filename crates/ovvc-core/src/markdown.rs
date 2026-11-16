//! A deliberately small Markdown subset for chat messages.
//!
//! Full Markdown is a document format, not a chat format. This parser covers
//! what people actually type in a message: bold, italic, inline code,
//! strikethrough, spoilers, links, `@mentions`, `#channel` references, and
//! fenced code blocks. Everything it
//! does not understand is kept as literal text, so a malformed message renders
//! the way its author typed it instead of disappearing.
//!
//! The parser is pure and allocation-only. Each frontend turns [`Span`]s into
//! its own styled text (an `egui::text::LayoutJob` in the GUI, ratatui
//! `Span`s in the terminal), and tests and the fuzzer exercise it directly.

/// Styling attributes for a run of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub mention: bool,
    pub link: bool,
    /// `||hidden||`. Frontends keep the text but hide it until the reader
    /// chooses to reveal it.
    pub spoiler: bool,
    /// `#channel-name`, a reference to a channel in the same guild.
    pub channel: bool,
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }
}

/// A styled run of text on one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One rendered line. `code_block` marks a line inside a fenced block, which
/// the renderer paints on a monospace background.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
    pub code_block: bool,
}

/// Parse a message body into lines and styled spans.
pub fn parse(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut in_fence = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            lines.push(Line {
                spans: vec![Span {
                    text: raw.to_string(),
                    style: Style {
                        code: true,
                        ..Style::default()
                    },
                }],
                code_block: true,
            });
            continue;
        }
        lines.push(parse_inline(raw));
    }
    lines
}

fn parse_inline(text: &str) -> Line {
    let mut spans: Vec<Span> = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut plain = String::new();

    while index < bytes.len() {
        let rest = &text[index..];
        let previous = text[..index].chars().next_back();
        if let Some((consumed, span)) = match_inline(rest, previous) {
            flush(&mut spans, &mut plain);
            spans.push(span);
            index += consumed;
            continue;
        }
        let c = rest.chars().next().unwrap_or('\u{fffd}');
        plain.push(c);
        index += c.len_utf8();
    }
    flush(&mut spans, &mut plain);
    if spans.is_empty() {
        spans.push(Span {
            text: String::new(),
            style: Style::default(),
        });
    }
    Line {
        spans,
        code_block: false,
    }
}

fn flush(spans: &mut Vec<Span>, plain: &mut String) {
    if plain.is_empty() {
        return;
    }
    let text = std::mem::take(plain);
    spans.push(Span {
        text,
        style: Style::default(),
    });
}

fn match_inline(rest: &str, previous: Option<char>) -> Option<(usize, Span)> {
    if let Some(after) = rest.strip_prefix("**")
        && let Some(end) = after.find("**")
        && end > 0
    {
        return Some((
            end + 4,
            Span {
                text: after[..end].to_string(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(after) = rest.strip_prefix("__")
        && let Some(end) = after.find("__")
        && end > 0
    {
        return Some((
            end + 4,
            Span {
                text: after[..end].to_string(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(after) = rest.strip_prefix("~~")
        && let Some(end) = after.find("~~")
        && end > 0
    {
        return Some((
            end + 4,
            Span {
                text: after[..end].to_string(),
                style: Style {
                    strike: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(after) = rest.strip_prefix('`')
        && let Some(end) = after.find('`')
        && end > 0
    {
        return Some((
            end + 2,
            Span {
                text: after[..end].to_string(),
                style: Style {
                    code: true,
                    ..Style::default()
                },
            },
        ));
    }
    // Italic uses one marker so `*word*` and `_word_` both work. A bold marker
    // is matched above, so `**` never falls into this branch.
    if let Some(rest) = rest.strip_prefix('*')
        && let Some(end) = rest.find('*')
        && end > 0
    {
        return Some((
            end + 2,
            Span {
                text: rest[..end].to_string(),
                style: Style {
                    italic: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(rest) = rest.strip_prefix('_')
        && let Some(end) = rest.find('_')
        && end > 0
    {
        return Some((
            end + 2,
            Span {
                text: rest[..end].to_string(),
                style: Style {
                    italic: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(rest) = rest.strip_prefix('[')
        && let Some(label_end) = rest.find("](")
        && let Some(url_end) = rest[label_end + 2..].find(')')
    {
        let label = &rest[..label_end];
        let url = &rest[label_end + 2..label_end + 2 + url_end];
        if !label.is_empty() && !url.is_empty() {
            return Some((
                label_end + url_end + 4,
                Span {
                    text: label.to_string(),
                    style: Style {
                        link: true,
                        ..Style::default()
                    },
                },
            ));
        }
    }
    if let Some(after) = rest.strip_prefix("||")
        && let Some(end) = after.find("||")
        && end > 0
    {
        return Some((
            end + 4,
            Span {
                text: after[..end].to_string(),
                style: Style {
                    spoiler: true,
                    ..Style::default()
                },
            },
        ));
    }
    if let Some(name) = rest.strip_prefix('#') {
        let end = name
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '-')))
            .map(|(index, _)| index)
            .unwrap_or(name.len());
        let at_word_start = previous.is_none_or(|c| !c.is_alphanumeric());
        if end > 0 && at_word_start {
            return Some((
                end + 1,
                Span {
                    text: format!("#{}", &name[..end]),
                    style: Style {
                        channel: true,
                        ..Style::default()
                    },
                },
            ));
        }
    }
    if let Some(rest) = rest.strip_prefix('@') {
        let end = rest
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        if end > 0 {
            return Some((
                end + 1,
                Span {
                    text: format!("@{}", &rest[..end]),
                    style: Style {
                        mention: true,
                        ..Style::default()
                    },
                },
            ));
        }
    }
    None
}

/// The message as plain text, with markers removed. Useful for search results,
/// notifications and window titles.
pub fn plain(text: &str) -> String {
    let mut out = String::new();
    for (index, line) in parse(text).into_iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        for span in line.spans {
            out.push_str(&span.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &Line) -> Vec<(&str, Style)> {
        line.spans
            .iter()
            .map(|span| (span.text.as_str(), span.style))
            .collect()
    }

    #[test]
    fn plain_text_is_one_span() {
        let lines = parse("hello there");
        assert_eq!(lines.len(), 1);
        assert_eq!(texts(&lines[0]), vec![("hello there", Style::default())]);
    }

    #[test]
    fn inline_styles_are_recognised() {
        let lines = parse("a **bold** b *it* c `code` d ~~gone~~ e");
        let flat: String = lines[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(flat, "a bold b it c code d gone e");
        assert!(lines[0].spans.iter().any(|span| span.style.bold));
        assert!(lines[0].spans.iter().any(|span| span.style.italic));
        assert!(lines[0].spans.iter().any(|span| span.style.code));
        assert!(lines[0].spans.iter().any(|span| span.style.strike));
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        for text in ["2 * 3 = 6", "**unclosed", "~~unclosed", "`unclosed"] {
            let lines = parse(text);
            let flat: String = lines[0]
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect();
            assert_eq!(flat, text);
            assert!(lines[0].spans.iter().all(|span| span.style.is_plain()));
        }
    }

    #[test]
    fn mentions_and_links_are_styled() {
        let lines = parse("hi @alice see [docs](https://example.com)");
        assert!(lines[0].spans.iter().any(|span| span.style.mention));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.link && span.text == "docs")
        );
    }

    #[test]
    fn spoilers_and_channel_links_are_styled() {
        let lines = parse("careful ||the ending|| see #dev-general");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.spoiler && span.text == "the ending")
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.channel && span.text == "#dev-general")
        );
    }

    #[test]
    fn a_channel_hash_inside_a_word_is_literal() {
        let lines = parse("C# is fine");
        assert!(lines[0].spans.iter().all(|span| !span.style.channel));
        assert_eq!(plain("C# is fine"), "C# is fine");
    }

    #[test]
    fn fenced_code_blocks_are_not_inline_parsed() {
        let lines = parse("before\n```\nlet **x** = 1;\n```\nafter");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].code_block);
        assert_eq!(lines[1].spans[0].text, "let **x** = 1;");
        assert!(lines[1].spans[0].style.code);
        assert!(!lines[2].code_block);
    }

    #[test]
    fn empty_and_multiline_text_is_stable() {
        assert!(plain("").is_empty());
        assert_eq!(plain("**hi**\n`x`"), "hi\nx");
        assert_eq!(parse("one\ntwo").len(), 2);
    }

    #[test]
    fn pathological_markers_do_not_panic() {
        for case in [
            "*",
            "**",
            "***",
            "``",
            "~~",
            "@",
            "[",
            "](",
            "```",
            "``````",
            "@@@",
            "||",
            "#",
            "||unclosed",
            "*a**b*",
            "**a*b**",
            "[a](",
            "\u{1f600}**\u{1f600}**",
        ] {
            let _ = parse(case);
            let _ = plain(case);
        }
    }
}
