//! Splitting a known-size download into contiguous byte-range segments.

use serde::{Deserialize, Serialize};

/// A half-open byte range `[start, end]` (inclusive end, matching HTTP `Range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    /// Bytes already written for this segment (for resume).
    #[serde(default)]
    pub completed: u64,
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_done(&self) -> bool {
        self.completed >= self.len()
    }

    /// The absolute offset where the next byte should be written.
    pub fn write_offset(&self) -> u64 {
        self.start + self.completed
    }

    /// The `Range` header value needed to fetch the remaining bytes.
    pub fn range_header(&self) -> String {
        format!("bytes={}-{}", self.write_offset(), self.end)
    }
}

/// Divide `total` bytes into at most `max_parts` roughly equal segments.
///
/// Rules:
///   - Files smaller than `min_split_size` download as a single segment.
///   - `max_parts` is clamped to at least 1.
///   - The final segment absorbs any remainder so ranges cover `[0, total-1]`.
pub fn plan(total: u64, max_parts: usize, min_split_size: u64) -> Vec<Segment> {
    if total == 0 {
        return vec![];
    }
    let max_parts = max_parts.max(1);

    // Too small to be worth splitting.
    if total < min_split_size || max_parts == 1 {
        return vec![Segment {
            start: 0,
            end: total - 1,
            completed: 0,
        }];
    }

    // Never create a segment smaller than the min split size.
    let parts_by_size = (total / min_split_size).max(1) as usize;
    let parts = max_parts.min(parts_by_size).max(1);

    let base = total / parts as u64;
    let mut segments = Vec::with_capacity(parts);
    let mut start = 0u64;
    for i in 0..parts {
        let end = if i == parts - 1 {
            total - 1
        } else {
            start + base - 1
        };
        segments.push(Segment {
            start,
            end,
            completed: 0,
        });
        start = end + 1;
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_segment_for_small_file() {
        let segs = plan(500, 8, 1024);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], Segment { start: 0, end: 499, completed: 0 });
    }

    #[test]
    fn splits_large_file_and_covers_all_bytes() {
        let total = 1000;
        let segs = plan(total, 4, 100);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, total - 1);
        // Contiguous, no gaps or overlaps.
        for w in segs.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start);
        }
        // Total bytes add up.
        let sum: u64 = segs.iter().map(|s| s.len()).sum();
        assert_eq!(sum, total);
    }

    #[test]
    fn respects_min_split_size() {
        // 3000 bytes, min split 1000 => at most 3 parts even if 8 requested.
        let segs = plan(3000, 8, 1000);
        assert_eq!(segs.len(), 3);
    }

    #[test]
    fn range_header_accounts_for_completed() {
        let seg = Segment { start: 100, end: 199, completed: 40 };
        assert_eq!(seg.range_header(), "bytes=140-199");
        assert_eq!(seg.write_offset(), 140);
        assert!(!seg.is_done());
    }
}
