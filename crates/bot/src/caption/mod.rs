//! Renders the platform-agnostic caption blocks of `feedbot_core::present`
//! as Telegram HTML within the UTF-16 limits, and actions as buttons.

#[cfg(test)]
mod tests;

use feedbot_core::Extracted;
use feedbot_core::present::{self, Block, CaptionPage, MediaAction, Present, Span};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use teloxide::utils::html;

pub const CAPTION_LIMIT: usize = 1024;
pub const TEXT_LIMIT: usize = 4096;
const CLIP_NOTICE: &str = "简介过长";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedText {
    pub caption: String,
    pub extras: Vec<String>,
}

/// Caption of the send group starting at `page.index`, in the site's layout.
pub fn build_caption_page(
    present: Present,
    extracted: &Extracted,
    page: CaptionPage,
) -> RenderedText {
    render_blocks(present::blocks(present, extracted, page))
}

/// The layout's link buttons (e.g. "作者" / "原帖") for single-media posts.
pub fn media_keyboard(present: Present, extracted: &Extracted) -> Option<InlineKeyboardMarkup> {
    let row: Vec<InlineKeyboardButton> = present::actions(present, extracted)
        .into_iter()
        .map(|action| match action {
            MediaAction::Url { label, href } => InlineKeyboardButton::url(label, href),
        })
        .collect();
    (!row.is_empty()).then(|| InlineKeyboardMarkup::new(vec![row]))
}

fn render_blocks(blocks: Vec<Block>) -> RenderedText {
    let mut budget = CAPTION_LIMIT;
    let mut leftover = String::new();
    let mut caption = String::new();
    let mut pending = blocks.into_iter().peekable();
    let mut first = true;

    while let Some(block) = pending.next() {
        if !first && !try_newline(&mut caption, &mut budget) {
            match &block {
                Block::ExpandableClip(_) => push_leftover(&mut leftover, CLIP_NOTICE),
                _ => push_leftover(&mut leftover, &block.plain_text()),
            }
            for rest in pending {
                match &rest {
                    Block::ExpandableClip(_) => push_leftover(&mut leftover, CLIP_NOTICE),
                    _ => push_leftover(&mut leftover, &rest.plain_text()),
                }
            }
            break;
        }
        first = false;
        match block {
            Block::Line(spans) => push_line(spans, &mut caption, &mut leftover, &mut budget),
            Block::Expandable(text) => {
                push_expandable(&text, &mut caption, &mut leftover, &mut budget, false);
            }
            Block::ExpandableClip(text) => {
                push_expandable(&text, &mut caption, &mut leftover, &mut budget, true);
            }
        }
    }

    RenderedText {
        caption,
        extras: chunk_escaped(&leftover, TEXT_LIMIT),
    }
}

fn push_line(spans: Vec<Span>, caption: &mut String, leftover: &mut String, budget: &mut usize) {
    let mut spans = spans.into_iter();
    while let Some(span) = spans.next() {
        match span {
            Span::Atom(s) => {
                let n = utf16_len(&s);
                if n <= *budget {
                    caption.push_str(&html::escape(&s));
                    *budget -= n;
                } else {
                    push_leftover(leftover, &s);
                    dump_spans(leftover, spans);
                    return;
                }
            }
            Span::Plain(s) => {
                if !push_visible(caption, leftover, budget, &s, html::escape) {
                    dump_spans(leftover, spans);
                    return;
                }
            }
            Span::Link { href, text } => {
                if !push_visible(caption, leftover, budget, &text, |cut| {
                    html::link(&href, cut)
                }) {
                    dump_spans(leftover, spans);
                    return;
                }
            }
        }
    }
}

fn dump_spans(leftover: &mut String, spans: impl Iterator<Item = Span>) {
    for span in spans {
        push_leftover(leftover, span.visible());
    }
}

