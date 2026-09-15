//! HTTP API in front of the Parquet flow archive.
//!
//! Read-only query service so other internal tools (abuse handling,
//! troubleshooting, capacity planning) go through a stable API instead of
//! touching the storage layer directly. Uses DataFusion (Arrow's own SQL
//! engine) rather than embedding a database - DataFusion's `object_store`
//! backend speaks local disk and S3-compatible stores (Ceph RGW included)
//! through the same code path, so moving the archive off local disk later
//! is a config change (a `file://` path becomes `s3://`), not a rewrite.
//!
//! First endpoint: /v1/nat-lookup - resolve an abuse report's
//! public IP:port at a point in time back to the internal customer IP via
//! the post_nat_*/post_napt_* columns (see the field-map.yaml fix that
//! made this data available at all).

mod asn;
mod cidr;

use std::{
    any::Any,
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Datelike, Duration, Utc};
use clap::Parser;
use datafusion::{
    arrow::{array::BooleanBuilder, datatypes::DataType},
    common::Result as DFResult,
    datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl},
    logical_expr::{ColumnarValue, ScalarUDF, ScalarUDFImpl, Signature, Volatility},
    prelude::*,
};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::json;

use asn::AsnDb;
use cidr::PrefixRanges;

const FLOWS_TABLE: &str = "flows";

#[derive(Parser)]
struct Cli {
    /// Root of the Hive-partitioned Parquet archive (year=/month=/day=/*.parquet)
    #[arg(long)]
    data_dir: String,

    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:8090")]
    listen_addr: String,

    /// iptoasn.com-format ip2asn-combined.tsv.gz, for /v1/asn-stats
    #[arg(long, default_value = "data/ip2asn-combined.tsv.gz")]
    ip2asn_path: PathBuf,

    /// Custom ASN overrides (private ranges, this network's own blocks)
    #[arg(long, default_value = "data/override.yaml")]
    override_path: PathBuf,
}

struct AppState {
    ctx: SessionContext,
    asn_db: AsnDb,
    /// Source of unique names for the per-request CIDR-match UDF
    /// asn_peer_stats registers - see its doc comment. Relaxed ordering is
    /// fine: uniqueness across concurrent requests is all that's needed,
    /// not any particular order.
    next_udf_id: AtomicU64,
    /// Caches for asn_peer_stats/asn_peer_timeseries - see PeerStatsCache's
    /// doc comment for what's cached and why.
    peer_stats_cache: Mutex<HashMap<String, CacheEntry<Vec<AsnStat>>>>,
    peer_timeseries_cache: Mutex<HashMap<String, CacheEntry<Vec<AsnTimeseriesPoint>>>>,
}

/// One cached (outbound, inbound) result, keyed on everything that
/// determines the underlying per-ASN data (prefixes/time range/bucket
/// size) but deliberately *not* `top` or `exclude_asns` - both are cheap
/// to reapply to an already-fetched full breakdown, so caching before
/// they're applied means a request that only changes which ASNs are
/// excluded, or how many rows it wants, is still a cache hit. See
/// `cache_key`/`cache_expiry`/`get_or_compute` for how this is used.
struct CacheEntry<T> {
    outbound: T,
    inbound: T,
    /// None means "never expires" - used for a fully-elapsed time window,
    /// which can't produce different data on a later query. Some(_) is
    /// used for a window still open (touches "now"), which needs a short
    /// TTL since new flows keep arriving for it.
    expires_at: Option<Instant>,
}

impl<T> CacheEntry<T> {
    fn is_fresh(&self) -> bool {
        match self.expires_at {
            None => true,
            Some(t) => Instant::now() < t,
        }
    }
}

const OPEN_WINDOW_CACHE_TTL: StdDuration = StdDuration::from_secs(300);
// How far in the past `end` must be before its window is treated as fully
// elapsed (and thus cacheable forever) - a safety margin over the ~5-minute
// flush window parquet-datalake writes in, so a "closed" window can't
// still be missing not-yet-flushed data.
const WINDOW_CLOSED_MARGIN: Duration = Duration::minutes(10);

/// Whether [start, end) is fully in the past (by WINDOW_CLOSED_MARGIN) and
/// therefore immutable - if so, its cache entry never needs to expire.
fn window_is_closed(end: DateTime<Utc>) -> bool {
    Utc::now() - end > WINDOW_CLOSED_MARGIN
}

fn cache_expiry(end: DateTime<Utc>) -> Option<Instant> {
    if window_is_closed(end) {
        None
    } else {
        Some(Instant::now() + OPEN_WINDOW_CACHE_TTL)
    }
}

/// Cache key covering everything that changes the underlying per-ASN data:
/// the prefix list (order-independent - sorted first) and the time
/// range/bucket size. Deliberately excludes `top`/`exclude_asns` - see
/// `CacheEntry`'s doc comment.
fn cache_key(prefixes: &[IpNet], start_ns: i64, end_ns: i64, bucket_secs: Option<i64>) -> String {
    let mut sorted: Vec<String> = prefixes.iter().map(|p| p.to_string()).collect();
    sorted.sort_unstable();
    format!("{}|{}|{}|{}", sorted.join(","), start_ns, end_ns, bucket_secs.unwrap_or(0))
}

/// Cap on distinct cache entries kept at once - a crude but sufficient
/// guard against unbounded growth over a long uptime. In practice the
/// number of distinct (prefixes, time range, bucket size) combinations
/// actually queried is expected to stay far below this (a handful of
/// networks x a handful of recent days x 1-2 bucket sizes), so falling
/// back to "just clear everything" on overflow isn't expected to bite -
/// LRU eviction would be overkill for that pattern.
const CACHE_MAX_ENTRIES: usize = 500;

