//! CIDR-list containment for /v1/asn-peer-stats.
//!
//! A naive SQL `WHERE addr BETWEEN X'..' AND X'..' OR ...` over a caller-
//! supplied prefix list (see the first version of asn_peer_stats) doesn't
//! scale: DataFusion evaluates that OR chain per row with no way to turn it
//! into a binary search, so a few hundred prefixes over a large scan can
//! take minutes instead of seconds. `PrefixRanges` builds the same kind of
//! sorted-range binary-search structure `asn::AsnDb` already uses for its
//! (much smaller, fixed) override list, registered as a one-off DataFusion
//! scalar UDF per request instead of a giant boolean expression - see
//! main.rs's `asn_peer_stats` for how it's wired in.

use ipnet::IpNet;

#[derive(Debug)]
pub struct PrefixRanges {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

impl PrefixRanges {
    /// Build from a caller-supplied prefix list. Ranges are merged and
    /// sorted here rather than trusted to already be a minimal disjoint
    /// set - the binary search in `contains_bytes` requires that
    /// invariant to be correct, and a defensive merge is cheap next to the
    /// per-row cost this whole structure exists to avoid.
    pub fn new(prefixes: &[IpNet]) -> Self {
        let mut v4_raw = Vec::new();
        let mut v6_raw = Vec::new();
        for net in prefixes {
            match (net.network(), net.broadcast()) {
                (std::net::IpAddr::V4(s), std::net::IpAddr::V4(e)) => {
                    v4_raw.push((u32::from(s), u32::from(e)));
                }
                (std::net::IpAddr::V6(s), std::net::IpAddr::V6(e)) => {
                    v6_raw.push((u128::from(s), u128::from(e)));
                }
                _ => {} // network()/broadcast() always agree in family with a valid IpNet
            }
        }
        Self { v4: merge_ranges_u32(v4_raw), v6: merge_ranges_u128(v6_raw) }
    }

    /// Whether the given raw address bytes (4 = IPv4, 16 = IPv6, as stored
    /// in the flows table's src_addr/dst_addr columns) fall within any of
    /// this set's prefixes.
    pub fn contains_bytes(&self, bytes: &[u8]) -> bool {
        match bytes.len() {
            4 => {
                let key = u32::from_be_bytes(bytes.try_into().unwrap());
                contains(&self.v4, key)
            }
            16 => {
                let key = u128::from_be_bytes(bytes.try_into().unwrap());
                contains(&self.v6, key)
            }
            _ => false,
        }
    }
}

fn contains<T: Ord + Copy>(ranges: &[(T, T)], key: T) -> bool {
    let idx = ranges.partition_point(|&(start, _)| start <= key);
    idx > 0 && key <= ranges[idx - 1].1
}

macro_rules! merge_ranges_fn {
    ($name:ident, $t:ty) => {
        /// Sort by start, then fold any overlapping or adjacent (end+1 ==
        /// next start) ranges together so the binary search in `contains`
        /// only ever needs to look at the one range that starts
        /// at-or-before the key.
        fn $name(mut ranges: Vec<($t, $t)>) -> Vec<($t, $t)> {
            ranges.sort_by_key(|&(start, _)| start);
            let mut merged: Vec<($t, $t)> = Vec::with_capacity(ranges.len());
            for (start, end) in ranges {
                if let Some(last) = merged.last_mut() {
                    if start <= last.1.saturating_add(1) {
                        if end > last.1 {
                            last.1 = end;
                        }
                        continue;
                    }
                }
                merged.push((start, end));
            }
            merged
        }
    };
}

merge_ranges_fn!(merge_ranges_u32, u32);
merge_ranges_fn!(merge_ranges_u128, u128);
