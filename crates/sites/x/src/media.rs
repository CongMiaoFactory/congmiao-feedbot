//! Media URLs: photo size rewriting, best MP4 variant, file names.

use super::Tweet;
use super::syndication::VideoVariant;
use feedbot_core::extract::{ExtractError, MediaItem, MediaKind, MediaSource};
use http::HeaderMap;
use url::Url;

use feedbot_site_common::last_path_segment;

pub(super) fn media_items(tweet: &Tweet, id: u64, orig: bool) -> Vec<MediaItem> {
    tweet
        .media
        .iter()
        .enumerate()
        .filter_map(|(idx, raw)| {
            let url = match raw.kind {
                MediaKind::Photo => rewrite_photo_url(&raw.url, orig).ok()?,
                _ => Url::parse(&raw.url).ok()?,
            };
            let filename = filename_from_url(&url);
            Some(MediaItem {
                id: format!("{id}_p{idx}"),
                kind: raw.kind,
                source: MediaSource::Direct {
                    url,
                    headers: HeaderMap::new(),
                },
                filename,
                cover: None,
            })
        })
        .collect()
}

pub(super) fn rewrite_photo_url(raw: &str, orig: bool) -> Result<Url, ExtractError> {
    let size = if orig { "orig" } else { "large" };
    let mut url = Url::parse(raw).map_err(|e| ExtractError::Failed(e.to_string()))?;
    strip_size_suffix(&mut url);
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.iter().any(|(k, _)| k == "name" || k == "format") {
        let mut next = Vec::new();
        let mut saw_name = false;
        for (k, v) in pairs {
            if k == "name" {
                next.push((k, size.to_string()));
                saw_name = true;
            } else {
                next.push((k, v));
            }
        }
        if !saw_name {
            next.push(("name".into(), size.to_string()));
        }
        let encoded = next
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&encoded));
    } else {
        let mut path = url.path().to_string();
        path.push(':');
        path.push_str(size);
        url.set_path(&path);
    }
    Ok(url)
}

pub(super) fn strip_size_suffix(url: &mut Url) {
    let Some(last) = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(str::to_string)
    else {
        return;
    };
    if let Some((base, suffix)) = last.rsplit_once(':')
        && (matches!(
            suffix,
            "orig" | "large" | "medium" | "small" | "thumb" | "orig.jpg" | "large.jpg"
        ) || suffix.chars().all(|c| c.is_ascii_alphabetic()))
    {
        let mut segs: Vec<String> = url.path_segments().unwrap().map(str::to_string).collect();
        if let Some(last) = segs.last_mut() {
            *last = base.to_string();
        }
        let path = format!("/{}", segs.join("/"));
        url.set_path(&path);
    }
}

pub(super) fn filename_from_url(url: &Url) -> Option<String> {
    last_path_segment(url).map(|s| s.split(':').next().unwrap_or(&s).to_string())
}

pub(super) fn pick_mp4(variants: &[VideoVariant]) -> Option<String> {
    variants
        .iter()
        .filter(|v| {
            v.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("video/mp4"))
                && v.url.as_deref().is_some_and(|u| !u.is_empty())
        })
        .max_by_key(|v| v.bitrate.unwrap_or(0))
        .and_then(|v| v.url.clone())
}
