//! Pure album planner: type runs, Telegram 2–10 album limit, even splits.

use feedbot_core::MediaKind;

pub const TELEGRAM_ALBUM_MAX: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupClass {
    PhotoVideo,
    Audio,
    Document,
    Solo,
}

impl GroupClass {
    pub fn of(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Photo | MediaKind::Video => Self::PhotoVideo,
            MediaKind::Audio => Self::Audio,
            MediaKind::Document => Self::Document,
            MediaKind::Animation => Self::Solo,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendPlan {
    Single(usize),
    Album(Vec<usize>),
}

impl SendPlan {
    pub fn indices(&self) -> Vec<usize> {
        match self {
            Self::Single(i) => vec![*i],
            Self::Album(v) => v.clone(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Album(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Single(_) => false,
            Self::Album(v) => v.is_empty(),
        }
    }
}

pub fn plan_groups(kinds: &[MediaKind]) -> Vec<SendPlan> {
    plan_groups_with_max(kinds, TELEGRAM_ALBUM_MAX)
}

pub fn plan_groups_with_max(kinds: &[MediaKind], album_max: usize) -> Vec<SendPlan> {
    let album_max = album_max.clamp(2, TELEGRAM_ALBUM_MAX);
    let mut plans = Vec::new();
    let mut i = 0;
    while i < kinds.len() {
        let class = GroupClass::of(kinds[i]);
        if class == GroupClass::Solo {
            plans.push(SendPlan::Single(i));
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < kinds.len() && GroupClass::of(kinds[i]) == class {
            i += 1;
        }
        plans.extend(chunk_run((start..i).collect(), album_max));
    }
    plans
}

fn chunk_run(run: Vec<usize>, album_max: usize) -> Vec<SendPlan> {
    let n = run.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![SendPlan::Single(run[0])];
    }
    if n <= album_max {
        return vec![SendPlan::Album(run)];
    }
    let groups = n.div_ceil(album_max);
    let base = n / groups;
    let extra = n % groups;
    let mut out = Vec::with_capacity(groups);
    let mut offset = 0;
    for g in 0..groups {
        let size = if g < extra { base + 1 } else { base };
        let slice = run[offset..offset + size].to_vec();
        offset += size;
        if slice.len() == 1 {
            out.push(SendPlan::Single(slice[0]));
        } else {
            out.push(SendPlan::Album(slice));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photos(n: usize) -> Vec<MediaKind> {
        vec![MediaKind::Photo; n]
    }

    fn sizes(plans: &[SendPlan]) -> Vec<usize> {
        plans.iter().map(SendPlan::len).collect()
    }

    #[test]
    fn n1_is_single() {
        let plans = plan_groups(&photos(1));
        assert_eq!(plans, vec![SendPlan::Single(0)]);
    }

    #[test]
    fn n2_is_one_album() {
        let plans = plan_groups(&photos(2));
        assert_eq!(plans, vec![SendPlan::Album(vec![0, 1])]);
    }

    #[test]
    fn n10_is_one_album() {
        let plans = plan_groups(&photos(10));
        assert_eq!(plans, vec![SendPlan::Album((0..10).collect())]);
        assert_eq!(sizes(&plans), vec![10]);
    }

    #[test]
    fn n11_splits_6_5() {
        let plans = plan_groups(&photos(11));
        assert_eq!(sizes(&plans), vec![6, 5]);
        assert_eq!(
            plans[0]
                .indices()
                .into_iter()
                .chain(plans[1].indices())
                .collect::<Vec<_>>(),
            (0..11).collect::<Vec<_>>()
        );
    }

    #[test]
    fn n14_splits_7_7() {
        assert_eq!(sizes(&plan_groups(&photos(14))), vec![7, 7]);
    }

    #[test]
    fn n20_splits_10_10() {
        assert_eq!(sizes(&plan_groups(&photos(20))), vec![10, 10]);
    }

    #[test]
    fn n21_splits_7_7_7() {
        assert_eq!(sizes(&plan_groups(&photos(21))), vec![7, 7, 7]);
    }

    #[test]
    fn n25_splits_9_8_8() {
        assert_eq!(sizes(&plan_groups(&photos(25))), vec![9, 8, 8]);
    }

    #[test]
    fn photo_and_video_mix_in_one_album() {
        let kinds = vec![MediaKind::Photo, MediaKind::Video, MediaKind::Photo];
        let plans = plan_groups(&kinds);
        assert_eq!(plans, vec![SendPlan::Album(vec![0, 1, 2])]);
    }

    #[test]
    fn animation_breaks_album_and_is_always_single() {
        let kinds = vec![
            MediaKind::Photo,
            MediaKind::Photo,
            MediaKind::Animation,
            MediaKind::Animation,
            MediaKind::Photo,
        ];
        let plans = plan_groups(&kinds);
        assert_eq!(
            plans,
            vec![
                SendPlan::Album(vec![0, 1]),
                SendPlan::Single(2),
                SendPlan::Single(3),
                SendPlan::Single(4),
            ]
        );
    }

    #[test]
    fn audio_and_document_do_not_mix() {
        let kinds = vec![
            MediaKind::Audio,
            MediaKind::Audio,
            MediaKind::Document,
            MediaKind::Document,
            MediaKind::Photo,
        ];
        let plans = plan_groups(&kinds);
        assert_eq!(
            plans,
            vec![
                SendPlan::Album(vec![0, 1]),
                SendPlan::Album(vec![2, 3]),
                SendPlan::Single(4),
            ]
        );
    }

    #[test]
    fn album_max_clamped_to_telegram_limit() {
        let plans = plan_groups_with_max(&photos(12), 20);
        assert!(plans.iter().all(|p| p.len() <= TELEGRAM_ALBUM_MAX));
        assert_eq!(sizes(&plans), vec![6, 6]);
    }
}
