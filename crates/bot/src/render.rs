//! Compatibility facade: caption layout lives in [`crate::caption`].

pub use crate::caption::{
    build_caption, build_caption_page, media_keyboard, take_utf16, utf16_len, CaptionPage,
    RenderedText, CAPTION_LIMIT, TEXT_LIMIT,
};
