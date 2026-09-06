# flow-api

Read-only HTTP API in front of the Parquet flow archive (`parquet-datalake`).
Other internal tools (abuse handling, troubleshooting, capacity planning)
query through this API instead of touching the storage layer directly - so
the storage backend can change later (local disk -> Ceph/S3, say, if
traffic outgrows a single box) without any consumer needing to change.

## Why DataFusion

Apache DataFusion is Arrow's own SQL engine, built on the same `arrow`
crate already used throughout this project (`pq-consumer`, `pq-verify`).
Its `object_store` backend reads Parquet from local disk, S3, and any
S3-compatible store (including Ceph RGW) through the same code path - so
moving the archive off local disk is a `--data-dir` config change (a
`file://` path becomes `s3://bucket/prefix`), not an application rewrite.
That property is the reason DataFusion was picked over embedding a
database like DuckDB (also viable, but a C++ dependency via FFI rather
than a native part of this project's existing Rust/Arrow stack).

## Endpoints

### `GET /v1/nat-lookup`

Resolve an abuse report's public IP:port at a point in time back to the
internal customer IP, via the `post_nat_*`/`post_napt_*` columns (see
`goflow2/parquet-datalake`'s README and the field-map.yaml fix that made
this data available at all).

Params: `ip`, `port`, `at` (RFC3339 timestamp), `tolerance_secs` (default 60).

Checks both directions in one query, since an abuse report doesn't tell
you which side of the flow it caught:
- **outbound**: the IP:port is the post-NAT *source* (a customer's
  outbound traffic) - returns the customer's real internal IP as
  `internal_ip`.
- **inbound**: the IP:port is the post-NAT *destination* (e.g. port
  forwarding) - same shape, translated the other way.

```
curl 'http://localhost:8090/v1/nat-lookup?ip=170.246.160.195&port=59618&at=2026-09-06T12:57:03Z&tolerance_secs=120'
```

```json
{
  "matches": [{
    "direction": "outbound",
    "internal_ip": "10.2.19.230",
    "remote_ip": "157.240.14.53",
    "remote_port": 5222,
    "proto": 6,
    "bytes": 10862, "packets": 202,
    "time_flow_start_ns": ..., "time_flow_end_ns": ...
  }],
  "query": { ... }
}
```

### `GET /health`

Liveness check.

## Not yet built

- Tier 2: point queries (`/v1/flows?ip=...`, `/v1/traffic-stats?ip=...`)
- Tier 3: ASN-level aggregation for capacity planning (needs an IP->ASN
  join, likely a DataFusion UDF against an ip2asn table)

## Gotcha worth knowing if you touch the query code

DataFusion promotes the on-disk `Binary` columns (src_addr, dst_addr, the
post-NAT address columns) to `BinaryView` when they pass through a `UNION
ALL` - its internal coalesce/interleave kernels prefer the view type. Read
them back as `BinaryViewArray`, not `BinaryArray`, or every IP will
silently come back empty (this bit the first version of this endpoint).

## Deployment

```bash
cargo build --release
cp target/release/flow-api ~/flow-api/
```

Run pointed at the live archive:

```bash
~/flow-api/flow-api --data-dir /mnt/netflow/airlink-logs/parquet --listen-addr 0.0.0.0:8090
```

Read-only against the same directory `pq-consumer` writes into - no
coordination needed between the two processes.
