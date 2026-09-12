//! Platform-agnostic caption pieces. Sites emit these; HTML stays in the layout engine.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// Visible text that may be truncated to fit the caption budget.
    Plain(String),
    /// Visible text that is kept only if the whole string fits.
    Atom(String),
    Link {
        href: String,
        text: String,
    },
}

impl Span {
    pub fn plain(s: impl Into<String>) -> Self {
        Self::Plain(s.into())
    }

    pub fn atom(s: impl Into<String>) -> Self {
        Self::Atom(s.into())
    }

    pub fn link(href: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Link {
            href: href.into(),
            text: text.into(),
        }
    }

    pub fn visible(&self) -> &str {
        match self {
            Self::Plain(s) | Self::Atom(s) => s,
            Self::Link { text, .. } => text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Line(Vec<Span>),
    Expandable(String),
    /// Like [`Self::Expandable`], but leftover text is dropped and the caption
    /// gets a `简介过长` note instead of a follow-up message.
    ExpandableClip(String),
}

impl Block {
    pub fn plain_text(&self) -> String {
        match self {
            Self::Line(spans) => spans.iter().map(Span::visible).collect::<Vec<_>>().join(""),
            Self::Expandable(s) | Self::ExpandableClip(s) => s.clone(),
        }
    }
}
