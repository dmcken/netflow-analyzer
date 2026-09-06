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

use std::{net::IpAddr, sync::Arc};

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

const FLOWS_TABLE: &str = "flows";

#[derive(Parser)]
struct Cli {
    /// Root of the Hive-partitioned Parquet archive (year=/month=/day=/*.parquet)
    #[arg(long)]
    data_dir: String,

    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:8090")]
    listen_addr: String,
}

struct AppState {
    ctx: SessionContext,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    let ctx = SessionContext::new();
    register_flows_table(&ctx, &args.data_dir).await?;

    let state = Arc::new(AppState { ctx });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/nat-lookup", get(nat_lookup))
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

// DataFusion promotes the plain Binary columns from the Parquet schema to
// BinaryView through the UNION ALL (its internal coalesce/interleave
// kernels prefer the view type), so read them as BinaryViewArray rather
// than the on-disk BinaryArray type.
fn binary_view_at(col: &dyn datafusion::arrow::array::Array, i: usize) -> Option<Vec<u8>> {
    use datafusion::arrow::array::{Array, BinaryViewArray};
    let arr = col.as_any().downcast_ref::<BinaryViewArray>()?;
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_vec())
    }
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
        let internal_ip = src_addr.and_then(|a| binary_view_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default();
        let remote_ip = dst_addr.and_then(|a| binary_view_at(a, i)).map(|b| fmt_ip(&b)).unwrap_or_default();
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
