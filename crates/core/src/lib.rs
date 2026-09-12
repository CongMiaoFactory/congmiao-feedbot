//! Domain models, extractor/auth/media traits, and the `+`/`-` option parser.

pub mod auth;
pub mod extract;
pub mod job;
pub mod media;
pub mod options;
pub mod repo;

pub use auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
pub use extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, HttpClient,
    HttpMethod, HttpRequest, HttpResponse, ManifestKind, MediaItem, MediaKind, MediaSource,
    RequesterInfo, Scope, UgoiraFrame,
};
pub use job::{
    ChatSettings, Job, JobState, MediaCacheEntry, PublishError, Publisher, Subscription,
    WorkEnqueue,
};
pub use media::{MediaError, MediaInfo, MediaProcessor, TranscodeSpec, Ytdlp, YtdlpSpec};
pub use options::{
    fold_fullwidth, merge_layers, parse_tokens, split_option_tokens, OptionDef, OptionDefault,
    OptionError, OptionKind, OptionSpec, OptionValue, OptionValues,
};
pub use repo::{
    load_saved_options, CredentialRepo, JobRepo, MediaCacheRepo, SeenRepo, SettingsRepo, Store,
    StoreError, StoredCredentialMeta, SubscriptionRepo,
};