fn push_expandable(
    text: &str,
    caption: &mut String,
    leftover: &mut String,
    budget: &mut usize,
    clip: bool,
) {
    if text.is_empty() {
        return;
    }
    if *budget == 0 {
        if caption.ends_with('\n') {
            caption.pop();
            *budget += 1;
        }
        if *budget == 0 {
            if clip {
                push_leftover(leftover, CLIP_NOTICE);
            } else {
                push_leftover(leftover, text);
            }
            return;
        }
    }
    let notice_n = utf16_len(CLIP_NOTICE);
    let overflow = utf16_len(text) > *budget;
    let fold_budget = if clip && overflow && *budget > 1 + notice_n {
        *budget - 1 - notice_n
    } else {
        *budget
    };
    let (cut, rest) = take_utf16(text, fold_budget);
    if cut.is_empty() {
        if clip {
            if notice_n <= *budget {
                caption.push_str(&html::escape(CLIP_NOTICE));
                *budget -= notice_n;
            } else {
                push_leftover(leftover, CLIP_NOTICE);
            }
        } else {
            push_leftover(leftover, text);
        }
        return;
    }
    *budget -= utf16_len(cut);
    caption.push_str("<blockquote expandable>");
    caption.push_str(&html::escape(cut));
    caption.push_str("</blockquote>");
    if rest.is_empty() {
        return;
    }
    if clip {
        if try_newline(caption, budget) && notice_n <= *budget {
            caption.push_str(&html::escape(CLIP_NOTICE));
            *budget -= notice_n;
        }
        return;
    }
    push_leftover(leftover, rest);
}

fn push_visible(
    caption: &mut String,
    leftover: &mut String,
    budget: &mut usize,
    visible: &str,
    to_html: impl FnOnce(&str) -> String,
) -> bool {
    if visible.is_empty() {
        return true;
    }
    if *budget == 0 {
        push_leftover(leftover, visible);
        return false;
    }
    let (cut, rest) = take_utf16(visible, *budget);
    *budget = budget.saturating_sub(utf16_len(cut));
    if !cut.is_empty() {
        caption.push_str(&to_html(cut));
    }
    push_leftover(leftover, rest);
    rest.is_empty()
}

fn try_newline(caption: &mut String, budget: &mut usize) -> bool {
    if *budget >= 1 {
        caption.push('\n');
        *budget -= 1;
        true
    } else {
        false
    }
}

fn push_leftover(dst: &mut String, rest: &str) {
    if rest.is_empty() {
        return;
    }
    if !dst.is_empty() {
        dst.push('\n');
    }
    dst.push_str(rest);
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Truncate `s` to at most `max` UTF-16 code units, returning `(kept, rest)`.
pub fn take_utf16(s: &str, max: usize) -> (&str, &str) {
    if max == 0 {
        return ("", s);
    }
    let mut count = 0;
    for (i, ch) in s.char_indices() {
        let w = ch.len_utf16();
        if count + w > max {
            return (&s[..i], &s[i..]);
        }
        count += w;
    }
    (s, "")
}

fn chunk_escaped(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (cut, next) = take_utf16(rest, limit);
        if cut.is_empty() {
            break;
        }
        out.push(html::escape(cut));
        rest = next;
    }
    out
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn emoji_counts_as_two_utf16_units() {
        let (cut, rest) = take_utf16("😀ab", 2);
        assert_eq!(cut, "😀");
        assert_eq!(rest, "ab");
        assert_eq!(utf16_len("😀"), 2);
    }

    #[test]
    fn later_expandable_overflows_to_extras() {
        let rendered = render_blocks(vec![
            Block::Line(vec![Span::plain("T")]),
            Block::Expandable("x".repeat(2000)),
        ]);
        assert!(rendered.caption.contains("<blockquote expandable>"));
        assert!(!rendered.extras.is_empty());
        for extra in &rendered.extras {
            assert!(utf16_len(extra) <= TEXT_LIMIT + 16);
        }
    }

    #[test]
    fn expandable_clip_notes_overflow_in_caption() {
        let rendered = render_blocks(vec![
            Block::Line(vec![Span::plain("T")]),
            Block::ExpandableClip("x".repeat(2000)),
        ]);
        assert!(rendered.caption.contains("<blockquote expandable>"));
        assert!(rendered.caption.contains("简介过长"));
        assert!(rendered.extras.is_empty());
    }
}
