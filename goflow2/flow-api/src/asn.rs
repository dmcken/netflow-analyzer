//! IP -> ASN enrichment for the tier-3 capacity-planning endpoint.
//!
//! Loaded once at startup from an iptoasn.com-format TSV (start_ip, end_ip,
//! asn, country_code, as_name) plus a small YAML override list for private
//! address space and this network's own custom ASN blocks - same source
//! data and override precedence as the original goflow2_analysis tool
//! (custom overrides checked first, then the public database).
//!
//! Enrichment happens in Rust after DataFusion does the heavy-lifting
//! GROUP BY dst_addr/src_addr aggregation, not as a DataFusion UDF - simpler
//! to get right, and the row count DataFusion hands back (distinct IPs, not
//! raw flows) is small enough that a second aggregation pass in Rust is
//! cheap.

use std::{fs::File, io::Read, net::IpAddr, path::Path};

use flate2::read::GzDecoder;
use ipnet::IpNet;
use serde::Deserialize;

#[derive(Clone)]
pub struct AsnInfo {
    pub asn: u32,
    pub country: String,
    pub org: String,
}

#[derive(Deserialize)]
struct OverrideEntry {
    network: IpNet,
    asn: u32,
    organization: String,
    country_code: String,
}

struct Range4 {
    start: u32,
    end: u32,
    info: AsnInfo,
}

struct Range6 {
    start: u128,
    end: u128,
    info: AsnInfo,
}

pub struct AsnDb {
    overrides: Vec<(IpNet, AsnInfo)>,
    v4: Vec<Range4>,
    v6: Vec<Range6>,
}

impl AsnDb {
    pub fn load(ip2asn_tsv_gz: &Path, override_yaml: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut overrides = Vec::new();
        if override_yaml.exists() {
            let content = std::fs::read_to_string(override_yaml)?;
            let entries: Vec<OverrideEntry> = serde_yaml_ng::from_str(&content)?;
            for e in entries {
                overrides.push((
                    e.network,
                    AsnInfo { asn: e.asn, country: e.country_code, org: e.organization },
                ));
            }
            // Longest prefix first, so a more specific override wins over a
            // broader one covering the same address.
            overrides.sort_by_key(|(net, _)| std::cmp::Reverse(net.prefix_len()));
        }

        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        let file = File::open(ip2asn_tsv_gz)?;
        let mut decoder = GzDecoder::new(file);
        let mut text = String::new();
        decoder.read_to_string(&mut text)?;

        for line in text.lines() {
            let mut cols = line.split('\t');
            let (Some(start_s), Some(end_s), Some(asn_s), Some(country), Some(org)) =
                (cols.next(), cols.next(), cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let (Ok(start_ip), Ok(end_ip)) = (start_s.parse::<IpAddr>(), end_s.parse::<IpAddr>()) else {
                continue;
            };
            let Ok(asn) = asn_s.parse::<u32>() else { continue };
            if asn == 0 {
                continue; // "Not routed" / unassigned - not useful to report
            }
            let info = AsnInfo { asn, country: country.to_string(), org: org.to_string() };
            match (start_ip, end_ip) {
                (IpAddr::V4(s), IpAddr::V4(e)) => v4.push(Range4 { start: s.into(), end: e.into(), info }),
                (IpAddr::V6(s), IpAddr::V6(e)) => v6.push(Range6 { start: s.into(), end: e.into(), info }),
                _ => {}
            }
        }
        v4.sort_by_key(|r| r.start);
        v6.sort_by_key(|r| r.start);

        tracing::info!(
            "loaded ASN db: {} IPv4 ranges, {} IPv6 ranges, {} overrides",
            v4.len(),
            v6.len(),
            overrides.len()
        );

        Ok(Self { overrides, v4, v6 })
    }

    pub fn lookup(&self, ip: IpAddr) -> Option<AsnInfo> {
        for (net, info) in &self.overrides {
            if net.contains(&ip) {
                return Some(info.clone());
            }
        }
        match ip {
            IpAddr::V4(v4) => {
                let key: u32 = v4.into();
                let idx = self.v4.partition_point(|r| r.start <= key);
                if idx == 0 {
                    return None;
                }
                let r = &self.v4[idx - 1];
                (key <= r.end).then(|| r.info.clone())
            }
            IpAddr::V6(v6) => {
                let key: u128 = v6.into();
                let idx = self.v6.partition_point(|r| r.start <= key);
                if idx == 0 {
                    return None;
                }
                let r = &self.v6[idx - 1];
                (key <= r.end).then(|| r.info.clone())
            }
        }
    }
}
