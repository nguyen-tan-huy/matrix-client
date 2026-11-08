//! Message bodies are markdown (what app.js renders with `marked`): turned
//! into Pango markup for a `gtk::Label` here. Everything from the message
//! is escaped; only the tags below are ever emitted, so a body can't inject
//! markup. Bare URLs become links too (GFM autolinks).

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub fn to_pango(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 32);
    let mut list_depth: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;
    let mut in_link = false;
    let mut pending_break = false;
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;

    let brk = |out: &mut String, pending: &mut bool| {
        if *pending && !out.is_empty() {
            out.push('\n');
        }
        *pending = false;
    };

    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => brk(&mut out, &mut pending_break),
                Tag::Heading { level, .. } => {
                    brk(&mut out, &mut pending_break);
                    let size = match level {
                        HeadingLevel::H1 => "x-large",
                        HeadingLevel::H2 => "large",
                        _ => "medium",
                    };
                    out.push_str(&format!("<span size=\"{size}\" weight=\"bold\">"));
                }
                Tag::BlockQuote(_) => {
                    brk(&mut out, &mut pending_break);
                    out.push_str("<span alpha=\"70%\">▎ ");
                }
                Tag::CodeBlock(_) => {
                    brk(&mut out, &mut pending_break);
                    in_code_block = true;
                    out.push_str("<tt>");
                }
                Tag::List(start) => {
                    if list_depth.is_empty() {
                        brk(&mut out, &mut pending_break);
                    }
                    list_depth.push(start);
                }
                Tag::Item => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    let indent = "    ".repeat(list_depth.len().saturating_sub(1));
                    let bullet = match list_depth.last_mut() {
                        Some(Some(n)) => {
                            let b = format!("{n}. ");
                            *n += 1;
                            b
                        }
                        _ => "• ".to_string(),
                    };
                    out.push_str(&indent);
                    out.push_str(&bullet);
                }
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    in_link = true;
                    out.push_str(&format!("<a href=\"{}\">", glib_escape(&dest_url)));
                }
                Tag::Image { dest_url, .. } => {
                    in_link = true;
                    out.push_str(&format!("<a href=\"{}\">", glib_escape(&dest_url)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => pending_break = true,
                TagEnd::Heading(_) => {
                    out.push_str("</span>");
                    pending_break = true;
                }
                TagEnd::BlockQuote(_) => {
                    out.push_str("</span>");
                    pending_break = true;
                }
                TagEnd::CodeBlock => {
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("</tt>");
                    in_code_block = false;
                    pending_break = true;
                }
                TagEnd::List(_) => {
                    list_depth.pop();
                    if list_depth.is_empty() {
                        pending_break = true;
                    }
                }
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link | TagEnd::Image => {
                    out.push_str("</a>");
                    in_link = false;
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code_block || in_link {
                    out.push_str(&glib_escape(&t));
                } else {
                    linkify(&t, &mut out);
                }
            }
            Event::Code(t) => {
                out.push_str("<tt>");
                out.push_str(&glib_escape(&t));
                out.push_str("</tt>");
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Html(t) | Event::InlineHtml(t) => out.push_str(&glib_escape(&t)),
            Event::TaskListMarker(done) => out.push_str(if done { "☑ " } else { "☐ " }),
            Event::Rule => {
                brk(&mut out, &mut pending_break);
                out.push_str("──────────");
                pending_break = true;
            }
            _ => {}
        }
    }
    out
}

/// Highlights each `@Name` (for the given display names) in already-built
/// Pango markup as a pill. Only text outside `<...>` tags is touched, so
/// link targets and attributes can't be altered. `own` picks colors that
/// read on the blue "own message" bubble.
pub fn highlight_mentions(markup: &str, names: &[String], own: bool, bare: bool) -> String {
    // longest first so "An Nguyen" wins over "An"
    let mut names: Vec<String> = names.iter().filter(|n| !n.is_empty()).map(|n| glib_escape(n)).collect();
    if names.is_empty() {
        return markup.to_string();
    }
    names.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let open = if own {
        "<span background=\"#ffffff\" foreground=\"#0a63c9\" weight=\"bold\">"
    } else {
        "<span background=\"#0a84ff33\" foreground=\"#4da3ff\" weight=\"bold\">"
    };
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::with_capacity(markup.len() + 64);
    let mut rest = markup;
    let mut prev: Option<char> = None;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |i| i + 1);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            prev = None;
            continue;
        }
        // "@Name", or the bare "Name" some clients send for a pill
        let hit = names.iter().find_map(|n| {
            if let Some(r) = rest.strip_prefix('@').and_then(|r| r.strip_prefix(n.as_str())) {
                if !word(r.chars().next()) {
                    return Some(1 + n.len());
                }
            }
            if bare && !word(prev) {
                if let Some(r) = rest.strip_prefix(n.as_str()) {
                    if !word(r.chars().next()) {
                        return Some(n.len());
                    }
                }
            }
            None
        });
        if let Some(len) = hit {
            out.push_str(open);
            out.push_str(&rest[..len]);
            out.push_str("</span>");
            prev = rest[..len].chars().last();
            rest = &rest[len..];
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            prev = Some(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Escape for Pango markup (text and attribute values).
fn glib_escape(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Text with bare `http(s)://` URLs turned into links.
fn linkify(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(i) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&glib_escape(&rest[..i]));
        let tail = &rest[i..];
        let mut end = tail.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"').unwrap_or(tail.len());
        // trailing punctuation belongs to the sentence, not the URL
        while end > 0 && tail[..end].ends_with(['.', ',', ')', ';', ':', '!', '?']) {
            end -= 1;
        }
        let url = &tail[..end];
        out.push_str(&format!("<a href=\"{}\">{}</a>", glib_escape(url), glib_escape(url)));
        rest = &tail[end..];
        if end == 0 {
            out.push_str(&glib_escape(&rest[..1]));
            rest = &rest[1..];
        }
    }
    out.push_str(&glib_escape(rest));
}

#[cfg(test)]
mod tests {
    use super::to_pango;

    #[test]
    fn escapes_and_formats() {
        assert_eq!(to_pango("a <b> **c**"), "a &lt;b&gt; <b>c</b>");
        assert_eq!(to_pango("see https://x.y/z."), "see <a href=\"https://x.y/z\">https://x.y/z</a>.");
        assert_eq!(to_pango("[t](https://a.b)"), "<a href=\"https://a.b\">t</a>");
        assert_eq!(to_pango("`<x>`"), "<tt>&lt;x&gt;</tt>");
        assert_eq!(to_pango("- a\n- b"), "• a\n• b");
    }

    #[test]
    fn markup_is_valid() {
        for src in ["**unclosed", "> quote\n\ntext", "```\ncode <b>\n```", "1. a\n2. b\n\nend", "| a | b |\n|---|---|\n| 1 | 2 |"] {
            let m = to_pango(src);
            assert!(gtk::pango::parse_markup(&m, '\0').is_ok(), "{src:?} -> {m:?}");
        }
    }
}
