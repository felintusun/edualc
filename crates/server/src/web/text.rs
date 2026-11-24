//! Text as the page shows it: plain, or inline Markdown made HTML, the text's own HTML shown as
//! text, never taken as markup.

use dioxus::prelude::*;
use edualc_core::present::Styled;
use edualc_core::present::model::{BlockKind, CjkChars, MdBlock};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};

/// Inline Markdown as HTML, safe to set as a node's inner HTML.
pub fn markdown_html(source: &str) -> String {
    let mut links_open = Vec::new();
    let events = Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => match html.trim() {
            "<u>" | "</u>" => Event::InlineHtml(html),
            _ => Event::Text(html),
        },
        Event::Start(Tag::Link { dest_url, .. }) => {
            let mut href = String::new();
            // A link that may not open, or whose address didn't escape, shows as text.
            let opens = edualc_core::desktop::is_web_link(&dest_url)
                && pulldown_cmark_escape::escape_href(&mut href, &dest_url).is_ok();
            links_open.push(opens);
            if opens {
                Event::InlineHtml(CowStr::from(format!(
                    "<a href=\"{href}\" target=\"_blank\" rel=\"noopener noreferrer\">"
                )))
            } else {
                Event::InlineHtml(CowStr::from("<span class=\"link\">"))
            }
        }
        Event::End(TagEnd::Link) => {
            Event::InlineHtml(CowStr::from(if links_open.pop().unwrap_or(false) {
                "</a>"
            } else {
                "</span>"
            }))
        }
        // Images and the like aren't fetched: their text stands for them.
        Event::Start(Tag::Image { .. }) => Event::Text(CowStr::from("")),
        other @ (Event::Start(_)
        | Event::End(_)
        | Event::Text(_)
        | Event::Code(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::FootnoteReference(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::Rule
        | Event::TaskListMarker(_)) => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

/// The `lang` attribute a text's CJK characters ask for, so the browser picks their glyphs.
pub fn lang(cjk: &CjkChars) -> &'static str {
    use edualc_core::present::model::Cjk;
    match cjk.language {
        Cjk::None => "",
        Cjk::Japanese => "ja",
        Cjk::SimplifiedChinese => "zh-Hans",
        Cjk::TraditionalChinese => "zh-Hant",
        Cjk::Korean => "ko",
    }
}

/// Styled text: plain (whitespace kept), or Markdown.
#[component]
pub fn Text(text: Styled, #[props(default)] class: String) -> Element {
    match text {
        Styled::Empty => rsx! {},
        Styled::Plain(text) => rsx! {
            div { class: "plain {class}", "{text}" }
        },
        Styled::Markdown(source) => rsx! {
            div { class: "markdown {class}", dangerous_inner_html: markdown_html(&source) }
        },
    }
}

/// A Markdown block of a message, a plan or a side answer.
#[component]
pub fn Block(block: MdBlock) -> Element {
    let lang = lang(&block.cjk);
    match block.kind {
        BlockKind::Text => rsx! {
            div { class: "block text", lang, Text { text: block.body } }
        },
        BlockKind::Heading => {
            let level = block.level.clamp(1, 6);
            rsx! {
                div { class: "block heading h{level}", lang, role: "heading", "aria-level": "{level}",
                    Text { text: block.body }
                }
            }
        }
        BlockKind::Code => rsx! {
            div { class: "block code",
                if !block.lang.is_empty() {
                    div { class: "code-lang", "{block.lang}" }
                }
                button {
                    class: "copy",
                    title: "Copy",
                    onclick: move |_| super::platform::copy(&block.raw),
                    "⧉"
                }
                pre { code { "{block.raw}" } }
            }
        },
        BlockKind::Quote => rsx! {
            blockquote { class: "block quote", lang, Text { text: block.body } }
        },
        BlockKind::Table => rsx! {
            div { class: "block table", lang,
                table {
                    for (index, row) in block.rows.iter().enumerate() {
                        tr {
                            for cell in row.cells.iter() {
                                if index == 0 {
                                    th { Text { text: cell.clone() } }
                                } else {
                                    td { Text { text: cell.clone() } }
                                }
                            }
                        }
                    }
                }
            }
        },
        BlockKind::Rule => rsx! {
            hr { class: "block rule" }
        },
    }
}

/// A message's blocks.
#[component]
pub fn Blocks(blocks: Vec<MdBlock>) -> Element {
    rsx! {
        div { class: "blocks",
            for block in blocks {
                Block { block }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_made_safe_html() {
        let html = markdown_html("a <script>alert(1)</script> and *this*");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("<em>this</em>"), "{html}");
        let html = markdown_html("[ok](https://example.com) [no](javascript:alert(1))");
        assert!(
            html.contains("<a href=\"https://example.com\" target=\"_blank\""),
            "{html}"
        );
        assert!(!html.contains("javascript:"), "{html}");
        let html = markdown_html(r#"[q](<https://example.com/"onmouseover=alert(1)>)"#);
        assert!(!html.contains("\"onmouseover"), "{html}");
    }
}
