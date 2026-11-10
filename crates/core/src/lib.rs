//! Domain models, extractor/auth/media traits, and the `+`/`-` option parser.

pub mod auth;
pub mod extract;
pub mod job;
pub mod media;
pub mod options;
pub mod present;
pub mod repo;

pub use auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
pub use extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, ExtractorLookup,
    FeedEntry, HttpClient, HttpMethod, HttpRequest, HttpResponse, MediaItem, MediaKind,
    MediaSource, RequesterInfo, Scope, SiteMeta, UgoiraFrame,
};
pub use job::{Job, JobState, MediaCacheEntry, PublishError, Publisher, Subscription, WorkEnqueue};
pub use media::{MediaError, MediaInfo, MediaProcessor, TranscodeSpec, Ytdlp, YtdlpSpec};
pub use options::{
    OptionDef, OptionDefault, OptionError, OptionKind, OptionSpec, OptionValue, OptionValues,
    apply_to_layer, fold_fullwidth, merge_layers, parse_tokens, split_option_tokens,
};
pub use present::{Caption, CaptionPage, MediaAction, Present};
pub use repo::{
    CachedPage, CredentialRepo, JobRepo, MediaCacheRepo, SeenRepo, SettingsRepo, Store, StoreError,
    StoredCredentialMeta, SubscriptionRepo, load_saved_options,
};
