use feedbot_core::Engagement;

pub fn stats_line(engagement: Option<&Engagement>) -> Option<String> {
    let e = engagement?;
    let mut parts = Vec::new();
    if let Some(n) = e.views {
        parts.push(format!("浏览 {}", format_count(n)));
    }
    if let Some(n) = e.likes {
        parts.push(format!("点赞 {}", format_count(n)));
    }
    if let Some(n) = e.shares {
        parts.push(format!("分享 {}", format_count(n)));
    }
    if let Some(n) = e.comments {
        parts.push(format!("评论 {}", format_count(n)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

pub fn format_count(n: u64) -> String {
    if n >= 100_000_000 {
        one_decimal(n as f64 / 100_000_000.0, "亿")
    } else if n >= 10_000 {
        one_decimal(n as f64 / 10_000.0, "万")
    } else {
        n.to_string()
    }
}

fn one_decimal(v: f64, unit: &str) -> String {
    let s = format!("{v:.1}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_count_uses_wan_and_yi() {
        assert_eq!(format_count(320), "320");
        assert_eq!(format_count(10_000), "1万");
        assert_eq!(format_count(12_345), "1.2万");
        assert_eq!(format_count(100_000_000), "1亿");
    }

    #[test]
    fn stats_omit_missing_and_do_not_invent_zero() {
        assert_eq!(
            stats_line(Some(&Engagement {
                views: Some(12_345),
                likes: Some(320),
                shares: None,
                comments: None,
            })),
            Some("浏览 1.2万 · 点赞 320".into())
        );
        assert_eq!(
            stats_line(Some(&Engagement {
                views: None,
                likes: Some(7),
                shares: None,
                comments: None,
            })),
            Some("点赞 7".into())
        );
        assert!(stats_line(None).is_none());
        assert!(stats_line(Some(&Engagement::default())).is_none());
    }
}