fn cache_insert<T>(cache: &Mutex<HashMap<String, CacheEntry<T>>>, key: String, entry: CacheEntry<T>) {
    let mut guard = cache.lock().unwrap();
    if guard.len() >= CACHE_MAX_ENTRIES {
        guard.retain(|_, v| v.is_fresh());
        if guard.len() >= CACHE_MAX_ENTRIES {
            guard.clear();
        }
    }
    guard.insert(key, entry);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    let ctx = SessionContext::new();
    register_flows_table(&ctx, &args.data_dir).await?;
    let asn_db = AsnDb::load(&args.ip2asn_path, &args.override_path)?;

    let state = Arc::new(AppState {
        ctx,
        asn_db,
        next_udf_id: AtomicU64::new(0),
        peer_stats_cache: Mutex::new(HashMap::new()),
        peer_timeseries_cache: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/nat-lookup", get(nat_lookup))
        .route("/v1/flows", get(flows))
        .route("/v1/traffic-stats", get(traffic_stats))
        .route("/v1/asn-stats", get(asn_stats))
        .route("/v1/asn-peer-stats", get(asn_peer_stats))
        .route("/v1/asn-peer-timeseries", get(asn_peer_timeseries))
        .with_state(state);

    tracing::info!("listening on {}", args.listen_addr);
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn register_flows_table(ctx: &SessionContext, data_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let table_url = ListingTableUrl::parse(data_dir)?;

    let options = ListingOptions::new(Arc::new(
        datafusion::datasource::file_format::parquet::ParquetFormat::default(),
    ))
    .with_table_partition_cols(vec![
        ("year".to_string(), DataType::Utf8),
        ("month".to_string(), DataType::Utf8),
        ("day".to_string(), DataType::Utf8),
    ])
    .with_file_extension(".parquet");

    let config = ListingTableConfig::new(table_url)
        .with_listing_options(options)
        .infer_schema(&ctx.state())
        .await?;

    let table = ListingTable::try_new(config)?;
    ctx.register_table(FLOWS_TABLE, Arc::new(table))?;
    tracing::info!("registered '{FLOWS_TABLE}' table from {data_dir}");
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

fn ip_to_hex_literal(ip: IpAddr) -> String {
    let bytes: Vec<u8> = match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[derive(Deserialize)]
struct NatLookupParams {
    ip: IpAddr,
    port: u16,
    /// RFC3339 timestamp, e.g. 2026-09-01T22:26:57Z
    at: DateTime<Utc>,
    /// How far before/after `at` to search for an overlapping flow window
    #[serde(default = "default_tolerance")]
    tolerance_secs: i64,
}

fn default_tolerance() -> i64 {
    60
}

#[derive(Serialize)]
struct NatLookupMatch {
    direction: &'static str,
    internal_ip: String,
    remote_ip: String,
    remote_port: Option<u32>,
    proto: u8,
    time_flow_start_ns: i64,
    time_flow_end_ns: i64,
    bytes: i64,
    packets: i64,
}

async fn nat_lookup(
    State(state): State<Arc<AppState>>,
    Query(params): Query<NatLookupParams>,
) -> impl IntoResponse {
    let ip_hex = ip_to_hex_literal(params.ip);
    let at_ns = params.at.timestamp_nanos_opt().unwrap_or(0);
    let tolerance_ns = params.tolerance_secs * 1_000_000_000;
    let window_start = at_ns - tolerance_ns;
    let window_end = at_ns + tolerance_ns;

    // Check both directions: the reported public IP:port could be either
    // the post-NAT source (outbound traffic from a customer) or the
    // post-NAT destination (inbound, e.g. port forwarding) - an abuse
    // report doesn't tell you which side of the flow it caught.
    let sql = format!(
        "SELECT 'outbound' AS direction, \
                src_addr, dst_addr, dst_port, proto, \
                time_flow_start_ns, time_flow_end_ns, bytes, packets \
         FROM {FLOWS_TABLE} \
         WHERE post_nat_src_ipv4_address = X'{ip_hex}' \
           AND post_napt_src_transport_port = {port} \
           AND time_flow_start_ns <= {window_end} \
           AND time_flow_end_ns >= {window_start} \
         UNION ALL \
         SELECT 'inbound' AS direction, \
                dst_addr AS src_addr, src_addr AS dst_addr, src_port AS dst_port, proto, \
                time_flow_start_ns, time_flow_end_ns, bytes, packets \
         FROM {FLOWS_TABLE} \
         WHERE post_nat_dst_ipv4_address = X'{ip_hex}' \
           AND post_napt_dst_transport_port = {port} \
           AND time_flow_start_ns <= {window_end} \
           AND time_flow_end_ns >= {window_start} \
         LIMIT 50",
        ip_hex = ip_hex,
        port = params.port,
        window_end = window_end,
        window_start = window_start,
    );

    let df = match state.ctx.sql(&sql).await {
        Ok(df) => df,
        Err(e) => {
            tracing::error!("query planning failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response();
        }
    };

    let batches = match df.collect().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("query execution failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response();
        }
    };

    let mut matches = Vec::new();
    for batch in &batches {
        for row in batch_to_nat_matches(batch) {
            matches.push(row);
        }
    }

    Json(json!({
        "query": {
            "ip": params.ip.to_string(),
            "port": params.port,
            "at": params.at.to_rfc3339(),
            "tolerance_secs": params.tolerance_secs,
        },
        "matches": matches,
    }))
    .into_response()
}

// DataFusion doesn't always hand back binary columns in their on-disk
// representation: a plain query preserves the Parquet schema's Binary
// type, but going through operators like UNION ALL promotes it to
// BinaryView (its coalesce/interleave kernels prefer the view type) - see
// the flow-api README for how this bit the first version of /v1/nat-lookup.
// Handle every representation DataFusion might produce rather than
// assuming one, so future queries don't silently return empty IPs again.
fn binary_at(col: &dyn datafusion::arrow::array::Array, i: usize) -> Option<Vec<u8>> {
    use datafusion::arrow::array::{Array, BinaryArray, BinaryViewArray, LargeBinaryArray};
    if let Some(a) = col.as_any().downcast_ref::<BinaryArray>() {
        return (!a.is_null(i)).then(|| a.value(i).to_vec());
    }
    if let Some(a) = col.as_any().downcast_ref::<BinaryViewArray>() {
        return (!a.is_null(i)).then(|| a.value(i).to_vec());
    }
    if let Some(a) = col.as_any().downcast_ref::<LargeBinaryArray>() {
        return (!a.is_null(i)).then(|| a.value(i).to_vec());
    }
    None
}

fn batch_to_nat_matches(batch: &datafusion::arrow::record_batch::RecordBatch) -> Vec<NatLookupMatch> {
    use datafusion::arrow::array::*;

    let direction = batch.column_by_name("direction").and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let src_addr = batch.column_by_name("src_addr");
    let dst_addr = batch.column_by_name("dst_addr");
    let dst_port = batch.column_by_name("dst_port").and_then(|c| c.as_any().downcast_ref::<UInt16Array>());
    let proto = batch.column_by_name("proto").and_then(|c| c.as_any().downcast_ref::<UInt8Array>());
    let start_ns = batch.column_by_name("time_flow_start_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let end_ns = batch.column_by_name("time_flow_end_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let bytes = batch.column_by_name("bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let packets = batch.column_by_name("packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

    let mut out = Vec::new();
    let Some(direction) = direction else { return out };
    for i in 0..batch.num_rows() {
        let internal_ip = src_addr.and_then(|a| binary_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default();
        let remote_ip = dst_addr.and_then(|a| binary_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default();
        out.push(NatLookupMatch {
            direction: if direction.value(i) == "outbound" { "outbound" } else { "inbound" },
            internal_ip,
            remote_ip,
            remote_port: dst_port.and_then(|a| if a.is_null(i) { None } else { Some(a.value(i) as u32) }),
            proto: proto.map(|a| a.value(i)).unwrap_or(0),
            time_flow_start_ns: start_ns.map(|a| a.value(i)).unwrap_or(0),
            time_flow_end_ns: end_ns.map(|a| a.value(i)).unwrap_or(0),
            bytes: bytes.map(|a| a.value(i)).unwrap_or(0),
            packets: packets.map(|a| a.value(i)).unwrap_or(0),
        });
    }
    out
}

// ---------- tier 2: point queries ----------

#[derive(Deserialize)]
struct FlowsParams {
    ip: IpAddr,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default = "default_flows_limit")]
    limit: usize,
}

fn default_flows_limit() -> usize {
    100
}

#[derive(Serialize)]
struct FlowRecord {
    src_addr: String,
    dst_addr: String,
    src_port: Option<u32>,
    dst_port: Option<u32>,
    proto: u8,
    bytes: i64,
    packets: i64,
    time_flow_start_ns: i64,
    time_flow_end_ns: i64,
}

/// Raw matching flow records for an IP (as either src or dst) over a time
/// range - "what connections did/does this IP have" for security audit or
/// connectivity troubleshooting.
async fn flows(State(state): State<Arc<AppState>>, Query(params): Query<FlowsParams>) -> impl IntoResponse {
    let ip_hex = ip_to_hex_literal(params.ip);
    let start_ns = params.start.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = params.end.timestamp_nanos_opt().unwrap_or(0);
    let limit = params.limit.min(1000);

    let sql = format!(
        "SELECT src_addr, dst_addr, src_port, dst_port, proto, bytes, packets, \
                time_flow_start_ns, time_flow_end_ns \
         FROM {FLOWS_TABLE} \
         WHERE (src_addr = X'{ip_hex}' OR dst_addr = X'{ip_hex}') \
           AND time_flow_start_ns <= {end_ns} \
           AND time_flow_end_ns >= {start_ns} \
         ORDER BY time_flow_start_ns DESC \
         LIMIT {limit}"
    );

    let batches = match run_sql(&state.ctx, &sql).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let mut records = Vec::new();
    for batch in &batches {
        records.extend(batch_to_flow_records(batch));
    }

    Json(json!({
        "query": {
            "ip": params.ip.to_string(),
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "limit": limit,
        },
        "flows": records,
    }))
    .into_response()
}

fn batch_to_flow_records(batch: &datafusion::arrow::record_batch::RecordBatch) -> Vec<FlowRecord> {
    use datafusion::arrow::array::*;

    let src_addr = batch.column_by_name("src_addr");
    let dst_addr = batch.column_by_name("dst_addr");
    let src_port = batch.column_by_name("src_port").and_then(|c| c.as_any().downcast_ref::<UInt16Array>());
    let dst_port = batch.column_by_name("dst_port").and_then(|c| c.as_any().downcast_ref::<UInt16Array>());
    let proto = batch.column_by_name("proto").and_then(|c| c.as_any().downcast_ref::<UInt8Array>());
    let bytes = batch.column_by_name("bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let packets = batch.column_by_name("packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let start_ns = batch.column_by_name("time_flow_start_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let end_ns = batch.column_by_name("time_flow_end_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(FlowRecord {
            src_addr: src_addr.and_then(|a| binary_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default(),
            dst_addr: dst_addr.and_then(|a| binary_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default(),
            src_port: src_port.and_then(|a| (!a.is_null(i)).then(|| a.value(i) as u32)),
            dst_port: dst_port.and_then(|a| (!a.is_null(i)).then(|| a.value(i) as u32)),
            proto: proto.map(|a| a.value(i)).unwrap_or(0),
            bytes: bytes.map(|a| a.value(i)).unwrap_or(0),
            packets: packets.map(|a| a.value(i)).unwrap_or(0),
            time_flow_start_ns: start_ns.map(|a| a.value(i)).unwrap_or(0),
            time_flow_end_ns: end_ns.map(|a| a.value(i)).unwrap_or(0),
        });
    }
    out
}

#[derive(Deserialize)]
struct TrafficStatsParams {
    ip: IpAddr,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default = "default_bucket_secs")]
    bucket_secs: i64,
}

fn default_bucket_secs() -> i64 {
    300
}

#[derive(Serialize)]
struct TrafficBucket {
    bucket_start: String,
    total_bytes: i64,
    total_packets: i64,
    flow_count: i64,
}

/// Aggregated bytes/packets over time buckets for an IP - "traffic stats
/// for a particular IP" for troubleshooting tools.
async fn traffic_stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TrafficStatsParams>,
) -> impl IntoResponse {
    let ip_hex = ip_to_hex_literal(params.ip);
    let start_ns = params.start.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = params.end.timestamp_nanos_opt().unwrap_or(0);
    let bucket_ns = params.bucket_secs.max(1) * 1_000_000_000;

    // Integer bucket math instead of DataFusion's timestamp functions -
    // time_flow_start_ns is already an epoch-nanosecond integer, so
    // (x / bucket) * bucket is a plain floor-to-bucket with no need to cast
    // through a Timestamp type at all.
    let sql = format!(
        "SELECT (time_flow_start_ns / {bucket_ns}) * {bucket_ns} AS bucket_start_ns, \
                SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
         FROM {FLOWS_TABLE} \
         WHERE (src_addr = X'{ip_hex}' OR dst_addr = X'{ip_hex}') \
           AND time_flow_start_ns <= {end_ns} \
           AND time_flow_start_ns >= {start_ns} \
         GROUP BY bucket_start_ns \
         ORDER BY bucket_start_ns"
    );

    let batches = match run_sql(&state.ctx, &sql).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let mut buckets = Vec::new();
    for batch in &batches {
        buckets.extend(batch_to_traffic_buckets(batch));
    }

    Json(json!({
        "query": {
            "ip": params.ip.to_string(),
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "bucket_secs": params.bucket_secs,
        },
        "buckets": buckets,
    }))
    .into_response()
}

fn batch_to_traffic_buckets(batch: &datafusion::arrow::record_batch::RecordBatch) -> Vec<TrafficBucket> {
    use datafusion::arrow::array::*;

    let bucket_ns = batch.column_by_name("bucket_start_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let total_bytes = batch.column_by_name("total_bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let total_packets = batch.column_by_name("total_packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let flow_count = batch.column_by_name("flow_count").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let ns = bucket_ns.map(|a| a.value(i)).unwrap_or(0);
        let bucket_start = DateTime::<Utc>::from_timestamp(ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        out.push(TrafficBucket {
            bucket_start,
            total_bytes: total_bytes.map(|a| a.value(i)).unwrap_or(0),
            total_packets: total_packets.map(|a| a.value(i)).unwrap_or(0),
            flow_count: flow_count.map(|a| a.value(i)).unwrap_or(0),
        });
    }
    out
}

// ---------- tier 3: ASN-level analysis ----------

#[derive(Deserialize)]
struct AsnStatsParams {
    /// "outbound": group by destination ASN (what our customers access).
    /// "inbound": group by source ASN (who accesses our network).
    direction: AsnDirection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default = "default_asn_top")]
    top: usize,
    /// Comma-separated ASNs to drop from the results entirely (e.g. a
    /// peering partner the caller doesn't want counted) - excluded before
    /// ranking, so it never displaces a real top-N entry.
    #[serde(default)]
    exclude_asns: String,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum AsnDirection {
    Outbound,
    Inbound,
}

fn default_asn_top() -> usize {
    20
}

#[derive(Serialize, Clone)]
struct AsnStat {
    asn: u32,
    org: String,
    country: String,
    total_bytes: i64,
    total_packets: i64,
    flow_count: i64,
}

/// ASN-level traffic aggregation for capacity planning ("what networks are
/// our customers accessing" / "what ASNs are accessing servers on our
/// network"). DataFusion does the heavy GROUP BY dst_addr/src_addr
/// aggregation (collapsing potentially billions of raw flows down to one
/// row per distinct IP); the IP -> ASN mapping and a second, much smaller
/// re-aggregation into per-ASN totals happens in Rust, since a CIDR range
/// lookup isn't a natural fit for a SQL GROUP BY without writing a custom
/// DataFusion UDF - and the row count by this point is small enough
/// (distinct IPs, not raw flows) that a HashMap pass over it is cheap.
async fn asn_stats(State(state): State<Arc<AppState>>, Query(params): Query<AsnStatsParams>) -> impl IntoResponse {
    let exclude = match parse_asn_list(&params.exclude_asns) {
        Ok(set) => set,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid exclude_asns: {e}")})))
                .into_response()
        }
    };

    let start_ns = params.start.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = params.end.timestamp_nanos_opt().unwrap_or(0);
    let addr_col = if params.direction == AsnDirection::Outbound { "dst_addr" } else { "src_addr" };

    let sql = format!(
        "SELECT {addr_col} AS addr, SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
         FROM {FLOWS_TABLE} \
         WHERE time_flow_start_ns <= {end_ns} \
           AND time_flow_start_ns >= {start_ns} \
         GROUP BY {addr_col}"
    );

    let batches = match run_sql(&state.ctx, &sql).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let stats = aggregate_by_asn(&batches, &state.asn_db, &exclude, params.top);

    Json(json!({
        "query": {
            "direction": if params.direction == AsnDirection::Outbound { "outbound" } else { "inbound" },
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "top": params.top,
            "exclude_asns": exclude,
        },
        "asn_stats": stats,
    }))
    .into_response()
}

/// Comma-separated ASN numbers (e.g. `exclude_asns`) - blank/empty input
/// parses to an empty set rather than an error, so the param can be
/// omitted or left blank with no special-casing at the call site.
fn parse_asn_list(raw: &str) -> Result<std::collections::HashSet<u32>, std::num::ParseIntError> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>())
        .collect()
}

/// Shared by /v1/asn-stats and /v1/asn-peer-stats: collapse a RecordBatch of
/// (addr, total_bytes, total_packets, flow_count) rows - one row per
/// distinct IP, already summed by DataFusion - into per-ASN totals via a
/// CIDR-range lookup per row, sorted by bytes descending. No `exclude`/`top`
/// applied here - see `rank_asn_stats`, which is deliberately a separate,
/// cheap step so asn_peer_stats can cache this (the expensive part) keyed
/// on everything except exclude/top, and still apply either fresh per
/// request without needing a cache entry per exclude/top combination.
fn aggregate_by_asn_full(batches: &[datafusion::arrow::record_batch::RecordBatch], asn_db: &AsnDb) -> Vec<AsnStat> {
    let mut per_asn: HashMap<u32, (String, String, i64, i64, i64)> = HashMap::new();
    for batch in batches {
        use datafusion::arrow::array::*;
        let addr = batch.column_by_name("addr");
        let total_bytes = batch.column_by_name("total_bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let total_packets = batch.column_by_name("total_packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let flow_count = batch.column_by_name("flow_count").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

        for i in 0..batch.num_rows() {
            let Some(raw) = addr.and_then(|a| binary_at(a, i)) else { continue };
            let Some(ip) = bytes_to_ip(&raw) else { continue };
            let Some(info) = asn_db.lookup(ip) else { continue };

            let entry = per_asn.entry(info.asn).or_insert_with(|| (info.org.clone(), info.country.clone(), 0, 0, 0));
            entry.2 += total_bytes.map(|a| a.value(i)).unwrap_or(0);
            entry.3 += total_packets.map(|a| a.value(i)).unwrap_or(0);
            entry.4 += flow_count.map(|a| a.value(i)).unwrap_or(0);
        }
    }

    let mut stats: Vec<AsnStat> = per_asn
        .into_iter()
        .map(|(asn, (org, country, total_bytes, total_packets, flow_count))| AsnStat {
            asn,
            org,
            country,
            total_bytes,
            total_packets,
            flow_count,
        })
        .collect();
    stats.sort_by_key(|s| std::cmp::Reverse(s.total_bytes));
    stats
}

/// Drop excluded ASNs and keep the top N - the cheap, always-fresh-per-
/// request step over `aggregate_by_asn_full`'s (possibly cached) output.
/// `full` is assumed already sorted by bytes descending.
fn rank_asn_stats(full: &[AsnStat], exclude: &std::collections::HashSet<u32>, top: usize) -> Vec<AsnStat> {
    full.iter().filter(|s| !exclude.contains(&s.asn)).take(top).cloned().collect()
}

fn aggregate_by_asn(
    batches: &[datafusion::arrow::record_batch::RecordBatch],
    asn_db: &AsnDb,
    exclude: &std::collections::HashSet<u32>,
    top: usize,
) -> Vec<AsnStat> {
    rank_asn_stats(&aggregate_by_asn_full(batches, asn_db), exclude, top)
}

// ---------- tier 4: peer-ASN analysis (traffic between one ASN's own
// known prefixes and every other ASN) ----------

#[derive(Deserialize)]
struct AsnPeerStatsParams {
    /// Comma-separated CIDR list for the ASN being viewed (its known
    /// prefixes, e.g. from Netbox) - the "near" side of every flow this
    /// endpoint looks at, distinct from the public ip2asn database used to
    /// classify the "far" side.
    prefixes: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default = "default_asn_top")]
    top: usize,
    /// Comma-separated ASNs to drop from the results entirely (e.g. a
    /// peering partner the caller doesn't want counted).
    #[serde(default)]
    exclude_asns: String,
}

/// Shared by /v1/asn-peer-stats and /v1/asn-peer-timeseries: parse the
/// `prefixes` param into a validated, non-empty CIDR list.
fn parse_prefixes(raw: &str) -> Result<Vec<IpNet>, String> {
    let prefixes: Result<Vec<IpNet>, _> = raw.split(',').map(|p| p.trim().parse::<IpNet>()).collect();
    match prefixes {
        Ok(p) if !p.is_empty() => Ok(p),
        Ok(_) => Err("prefixes must not be empty".to_string()),
        Err(e) => Err(format!("invalid prefix: {e}")),
    }
}

/// `(year = 'YYYY' AND month = 'MM' AND day = 'DD') OR ...` covering every
/// calendar day [start, end] touches, padded by one full day on each side.
///
/// A first version without the padding shipped briefly and silently
/// dropped real data: parquet-datalake's `partition_path()` (see
/// goflow2/parquet-datalake/src/main.rs) assigns a file's directory from
/// the *write-window's* start time, not each flow's own
/// `time_flow_start_ns` - a 5-minute flush window straddling midnight is
/// written entirely under the earlier day, even though some of its flows
/// timestamp into the next day. A query for e.g. 00:00-00:15 filtered to
/// just that day's partition missed exactly those spillover flows -
/// confirmed by asn-peer-stats' inbound total for that window going from
/// real, populated results to empty once the (unpadded) filter was added.
/// One day of padding on each side comfortably covers that spillover
/// (bounded by the flush window, a few minutes at most) without needing
/// exact knowledge of the write-time/flow-time skew, at the cost of
/// scanning up to 2 extra days - still a real reduction against this
/// archive's multi-day retention, just less dramatic than the unsafe
/// unpadded version would have been.
///
/// Without this at all, a query only filtering on time_flow_start_ns has
/// DataFusion list and check every file across the *entire* archive
/// before row-group statistics get a chance to skip irrelevant data -
/// with thousands of files accumulated over several days' retention,
/// that per-file listing/metadata overhead was a meaningful share of a
/// 2-hour asn-peer-timeseries query's ~110s runtime pre-padding. Only
/// wired into the two endpoints built for the "day chart" feature so far
/// (asn_peer_stats, asn_peer_timeseries) - the older tier 1-3 endpoints
/// (nat_lookup, flows, traffic_stats, asn_stats) would likely benefit the
/// same way, but weren't touched here to keep this change scoped to what
/// needed it.
fn day_partition_filter(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let start_date = start.date_naive() - Duration::days(1);
    let end_date = end.date_naive() + Duration::days(1);
    let num_days = (end_date - start_date).num_days().max(0);

    (0..=num_days)
        .filter_map(|i| start_date.checked_add_signed(Duration::days(i)))
        .map(|d| format!("(year = '{:04}' AND month = '{:02}' AND day = '{:02}')", d.year(), d.month(), d.day()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// ASN-level breakdown of traffic between one ASN's own known prefixes
/// (`prefixes`, not looked up from the ip2asn database - the caller already
/// knows these, e.g. from Netbox) and every other ASN. Two directions in
/// one response, both scoped to the same prefix list:
/// - `outbound`: `prefixes` as source, grouped by destination ASN - who
///   this ASN is sending traffic to.
/// - `inbound`: `prefixes` as destination, grouped by source ASN - who is
///   sending traffic to this ASN.
///
/// Unlike /v1/asn-stats (which aggregates every flow in the archive),
/// this narrows the scan to just the flows touching the given ASN's own
/// address space on one side. A first version expressed that as a SQL
/// `WHERE addr BETWEEN X'..' AND X'..' OR ...` per prefix - correct, but it
/// doesn't scale: DataFusion evaluates a big OR chain per row with no way
/// to turn it into a binary search, and a ~300-prefix list (a real ASN's
/// full known range, even after collapsing adjacent CIDRs) took over three
/// minutes against a 15-minute window and didn't finish. Registering a
/// one-off `cidr_match_<n>` scalar UDF per request instead - backed by the
/// same sorted-range binary search `asn::AsnDb` already uses for its (much
/// smaller) override list - turns "is this row's address in the prefix
/// list" into an O(log prefixes) check per row instead of O(prefixes).
/// A unique per-request name avoids two concurrent requests racing on
/// `SessionContext::register_udf`, which registers into state shared
/// across all requests; `deregister_udf` cleans it up again once both
/// queries below are done (including on an error return) so the registry
/// doesn't grow unbounded across the process's lifetime.
///
/// The full (pre-exclude/top) per-ASN breakdown is cached in
/// `state.peer_stats_cache`, keyed on prefixes+time range (see
/// `cache_key`) - a repeat request that only changes `top` or
/// `exclude_asns` skips the SQL/UDF work entirely. A window that's fully
/// elapsed (see `window_is_closed`) is cached forever, since it can't
/// produce different data later; a window still touching "now" gets a
/// short TTL so newly-arrived flows show up reasonably promptly.
async fn asn_peer_stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AsnPeerStatsParams>,
) -> impl IntoResponse {
    let prefixes = match parse_prefixes(&params.prefixes) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    let exclude = match parse_asn_list(&params.exclude_asns) {
        Ok(set) => set,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid exclude_asns: {e}")})))
                .into_response()
        }
    };

    let start_ns = params.start.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = params.end.timestamp_nanos_opt().unwrap_or(0);
    let key = cache_key(&prefixes, start_ns, end_ns, None);

    let cached = {
        let guard = state.peer_stats_cache.lock().unwrap();
        guard.get(&key).filter(|e| e.is_fresh()).map(|e| (e.outbound.clone(), e.inbound.clone()))
    };

    let (outbound_full, inbound_full) = match cached {
        Some(full) => full,
        None => {
            let day_filter = day_partition_filter(params.start, params.end);

            let udf_id = state.next_udf_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let udf_name = format!("cidr_match_{udf_id}");
            let ranges = Arc::new(PrefixRanges::new(&prefixes));
            state.ctx.register_udf(ScalarUDF::from(CidrMatchUdf {
                name: udf_name.clone(),
                signature: Signature::any(1, Volatility::Immutable),
                ranges,
            }));

            let outbound_sql = format!(
                "SELECT dst_addr AS addr, SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
                 FROM {FLOWS_TABLE} \
                 WHERE time_flow_start_ns <= {end_ns} AND time_flow_start_ns >= {start_ns} \
                   AND ({day_filter}) \
                   AND {udf_name}(src_addr) \
                 GROUP BY dst_addr"
            );
            let inbound_sql = format!(
                "SELECT src_addr AS addr, SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
                 FROM {FLOWS_TABLE} \
                 WHERE time_flow_start_ns <= {end_ns} AND time_flow_start_ns >= {start_ns} \
                   AND ({day_filter}) \
                   AND {udf_name}(dst_addr) \
                 GROUP BY src_addr"
            );

            let outbound_batches = match run_sql(&state.ctx, &outbound_sql).await {
                Ok(b) => b,
                Err(resp) => {
                    state.ctx.deregister_udf(&udf_name);
                    return resp;
                }
            };
            let inbound_batches = match run_sql(&state.ctx, &inbound_sql).await {
                Ok(b) => b,
                Err(resp) => {
                    state.ctx.deregister_udf(&udf_name);
                    return resp;
                }
            };
            state.ctx.deregister_udf(&udf_name);

            let outbound_full = aggregate_by_asn_full(&outbound_batches, &state.asn_db);
            let inbound_full = aggregate_by_asn_full(&inbound_batches, &state.asn_db);

            cache_insert(
                &state.peer_stats_cache,
                key,
                CacheEntry {
                    outbound: outbound_full.clone(),
                    inbound: inbound_full.clone(),
                    expires_at: cache_expiry(params.end),
                },
            );

            (outbound_full, inbound_full)
        }
    };

    let outbound = rank_asn_stats(&outbound_full, &exclude, params.top);
    let inbound = rank_asn_stats(&inbound_full, &exclude, params.top);

    Json(json!({
        "query": {
            "prefixes": prefixes.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "top": params.top,
            "exclude_asns": exclude,
        },
        "outbound": outbound,
        "inbound": inbound,
    }))
    .into_response()
}

/// Scalar UDF wrapping one request's `PrefixRanges` - see asn_peer_stats'
/// doc comment for why this replaced a plain SQL OR chain.
#[derive(Debug)]
struct CidrMatchUdf {
    name: String,
    signature: Signature,
    ranges: Arc<PrefixRanges>,
}

impl ScalarUDFImpl for CidrMatchUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> DFResult<DataType> {
        Ok(DataType::Boolean)
    }

    fn invoke_batch(&self, args: &[ColumnarValue], number_rows: usize) -> DFResult<ColumnarValue> {
        let array = args[0].clone().into_array(number_rows)?;
        let mut builder = BooleanBuilder::with_capacity(array.len());
        for i in 0..array.len() {
            match binary_at(array.as_ref(), i) {
                Some(bytes) => builder.append_value(self.ranges.contains_bytes(&bytes)),
                None => builder.append_null(),
            }
        }
        Ok(ColumnarValue::Array(Arc::new(builder.finish())))
    }
}

#[derive(Deserialize)]
struct AsnPeerTimeseriesParams {
    /// Same meaning as asn_peer_stats' `prefixes`.
    prefixes: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default = "default_bucket_secs")]
    bucket_secs: i64,
    #[serde(default = "default_asn_top")]
    top: usize,
    #[serde(default)]
    exclude_asns: String,
}

#[derive(Serialize, Clone)]
struct AsnTimeseriesPoint {
    bucket_start: String,
    asn: u32,
    org: String,
    country: String,
    total_bytes: i64,
    total_packets: i64,
    flow_count: i64,
}

/// Time-bucketed version of asn_peer_stats, for a stacked-over-time chart
/// (e.g. a day view): same prefix-scoped outbound/inbound breakdown, but
/// grouped by (time bucket, far-side ASN) instead of just far-side ASN.
///
/// The top N ASNs are chosen by their TOTAL over the *whole* window, not
/// per-bucket - ranking per-bucket independently would let a different set
/// of ASNs appear in each bucket, which makes an incoherent stacked chart
/// (series popping in and out). Every (top-N ASN, bucket) pair is emitted
/// even when that ASN had zero traffic in a given bucket, so the client
/// can pivot this directly into fixed-length per-ASN series (one point per
/// bucket, in order) without handling gaps - the same shape the old
/// CSV-based netflow_build_figure_csv pivoted from pandas.
/// Same caching strategy as asn_peer_stats (see its doc comment) - the
/// full (all ASNs, zero-filled across all buckets, no exclude/top applied)
/// breakdown is cached in `state.peer_timeseries_cache` keyed on
/// prefixes+time range+bucket size; `rank_asn_timeseries` (exclude + top +
/// re-flatten in rank order) runs fresh every request over either the
/// cached or freshly-queried full data.
async fn asn_peer_timeseries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AsnPeerTimeseriesParams>,
) -> impl IntoResponse {
    let prefixes = match parse_prefixes(&params.prefixes) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    let exclude = match parse_asn_list(&params.exclude_asns) {
        Ok(set) => set,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid exclude_asns: {e}")})))
                .into_response()
        }
    };

    let start_ns = params.start.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = params.end.timestamp_nanos_opt().unwrap_or(0);
    let bucket_ns = params.bucket_secs.max(1) * 1_000_000_000;
    let key = cache_key(&prefixes, start_ns, end_ns, Some(params.bucket_secs));

    let cached = {
        let guard = state.peer_timeseries_cache.lock().unwrap();
        guard.get(&key).filter(|e| e.is_fresh()).map(|e| (e.outbound.clone(), e.inbound.clone()))
    };

    let (outbound_full, inbound_full) = match cached {
        Some(full) => full,
        None => {
            let day_filter = day_partition_filter(params.start, params.end);

            let udf_id = state.next_udf_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let udf_name = format!("cidr_match_{udf_id}");
            let ranges = Arc::new(PrefixRanges::new(&prefixes));
            state.ctx.register_udf(ScalarUDF::from(CidrMatchUdf {
                name: udf_name.clone(),
                signature: Signature::any(1, Volatility::Immutable),
                ranges,
            }));

            let outbound_sql = format!(
                "SELECT (time_flow_start_ns / {bucket_ns}) * {bucket_ns} AS bucket_start_ns, \
                        dst_addr AS addr, SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
                 FROM {FLOWS_TABLE} \
                 WHERE time_flow_start_ns <= {end_ns} AND time_flow_start_ns >= {start_ns} \
                   AND ({day_filter}) \
                   AND {udf_name}(src_addr) \
                 GROUP BY bucket_start_ns, dst_addr"
            );
            let inbound_sql = format!(
                "SELECT (time_flow_start_ns / {bucket_ns}) * {bucket_ns} AS bucket_start_ns, \
                        src_addr AS addr, SUM(bytes) AS total_bytes, SUM(packets) AS total_packets, COUNT(*) AS flow_count \
                 FROM {FLOWS_TABLE} \
                 WHERE time_flow_start_ns <= {end_ns} AND time_flow_start_ns >= {start_ns} \
                   AND ({day_filter}) \
                   AND {udf_name}(dst_addr) \
                 GROUP BY bucket_start_ns, src_addr"
            );

            let outbound_batches = match run_sql(&state.ctx, &outbound_sql).await {
                Ok(b) => b,
                Err(resp) => {
                    state.ctx.deregister_udf(&udf_name);
                    return resp;
                }
            };
            let inbound_batches = match run_sql(&state.ctx, &inbound_sql).await {
                Ok(b) => b,
                Err(resp) => {
                    state.ctx.deregister_udf(&udf_name);
                    return resp;
                }
            };
            state.ctx.deregister_udf(&udf_name);

            let outbound_full = aggregate_by_asn_timeseries_full(&outbound_batches, &state.asn_db);
            let inbound_full = aggregate_by_asn_timeseries_full(&inbound_batches, &state.asn_db);

            cache_insert(
                &state.peer_timeseries_cache,
                key,
                CacheEntry {
                    outbound: outbound_full.clone(),
                    inbound: inbound_full.clone(),
                    expires_at: cache_expiry(params.end),
                },
            );

            (outbound_full, inbound_full)
        }
    };

    let outbound = rank_asn_timeseries(&outbound_full, &exclude, params.top);
    let inbound = rank_asn_timeseries(&inbound_full, &exclude, params.top);

    Json(json!({
        "query": {
            "prefixes": prefixes.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "bucket_secs": params.bucket_secs,
            "top": params.top,
            "exclude_asns": exclude,
        },
        "outbound": outbound,
        "inbound": inbound,
    }))
    .into_response()
}

