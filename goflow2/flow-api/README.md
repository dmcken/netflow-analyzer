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

### `GET /v1/flows`

Raw matching flow records for an IP (as either src or dst) over a time
range - "what connections did this IP have" for security audit or
connectivity troubleshooting.

Params: `ip`, `start`, `end` (RFC3339), `limit` (default 100, capped at 1000).

```
curl 'http://localhost:8090/v1/flows?ip=10.2.19.230&start=2026-09-06T12:55:00Z&end=2026-09-06T13:00:00Z&limit=3'
```

**Important semantic note**: query by the actual endpoint IP you care
about, not a shared CGNAT gateway's public IP - `src_addr`/`dst_addr` in
this schema are whatever address goflow2 observed at its sampling point,
which for CGNAT'd traffic is the *shared public* address, aggregating
every customer behind it. Querying `170.246.160.195` (a NAT pool address)
returns hundreds of thousands of flows across many different customers;
querying `10.2.19.230` (one customer's actual private IP, e.g. resolved
via `/v1/nat-lookup` first) returns just that customer's traffic.

### `GET /v1/traffic-stats`

Aggregated bytes/packets/flow count over time buckets for an IP -
"traffic stats for a particular IP" for troubleshooting tools. Same CGNAT
caveat as `/v1/flows` above applies.

Params: `ip`, `start`, `end` (RFC3339), `bucket_secs` (default 300).

```
curl 'http://localhost:8090/v1/traffic-stats?ip=10.2.19.230&start=2026-09-06T12:00:00Z&end=2026-09-06T13:00:00Z&bucket_secs=600'
```

Bucketing is done as plain integer math on `time_flow_start_ns`
(`(x / bucket_ns) * bucket_ns`) rather than through DataFusion's timestamp
functions - the column is already epoch nanoseconds, so there's no need
to cast through a `Timestamp` type just to floor it to a bucket.

### `GET /v1/asn-stats`

ASN-level traffic aggregation for capacity planning - "what networks are
our customers accessing" (`direction=outbound`, grouped by destination
ASN) or "what ASNs are accessing servers on our network"
(`direction=inbound`, grouped by source ASN).

Params: `direction` (`outbound`|`inbound`), `start`, `end` (RFC3339), `top` (default 20).

```
curl 'http://localhost:8090/v1/asn-stats?direction=outbound&start=2026-09-06T12:55:00Z&end=2026-09-06T13:00:00Z&top=10'
```

IP->ASN enrichment loads once at startup from an iptoasn.com-format TSV
(`--ip2asn-path`) plus a small YAML override list for private address
space and this network's own custom ASN blocks (`--override-path`, same
source/precedence as the original `goflow2_analysis` tool - custom
overrides checked first via longest-prefix, then the public database).
DataFusion does the heavy `GROUP BY dst_addr`/`src_addr` aggregation,
collapsing potentially billions of raw flows down to one row per distinct
IP; the IP->ASN mapping and a second, much smaller re-aggregation into
per-ASN totals happens in Rust via a sorted-range binary search - simpler
to get right than a custom DataFusion UDF for a CIDR range join, and the
row count by that point (distinct IPs, not raw flows) is small enough
that a `HashMap` pass over it is cheap. ~3.4s for a 5-minute window in
testing.

**Note on the override file**: `asn: 61478,` (trailing comma) in YAML
parses as the *string* `"61478,"`, not the integer `61478`. The original
`override.yaml` had this on every entry, and the old `goflow2_analysis`
tool's loader silently swallows YAML parse errors (`.ok().unwrap_or_default()`),
so it had been ignoring the whole override file - all of this network's
own address space was being classified by the public database instead of
the intended custom mapping, the entire time that tool existed. Fixed in
the copy `flow-api` uses (`data/override.yaml`); worth fixing upstream too
if `goflow2_analysis` is still used anywhere.

### `GET /health`

Liveness check.

## Gotcha worth knowing if you touch the query code

DataFusion doesn't always hand back binary columns in their on-disk
representation - a plain query preserves the Parquet schema's `Binary`
type, but going through operators like `UNION ALL` promotes it to
`BinaryView` (its coalesce/interleave kernels prefer the view type). The
shared `binary_at()` helper handles `Binary`, `BinaryView`, and
`LargeBinary` so this doesn't need re-discovering per endpoint - use it
for any new binary column extraction rather than downcasting to one
specific array type (this bit the first version of `/v1/nat-lookup`,
which is why the helper exists at all).

## Deployment

```bash
cargo build --release
cp target/release/flow-api ~/flow-api/
```

The ASN database isn't checked into git (a few MB, refreshed from
iptoasn.com rather than versioned):

```bash
mkdir -p ~/flow-api/data
curl -s https://iptoasn.com/data/ip2asn-combined.tsv.gz -o ~/flow-api/data/ip2asn-combined.tsv.gz
# override.yaml (custom private-range/own-ASN mappings) is checked in - copy it alongside
```

Run pointed at the live archive:

```bash
~/flow-api/flow-api \
    --data-dir /mnt/netflow/airlink-logs/parquet \
    --listen-addr 0.0.0.0:8090 \
    --ip2asn-path ~/flow-api/data/ip2asn-combined.tsv.gz \
    --override-path ~/flow-api/data/override.yaml
```

Read-only against the same directory `pq-consumer` writes into - no
coordination needed between the two processes.
