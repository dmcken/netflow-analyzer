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

use std::{collections::HashMap, net::IpAddr, path::PathBuf, sync::Arc};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use datafusion::{
    arrow::datatypes::DataType,
    datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl},
    prelude::*,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use asn::AsnDb;

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    let ctx = SessionContext::new();
    register_flows_table(&ctx, &args.data_dir).await?;
    let asn_db = AsnDb::load(&args.ip2asn_path, &args.override_path)?;

    let state = Arc::new(AppState { ctx, asn_db });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/nat-lookup", get(nat_lookup))
        .route("/v1/flows", get(flows))
        .route("/v1/traffic-stats", get(traffic_stats))
        .route("/v1/asn-stats", get(asn_stats))
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

#[derive(Serialize)]
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

    let mut per_asn: HashMap<u32, (String, String, i64, i64, i64)> = HashMap::new();
    for batch in &batches {
        use datafusion::arrow::array::*;
        let addr = batch.column_by_name("addr");
        let total_bytes = batch.column_by_name("total_bytes").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let total_packets = batch.column_by_name("total_packets").and_then(|c| c.as_any().downcast_ref::<Int64Array>());
        let flow_count = batch.column_by_name("flow_count").and_then(|c| c.as_any().downcast_ref::<Int64Array>());

        for i in 0..batch.num_rows() {
            let Some(raw) = addr.and_then(|a| binary_at(a, i)) else { continue };
            let Some(ip) = bytes_to_ip(&raw) else { continue };
            let Some(info) = state.asn_db.lookup(ip) else { continue };

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
    stats.truncate(params.top);

    Json(json!({
        "query": {
            "direction": if params.direction == AsnDirection::Outbound { "outbound" } else { "inbound" },
            "start": params.start.to_rfc3339(),
            "end": params.end.to_rfc3339(),
            "top": params.top,
        },
        "asn_stats": stats,
    }))
    .into_response()
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