/// Same per-row (addr -> ASN) resolution as aggregate_by_asn_full, but
/// keyed by (asn, bucket) instead of just asn, and zero-filled across
/// every distinct bucket seen for every ASN (not just the top-N ones -
/// that filtering happens in `rank_asn_timeseries`, kept separate for the
/// same cacheability reason `aggregate_by_asn_full`/`rank_asn_stats` are).
fn aggregate_by_asn_timeseries_full(
    batches: &[datafusion::arrow::record_batch::RecordBatch],
    asn_db: &AsnDb,
) -> Vec<AsnTimeseriesPoint> {
    use datafusion::arrow::array::*;

    let mut per_asn: HashMap<u32, (String, String, HashMap<i64, (i64, i64, i64)>)> = HashMap::new();
    let mut bucket_set: Vec<i64> = Vec::new();

    for batch in batches {
        let bucket_col = batch.column_by_name("bucket_start_ns").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let addr = batch.column_by_name("addr");
        let total_bytes = batch.column_by_name("total_bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let total_packets = batch.column_by_name("total_packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let flow_count = batch.column_by_name("flow_count").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

        for i in 0..batch.num_rows() {
            let Some(raw) = addr.and_then(|a| binary_at(a, i)) else { continue };
            let Some(ip) = bytes_to_ip(&raw) else { continue };
            let Some(info) = asn_db.lookup(ip) else { continue };
            let Some(bucket_ns) = bucket_col.map(|a| a.value(i)) else { continue };
            let bytes = total_bytes.map(|a| a.value(i)).unwrap_or(0);
            let packets = total_packets.map(|a| a.value(i)).unwrap_or(0);
            let flows = flow_count.map(|a| a.value(i)).unwrap_or(0);

            bucket_set.push(bucket_ns);
            let entry =
                per_asn.entry(info.asn).or_insert_with(|| (info.org.clone(), info.country.clone(), HashMap::new()));
            let bucket_entry = entry.2.entry(bucket_ns).or_insert((0, 0, 0));
            bucket_entry.0 += bytes;
            bucket_entry.1 += packets;
            bucket_entry.2 += flows;
        }
    }

    bucket_set.sort_unstable();
    bucket_set.dedup();

    let mut points = Vec::new();
    for (asn, (org, country, buckets)) in &per_asn {
        for &bucket_ns in &bucket_set {
            let (bytes, packets, flows) = buckets.get(&bucket_ns).copied().unwrap_or((0, 0, 0));
            let bucket_start = DateTime::<Utc>::from_timestamp(bucket_ns / 1_000_000_000, (bucket_ns % 1_000_000_000) as u32)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            points.push(AsnTimeseriesPoint {
                bucket_start,
                asn: *asn,
                org: org.clone(),
                country: country.clone(),
                total_bytes: bytes,
                total_packets: packets,
                flow_count: flows,
            });
        }
    }
    points
}

/// Drop excluded ASNs, rank the rest by total bytes across all buckets,
/// and keep the top N - the cheap, always-fresh-per-request step over
/// `aggregate_by_asn_timeseries_full`'s (possibly cached) output. Each
/// selected ASN's points are emitted together, in their original
/// (bucket-ordered) relative order, ranked ASN first.
fn rank_asn_timeseries(
    full: &[AsnTimeseriesPoint],
    exclude: &std::collections::HashSet<u32>,
    top: usize,
) -> Vec<AsnTimeseriesPoint> {
    let mut totals: HashMap<u32, i64> = HashMap::new();
    for p in full {
        if exclude.contains(&p.asn) {
            continue;
        }
        *totals.entry(p.asn).or_insert(0) += p.total_bytes;
    }

    let mut ranked_asns: Vec<(u32, i64)> = totals.into_iter().collect();
    ranked_asns.sort_by_key(|&(_, total)| std::cmp::Reverse(total));
    ranked_asns.truncate(top);

    let mut result = Vec::new();
    for (asn, _) in &ranked_asns {
        result.extend(full.iter().filter(|p| p.asn == *asn).cloned());
    }
    result
}

fn bytes_to_ip(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::V4(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))),
        16 => {
            let arr: [u8; 16] = bytes.try_into().ok()?;
            Some(IpAddr::V6(std::net::Ipv6Addr::from(arr)))
        }
        _ => None,
    }
}

// ---------- shared query helper ----------

async fn run_sql(
    ctx: &SessionContext,
    sql: &str,
) -> Result<Vec<datafusion::arrow::record_batch::RecordBatch>, axum::response::Response> {
    let df = ctx.sql(sql).await.map_err(|e| {
        tracing::error!("query planning failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    df.collect().await.map_err(|e| {
        tracing::error!("query execution failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })
}

fn fmt_ip(bytes: &[u8]) -> String {
    match bytes.len() {
        4 => IpAddr::V4(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])).to_string(),
        16 => {
            let arr: [u8; 16] = bytes.try_into().unwrap_or([0; 16]);
            IpAddr::V6(std::net::Ipv6Addr::from(arr)).to_string()
        }
        _ => hex::encode(bytes),
    }
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}
