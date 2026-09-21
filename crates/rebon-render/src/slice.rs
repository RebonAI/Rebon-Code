//! Cap and windowing helpers for the non-virtualized transcript render path.

use crate::timeline::TimelineMessage;

/// Default cap on how many rows the non-virtualized path renders.
pub const MAX_MESSAGES_WITHOUT_VIRTUALIZATION: usize = 200;
/// Slack added to the cap before the render window jumps forward.
pub const MESSAGE_CAP_STEP: usize = 50;

/// Stable UUID-bearing item.
pub trait HasUuid {
    /// Stable UUID string.
    fn uuid(&self) -> &str;
}

impl HasUuid for TimelineMessage {
    fn uuid(&self) -> &str {
        self.uuid()
    }
}

/// Persistent anchor tracked by the capped non-virtualized render path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceAnchor {
    /// UUID of the first rendered row.
    pub uuid: String,
    /// Index fallback when the UUID disappears after regrouping.
    pub idx: usize,
}

/// Pick the first row of the capped render window and keep `anchor_ref` pointing at it.
pub fn compute_slice_start<T: HasUuid>(
    collapsed: &[T],
    anchor_ref: &mut Option<SliceAnchor>,
    cap: usize,
    step: usize,
) -> usize {
    let anchor = anchor_ref.clone();
    let anchor_idx = anchor
        .as_ref()
        .and_then(|anchor| {
            collapsed
                .iter()
                .position(|message| message.uuid() == anchor.uuid)
        })
        .map(|idx| idx as isize)
        .unwrap_or(-1);

    let mut start = if anchor_idx >= 0 {
        anchor_idx as usize
    } else if let Some(anchor) = anchor.as_ref() {
        anchor.idx.min(collapsed.len().saturating_sub(cap))
    } else {
        0
    };

    if collapsed.len().saturating_sub(start) > cap + step {
        start = collapsed.len().saturating_sub(cap);
    }

    if let Some(message_at_start) = collapsed.get(start) {
        if anchor.as_ref().map_or(true, |anchor| {
            anchor.uuid != message_at_start.uuid() || anchor.idx != start
        }) {
            *anchor_ref = Some(SliceAnchor {
                uuid: message_at_start.uuid().to_owned(),
                idx: start,
            });
        }
    } else if anchor.is_some() {
        *anchor_ref = None;
    }

    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item {
        uuid: String,
    }

    impl HasUuid for Item {
        fn uuid(&self) -> &str {
            &self.uuid
        }
    }

    #[test]
    fn compute_slice_start_initializes_anchor_and_starts_at_zero() {
        let collapsed = vec![
            Item { uuid: "a".into() },
            Item { uuid: "b".into() },
            Item { uuid: "c".into() },
        ];
        let mut anchor = None;
        let start = compute_slice_start(&collapsed, &mut anchor, 200, 50);
        assert_eq!(start, 0);
        assert_eq!(
            anchor,
            Some(SliceAnchor {
                uuid: "a".into(),
                idx: 0
            })
        );
    }

    #[test]
    fn compute_slice_start_advances_when_cap_plus_step_is_exceeded() {
        let collapsed = (0..275)
            .map(|index| Item {
                uuid: format!("m-{index}"),
            })
            .collect::<Vec<_>>();
        let mut anchor = Some(SliceAnchor {
            uuid: "m-0".into(),
            idx: 0,
        });
        let start = compute_slice_start(&collapsed, &mut anchor, 200, 50);
        assert_eq!(start, 75);
        assert_eq!(anchor.as_ref().map(|anchor| anchor.idx), Some(75));
    }

    #[test]
    fn compute_slice_start_falls_back_to_stored_index_when_uuid_disappears() {
        let collapsed = vec![
            Item { uuid: "a".into() },
            Item { uuid: "c".into() },
            Item { uuid: "d".into() },
            Item { uuid: "e".into() },
        ];
        let mut anchor = Some(SliceAnchor {
            uuid: "b".into(),
            idx: 2,
        });
        let start = compute_slice_start(&collapsed, &mut anchor, 3, 1);
        assert_eq!(start, 1);
        assert_eq!(
            anchor.as_ref().map(|anchor| anchor.uuid.as_str()),
            Some("c")
        );
    }

    #[test]
    fn compute_slice_start_clears_anchor_when_list_becomes_empty() {
        let collapsed: Vec<Item> = Vec::new();
        let mut anchor = Some(SliceAnchor {
            uuid: "gone".into(),
            idx: 4,
        });
        let start = compute_slice_start(&collapsed, &mut anchor, 200, 50);
        assert_eq!(start, 0);
        assert_eq!(anchor, None);
    }
}
