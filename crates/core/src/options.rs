//! `+`/`-` prefix option grammar, declarative specs, and three-layer merge.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::{Display, Formatter};
use thiserror::Error;

/// A platform's declared option table. Platforms only declare; they do not parse.
#[derive(Debug, Clone, Copy)]
pub struct OptionSpec {
    pub options: &'static [OptionDef],
}

impl OptionSpec {
    pub const EMPTY: Self = Self { options: &[] };

    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        let key = name.to_ascii_lowercase();
        self.options.iter().find(|def| {
            def.name.eq_ignore_ascii_case(&key)
                || def
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(&key))
        })
    }

    pub fn help_table(&self) -> String {
        if self.options.is_empty() {
            return "（此平台没有可配置参数）".to_string();
        }
        let mut lines = Vec::new();
        for def in self.options {
            let aliases = if def.aliases.is_empty() {
                String::new()
            } else {
                def.aliases
                    .iter()
                    .map(|alias| format!(" / <code>+{}</code>", html_escape(alias)))
                    .collect::<String>()
            };
            lines.push(format!(
                "<code>+{name}</code>{aliases}  <i>{kind}</i>\n{desc}",
                name = html_escape(def.name),
                kind = html_escape(&def.kind.help_value_hint()),
                desc = html_escape(def.desc),
            ));
        }
        lines.join("\n")
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Debug, Clone, Copy)]
pub struct OptionDef {
    /// Short name used as the canonical key, e.g. `"sp"`.
    pub name: &'static str,
    /// Long aliases, e.g. `["subtitle"]`.
    pub aliases: &'static [&'static str],
    pub kind: OptionKind,
    pub default: Option<OptionDefault>,
    /// Whether this option changes the produced media (must enter cache key).
    pub affects_output: bool,
    pub desc: &'static str,
}

