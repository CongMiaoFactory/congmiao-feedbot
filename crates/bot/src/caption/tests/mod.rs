//! Rendered captions per site. A sample's `extractor_id` picks the layout the
//! registered site chose ([`feedbot_core::Extractor::present`]), so these
//! tests also pin each site's choice.

mod clip;
mod default;
mod gallery;
mod post;

use super::RenderedText;
use feedbot_core::{CaptionPage, Extracted, Present};
use feedbot_extractors::ExtractorRegistry;
use teloxide::types::InlineKeyboardMarkup;

fn present_of(extracted: &Extracted) -> Present {
    ExtractorRegistry::builtin()
        .by_id(&extracted.extractor_id)
        .map(|e| e.present())
        .unwrap_or(Present::Default)
}

fn build_caption(extracted: &Extracted) -> RenderedText {
    build_caption_page(extracted, CaptionPage::single(extracted.items.len()))
}

fn build_caption_page(extracted: &Extracted, page: CaptionPage) -> RenderedText {
    super::build_caption_page(present_of(extracted), extracted, page)
}

fn media_keyboard(extracted: &Extracted) -> Option<InlineKeyboardMarkup> {
    super::media_keyboard(present_of(extracted), extracted)
}
