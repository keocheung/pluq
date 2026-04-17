//! Generic clustering of byte regions for HTTP batch I/O.
//!
//! Archives stored on high-latency transports (HTTP range, Google Drive)
//! pay one RTT per `read_stream` call. When extracting N entries, naive
//! code opens N streams; this module groups entries whose compressed
//! regions are close enough on disk that one stream covers many of them.
//!
//! Pure math — format-agnostic, no I/O, no async. Format parsers supply
//! `FetchRegion<T>` structs describing (offset, total_len, payload) and
//! receive `Vec<Vec<FetchRegion<T>>>` back, ordered by offset.

/// A byte region to be fetched, carrying an opaque payload for the caller.
#[derive(Debug, Clone)]
pub struct FetchRegion<T> {
    /// Absolute byte offset in the source.
    pub offset: u64,
    /// Total bytes required to satisfy this region (header + data for RAR,
    /// LFH + data for ZIP, etc. — caller defines).
    pub total_len: u64,
    /// Format-specific payload (entry metadata, location handle, ...).
    pub payload: T,
}

/// Gap threshold. Regions whose ranges are within this distance share a
/// single `read_stream` call. 2 MiB ≈ 400 ms at 5 MB/s — less than the
/// extra-request RTT for typical cloud providers.
pub const COALESCE_GAP: u64 = 2 * 1024 * 1024;

/// Max ratio of wasted (gap) bytes to useful (data) bytes inside a cluster.
/// Prevents pathological over-fetch when many tiny entries sit just under
/// the gap threshold.
pub const MAX_WASTE_RATIO: u64 = 10;

/// Group regions into coalesced clusters.
///
/// Input regions are sorted by offset internally; callers may pass them in
/// any order. A new cluster is started when either the gap exceeds
/// `COALESCE_GAP` or the cumulative waste ratio would exceed
/// `MAX_WASTE_RATIO`.
pub fn build_clusters<T>(mut regions: Vec<FetchRegion<T>>) -> Vec<Vec<FetchRegion<T>>> {
    if regions.is_empty() {
        return vec![];
    }
    regions.sort_by_key(|r| r.offset);

    let mut clusters: Vec<Vec<FetchRegion<T>>> = Vec::new();
    let mut current: Vec<FetchRegion<T>> = Vec::new();
    let mut cluster_data: u64 = 0;
    let mut cluster_gap: u64 = 0;
    let mut prev_end: u64 = 0;

    for region in regions {
        if current.is_empty() {
            prev_end = region.offset.saturating_add(region.total_len);
            cluster_data = region.total_len;
            cluster_gap = 0;
            current.push(region);
            continue;
        }

        let gap = region.offset.saturating_sub(prev_end);
        let new_data = cluster_data.saturating_add(region.total_len);
        let new_gap = cluster_gap.saturating_add(gap);

        let gap_ok = gap < COALESCE_GAP;
        let ratio_ok = new_gap < new_data.saturating_mul(MAX_WASTE_RATIO);

        if gap_ok && ratio_ok {
            prev_end = region.offset.saturating_add(region.total_len);
            cluster_data = new_data;
            cluster_gap = new_gap;
            current.push(region);
        } else {
            clusters.push(std::mem::take(&mut current));
            prev_end = region.offset.saturating_add(region.total_len);
            cluster_data = region.total_len;
            cluster_gap = 0;
            current.push(region);
        }
    }

    clusters.push(current);
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u64, total_len: u64, id: u32) -> FetchRegion<u32> {
        FetchRegion { offset, total_len, payload: id }
    }

    #[test]
    fn empty_input_returns_empty() {
        let out: Vec<Vec<FetchRegion<u32>>> = build_clusters(vec![]);
        assert!(out.is_empty());
    }

    #[test]
    fn single_region_is_single_cluster() {
        let out = build_clusters(vec![r(100, 50, 1)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 1);
        assert_eq!(out[0][0].payload, 1);
    }

    #[test]
    fn adjacent_regions_coalesce() {
        let out = build_clusters(vec![r(0, 100, 1), r(100, 100, 2), r(200, 100, 3)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 3);
    }

    #[test]
    fn large_gap_splits_cluster() {
        let out = build_clusters(vec![
            r(0, 100, 1),
            r(100 + COALESCE_GAP + 1, 100, 2),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn sorts_unsorted_input() {
        let out = build_clusters(vec![r(300, 50, 3), r(100, 50, 1), r(200, 50, 2)]);
        assert_eq!(out.len(), 1);
        let ids: Vec<u32> = out[0].iter().map(|r| r.payload).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn waste_ratio_triggers_split() {
        // Tiny data, gaps just under COALESCE_GAP — cumulative gap eventually
        // exceeds MAX_WASTE_RATIO * data and forces a split.
        let step = COALESCE_GAP - 1;
        let regions: Vec<_> = (0..50).map(|i| r(i * step, 10, i as u32)).collect();
        let out = build_clusters(regions);
        assert!(out.len() > 1, "expected splits from ratio guard");
    }
}