/// Const-friendly default stored on [`OptionDef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionDefault {
    Flag(bool),
    Int(i64),
    Str(&'static str),
    Enum(&'static str),
    IntRange { start: i64, end: i64 },
}

impl OptionDefault {
    pub fn to_value(self) -> OptionValue {
        match self {
            Self::Flag(v) => OptionValue::Flag(v),
            Self::Int(v) => OptionValue::Int(v),
            Self::Str(v) => OptionValue::Str(v.to_string()),
            Self::Enum(v) => OptionValue::Enum(v.to_string()),
            Self::IntRange { start, end } => OptionValue::IntRange { start, end },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum OptionKind {
    Flag,
    Int {
        min: i64,
        max: i64,
    },
    Str {
        max_len: usize,
    },
    Enum {
        variants: &'static [&'static str],
    },
    IntRange {
        min: i64,
        max: i64,
    },
    List {
        of: &'static OptionKind,
        max_len: usize,
    },
}

impl OptionKind {
    pub fn help_hint(&self, name: &str) -> String {
        match self {
            Self::Flag => format!("+{name} / -{name}"),
            Self::Int { min, max } => format!("+{name}:<{min}-{max}>"),
            Self::Str { .. } => format!("+{name}:<text>"),
            Self::Enum { variants } => format!("+{name}:{}", variants.join("|")),
            Self::IntRange { min, max } => format!("+{name}:{min}-{max}"),
            Self::List { of, .. } => format!("+{name}:a,b  ({})", of.help_hint(name)),
        }
    }

    pub fn help_value_hint(&self) -> String {
        match self {
            Self::Flag => "开关".into(),
            Self::Int { min, max } => format!("{min}–{max}"),
            Self::Str { .. } => "文本".into(),
            Self::Enum { variants } => variants.join(" | "),
            Self::IntRange { min, max } => format!("{min}-{max}"),
            Self::List { of, .. } => format!("{},…", of.help_value_hint()),
        }
    }

    fn is_flag(&self) -> bool {
        matches!(self, Self::Flag)
    }

    fn is_list(&self) -> bool {
        matches!(self, Self::List { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionValue {
    Flag(bool),
    Int(i64),
    Str(String),
    Enum(String),
    IntRange { start: i64, end: i64 },
    List(Vec<OptionValue>),
}

impl Display for OptionValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Str(v) | Self::Enum(v) => write!(f, "{v}"),
            Self::IntRange { start, end } => write!(f, "{start}-{end}"),
            Self::List(items) => {
                let s: Vec<String> = items.iter().map(|i| i.to_string()).collect();
                write!(f, "{}", s.join(","))
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionValues {
    inner: IndexMap<String, OptionValue>,
}

impl OptionValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&OptionValue> {
        self.inner.get(name)
    }

    pub fn flag(&self, name: &str) -> bool {
        matches!(self.inner.get(name), Some(OptionValue::Flag(true)))
    }

    pub fn insert(&mut self, name: impl Into<String>, value: OptionValue) {
        self.inner.insert(name.into(), value);
    }

    pub fn remove(&mut self, name: &str) -> Option<OptionValue> {
        self.inner.shift_remove(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &OptionValue)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Stable fingerprint of options that affect produced media, for cache keys.
    pub fn fingerprint(&self, spec: &OptionSpec) -> String {
        let mut parts: Vec<(String, String)> = Vec::new();
        for def in spec.options {
            if !def.affects_output {
                continue;
            }
            if let Some(value) = self.inner.get(def.name) {
                parts.push((def.name.to_string(), value.to_string()));
            }
        }
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        let payload = parts
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let digest = Sha256::digest(payload.as_bytes());
        hex::encode(digest)
    }

    pub fn cache_key(url: &str, spec: &OptionSpec, values: &Self) -> String {
        format!("{url}#{}", values.fingerprint(spec))
    }
}

#[derive(Debug, Error)]
pub enum OptionError {
    #[error("未知参数 `{token}`{hint}")]
    Unknown { token: String, hint: String },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    CrossConstraint(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Set {
        name: String,
        value: OptionValue,
    },
    Clear {
        name: String,
    },
    ListAdd {
        name: String,
        items: Vec<OptionValue>,
    },
    ListRemove {
        name: String,
        items: Vec<OptionValue>,
    },
}

/// Fold fullwidth / smart-punctuation lookalikes before tokenization.
pub fn fold_fullwidth(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            '＋' => '+',
            '－' | '–' | '—' | '−' | '﹣' => '-',
            '：' => ':',
            '，' => ',',
            '＝' => '=',
            '　' => ' ',
            other => other,
        })
        .collect()
}

/// Split a message (already URL-stripped) into option tokens plus leftovers.
pub fn split_option_tokens(raw: &str) -> (Vec<String>, Vec<String>) {
    let folded = fold_fullwidth(raw);
    let mut opts = Vec::new();
    let mut leftover = Vec::new();
    for token in folded.split_whitespace() {
        if token.starts_with('+') || token.starts_with('-') {
            opts.push(token.to_string());
        } else {
            leftover.push(token.to_string());
        }
    }
    (opts, leftover)
}

/// Parse option tokens against a spec. Does not merge defaults.
pub fn parse_tokens(spec: &OptionSpec, tokens: &[String]) -> Result<Vec<ParsedToken>, OptionError> {
    let mut parsed = Vec::new();
    for token in tokens {
        parsed.push(parse_one(spec, token)?);
    }
    Ok(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToken {
    action: Action,
}

fn parse_one(spec: &OptionSpec, token: &str) -> Result<ParsedToken, OptionError> {
    if token.len() < 2 {
        return Err(unknown(spec, token));
    }
    let sign = token.as_bytes()[0] as char;
    if sign != '+' && sign != '-' {
        return Err(unknown(spec, token));
    }
    let rest = &token[1..];
    let (name_raw, value) = match rest.find([':', '=']) {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    if name_raw.is_empty()
        || !name_raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        || !name_raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(unknown(spec, token));
    }
    let def = spec.get(name_raw).ok_or_else(|| unknown(spec, token))?;
    let name = def.name.to_string();

    let action = match (sign, value) {
        ('+', None) => {
            if !def.kind.is_flag() {
                return Err(OptionError::Invalid(format!(
                    "参数 `{name}` 需要取值，例如 `+{name}:1`"
                )));
            }
            Action::Set {
                name,
                value: OptionValue::Flag(true),
            }
        }
        ('+', Some(raw)) => {
            if def.kind.is_flag() {
                return Err(OptionError::Invalid(format!(
                    "开关参数 `{name}` 不能带值，请使用 `+{name}` 或 `-{name}`"
                )));
            }
            if def.kind.is_list() {
                let items = parse_list(&def.kind, raw)?;
                Action::ListAdd { name, items }
            } else {
                Action::Set {
                    value: parse_value(&def.kind, raw)?,
                    name,
                }
            }
        }
        ('-', None) => {
            if def.kind.is_flag() {
                Action::Set {
                    name,
                    value: OptionValue::Flag(false),
                }
            } else {
                Action::Clear { name }
            }
        }
        ('-', Some(raw)) => {
            if !def.kind.is_list() {
                return Err(OptionError::Invalid(format!(
                    "参数 `{name}` 不能用 `-{name}:{raw}` 清除单项，请改用 `-{name}`"
                )));
            }
            let items = parse_list(&def.kind, raw)?;
            Action::ListRemove { name, items }
        }
        _ => unreachable!(),
    };
    Ok(ParsedToken { action })
}

fn parse_list(kind: &OptionKind, raw: &str) -> Result<Vec<OptionValue>, OptionError> {
    let OptionKind::List { of, max_len } = kind else {
        unreachable!("parse_list called on non-list");
    };
    let items: Vec<&str> = raw.split(',').filter(|s| !s.is_empty()).collect();
    if items.len() > *max_len {
        return Err(OptionError::Invalid(format!(
            "列表最多 {max_len} 项，实际 {}",
            items.len()
        )));
    }
    items
        .into_iter()
        .map(|item| parse_value(of, item))
        .collect()
}

fn parse_value(kind: &OptionKind, raw: &str) -> Result<OptionValue, OptionError> {
    if raw.is_empty() {
        return Err(OptionError::Invalid("参数值不能为空".into()));
    }
    match kind {
        OptionKind::Flag => Err(OptionError::Invalid("开关参数不能带值".into())),
        OptionKind::Int { min, max } => {
            let n: i64 = raw
                .parse()
                .map_err(|_| OptionError::Invalid(format!("`{raw}` 不是整数")))?;
            if n < *min || n > *max {
                return Err(OptionError::Invalid(format!("`{n}` 超出范围 {min}-{max}")));
            }
            Ok(OptionValue::Int(n))
        }
        OptionKind::Str { max_len } => {
            if raw.len() > *max_len {
                return Err(OptionError::Invalid(format!("文本最长 {max_len} 字符")));
            }
            Ok(OptionValue::Str(raw.to_string()))
        }
        OptionKind::Enum { variants } => {
            let found = variants
                .iter()
                .find(|v| v.eq_ignore_ascii_case(raw))
                .ok_or_else(|| {
                    OptionError::Invalid(format!(
                        "`{raw}` 不是合法取值，可选：{}",
                        variants.join(" / ")
                    ))
                })?;
            Ok(OptionValue::Enum((*found).to_string()))
        }
        OptionKind::IntRange { min, max } => {
            let (start, end) = parse_range(raw)?;
            if start < *min || end > *max || start > end {
                return Err(OptionError::Invalid(format!(
                    "区间 `{start}-{end}` 不在 {min}-{max} 内"
                )));
            }
            Ok(OptionValue::IntRange { start, end })
        }
        OptionKind::List { .. } => parse_list(kind, raw).map(OptionValue::List),
    }
}

fn parse_range(raw: &str) -> Result<(i64, i64), OptionError> {
    let Some((a, b)) = raw.split_once('-') else {
        return Err(OptionError::Invalid(format!(
            "`{raw}` 不是区间，例如 `1-5`"
        )));
    };
    let start: i64 = a
        .parse()
        .map_err(|_| OptionError::Invalid(format!("`{a}` 不是整数")))?;
    let end: i64 = b
        .parse()
        .map_err(|_| OptionError::Invalid(format!("`{b}` 不是整数")))?;
    Ok((start, end))
}

fn unknown(spec: &OptionSpec, token: &str) -> OptionError {
    let needle = token
        .trim_start_matches(['+', '-'])
        .split([':', '='])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    let mut best: Option<(&OptionDef, f64)> = None;
    for def in spec.options {
        let score = strsim::jaro_winkler(&needle, &def.name.to_ascii_lowercase());
        let better = best.map(|(_, s)| score > s).unwrap_or(true);
        if better && score >= 0.75 {
            best = Some((def, score));
        }
        for alias in def.aliases {
            let s = strsim::jaro_winkler(&needle, &alias.to_ascii_lowercase());
            let better = best.map(|(_, prev)| s > prev).unwrap_or(true);
            if better && s >= 0.75 {
                best = Some((def, s));
            }
        }
    }
    let hint = if let Some((def, _)) = best {
        let alias = def
            .aliases
            .first()
            .map(|a| format!("（{a}）"))
            .unwrap_or_default();
        let extra = match def.kind {
            OptionKind::Enum { variants } => format!(" 可选值：{}", variants.join(" / ")),
            _ => String::new(),
        };
        format!("，你是不是想输入 `+{}`{alias}？{extra}", def.name)
    } else {
        String::new()
    };
    OptionError::Unknown {
        token: token.to_string(),
        hint,
    }
}

/// Apply parsed inline tokens onto a base map (usually saved defaults).
pub fn apply_actions(
    spec: &OptionSpec,
    base: &OptionValues,
    tokens: &[ParsedToken],
) -> OptionValues {
    let mut out = base.clone();
    for token in tokens {
        match &token.action {
            Action::Set { name, value } => {
                out.insert(name, value.clone());
            }
            Action::Clear { name } => {
                out.remove(name);
                if let Some(def) = spec.get(name) {
                    if let Some(default) = def.default {
                        out.insert(name, default.to_value());
                    }
                }
            }
            Action::ListAdd { name, items } => match out.inner.get_mut(name) {
                Some(OptionValue::List(existing)) => {
                    for item in items {
                        if !existing.contains(item) {
                            existing.push(item.clone());
                        }
                    }
                }
                _ => out.insert(name, OptionValue::List(items.clone())),
            },
            Action::ListRemove { name, items } => {
                if let Some(OptionValue::List(existing)) = out.inner.get_mut(name) {
                    existing.retain(|v| !items.contains(v));
                }
            }
        }
    }
    out
}

/// Three-layer merge: spec defaults < saved settings < inline message tokens.
pub fn merge_layers(
    spec: &OptionSpec,
    saved: &OptionValues,
    inline_tokens: &[String],
) -> Result<OptionValues, OptionError> {
    let mut base = OptionValues::new();
    for def in spec.options {
        if let Some(default) = def.default {
            base.insert(def.name, default.to_value());
        }
    }
    for (k, v) in saved.iter() {
        base.insert(k, v.clone());
    }
    let parsed = parse_tokens(spec, inline_tokens)?;
    Ok(apply_actions(spec, &base, &parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INT_KIND: OptionKind = OptionKind::Int { min: 1, max: 99 };

    const SPEC: OptionSpec = OptionSpec {
        options: &[
            OptionDef {
                name: "sp",
                aliases: &["subtitle"],
                kind: OptionKind::Flag,
                default: Some(OptionDefault::Flag(false)),
                affects_output: true,
                desc: "字幕",
            },
            OptionDef {
                name: "q",
                aliases: &["quality"],
                kind: OptionKind::Enum {
                    variants: &["720p", "1080p", "max"],
                },
                default: Some(OptionDefault::Enum("1080p")),
                affects_output: true,
                desc: "画质",
            },
            OptionDef {
                name: "p",
                aliases: &["page"],
                kind: OptionKind::Int { min: 1, max: 99 },
                default: Some(OptionDefault::Int(1)),
                affects_output: true,
                desc: "页码",
            },
            OptionDef {
                name: "r",
                aliases: &[],
                kind: OptionKind::IntRange { min: 1, max: 20 },
                default: None,
                affects_output: true,
                desc: "区间",
            },
            OptionDef {
                name: "ids",
                aliases: &[],
                kind: OptionKind::List {
                    of: &INT_KIND,
                    max_len: 8,
                },
                default: None,
                affects_output: true,
                desc: "多值",
            },
        ],
    };

    fn parse_merge(raw: &str, saved: &OptionValues) -> OptionValues {
        let (tokens, leftover) = split_option_tokens(raw);
        assert!(leftover.is_empty(), "leftover: {leftover:?}");
        merge_layers(&SPEC, saved, &tokens).unwrap()
    }

    #[test]
    fn plus_flag_and_minus_flag() {
        let v = parse_merge("+sp", &OptionValues::new());
        assert!(v.flag("sp"));
        let v = parse_merge("-sp", &OptionValues::new());
        assert_eq!(v.get("sp"), Some(&OptionValue::Flag(false)));
    }

    #[test]
    fn plus_int_value() {
        let v = parse_merge("+p:1", &OptionValues::new());
        assert_eq!(v.get("p"), Some(&OptionValue::Int(1)));
    }

    #[test]
    fn plus_range_and_list() {
        let v = parse_merge("+r:1-5 +ids:1,3,5", &OptionValues::new());
        assert_eq!(
            v.get("r"),
            Some(&OptionValue::IntRange { start: 1, end: 5 })
        );
        assert_eq!(
            v.get("ids"),
            Some(&OptionValue::List(vec![
                OptionValue::Int(1),
                OptionValue::Int(3),
                OptionValue::Int(5),
            ]))
        );
    }

    #[test]
    fn minus_clears_to_spec_default() {
        let mut saved = OptionValues::new();
        saved.insert("p", OptionValue::Int(9));
        let v = parse_merge("-p", &saved);
        assert_eq!(v.get("p"), Some(&OptionValue::Int(1)));
    }

    #[test]
    fn fullwidth_fold() {
        let (tokens, _) = split_option_tokens("＋sp +p：2");
        assert_eq!(tokens, vec!["+sp".to_string(), "+p:2".to_string()]);
        let v = merge_layers(&SPEC, &OptionValues::new(), &tokens).unwrap();
        assert!(v.flag("sp"));
        assert_eq!(v.get("p"), Some(&OptionValue::Int(2)));
    }

    #[test]
    fn missing_value_and_extra_value_error() {
        let (tokens, _) = split_option_tokens("+p");
        let err = merge_layers(&SPEC, &OptionValues::new(), &tokens).unwrap_err();
        assert!(err.to_string().contains("需要取值"));
        let (tokens, _) = split_option_tokens("+sp:1");
        let err = merge_layers(&SPEC, &OptionValues::new(), &tokens).unwrap_err();
        assert!(err.to_string().contains("不能带值"));
    }

    #[test]
    fn unknown_with_suggestion() {
        let (tokens, _) = split_option_tokens("+q1");
        let err = merge_layers(&SPEC, &OptionValues::new(), &tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("+q"), "{msg}");
    }

    #[test]
    fn three_layer_priority() {
        let mut saved = OptionValues::new();
        saved.insert("q", OptionValue::Enum("720p".into()));
        saved.insert("sp", OptionValue::Flag(true));
        let v = parse_merge("+q:max -sp", &saved);
        assert_eq!(v.get("q"), Some(&OptionValue::Enum("max".into())));
        assert_eq!(v.get("sp"), Some(&OptionValue::Flag(false)));
    }

    #[test]
    fn alias_and_fingerprint_differs_by_quality() {
        let a = parse_merge("+subtitle +quality:720p", &OptionValues::new());
        let b = parse_merge("+sp +q:1080p", &OptionValues::new());
        assert!(a.flag("sp"));
        assert_ne!(a.fingerprint(&SPEC), b.fingerprint(&SPEC));
    }

    #[test]
    fn leftover_tokens_are_separated() {
        let (opts, leftover) = split_option_tokens("hello +sp world");
        assert_eq!(opts, vec!["+sp".to_string()]);
        assert_eq!(leftover, vec!["hello".to_string(), "world".to_string()]);
    }
}
