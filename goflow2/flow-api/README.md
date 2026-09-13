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

### `GET /v1/asn-peer-stats`

ASN-level breakdown of traffic between one ASN's own known prefixes and
every other ASN - "who is this ASN sending traffic to / receiving traffic
from" for a specific network the caller already knows the address space
of (e.g. from Netbox), rather than the whole archive.

Params: `prefixes` (comma-separated CIDR list), `start`, `end` (RFC3339), `top` (default 20).

```
curl 'http://localhost:8090/v1/asn-peer-stats?prefixes=10.2.19.0/24,10.2.20.0/24&start=2026-09-12T00:00:00Z&end=2026-09-12T00:15:00Z&top=5'
```

Response has both directions, both scoped to the same prefix list:
- `outbound`: `prefixes` as source, grouped by destination ASN.
- `inbound`: `prefixes` as destination, grouped by source ASN.

Unlike `/v1/asn-stats` (aggregates the whole archive), this narrows the
scan to flows touching the given prefixes on one side. A first version
expressed that as a SQL `WHERE addr BETWEEN X'..' AND X'..' OR ...` per
prefix - correct, but it doesn't scale: DataFusion evaluates a big OR
chain per row with no way to turn it into a binary search, and a
~300-prefix list (a real ASN's full known range, even after collapsing
adjacent CIDRs) took over three minutes against a 15-minute window and
didn't finish. It's a one-off `cidr_match_<n>` scalar UDF per request now
instead - backed by the same sorted-range binary search `asn::AsnDb`
already uses for its (much smaller) override list - which turns "is this
row's address in the prefix list" into an O(log prefixes) check per row.
A unique per-request UDF name avoids concurrent requests racing on
`SessionContext::register_udf` (registers into state shared across all
requests); `deregister_udf` cleans it up again afterward either way, so
the registry doesn't grow unbounded across the process's lifetime. Same
~300-prefix/15-minute case: ~30s after the rewrite.

Also takes an optional `exclude_asns` (comma-separated ASN numbers,
default empty) - dropped from the results before ranking, so an excluded
ASN never displaces a real top-N entry. Useful for excluding a peering
partner from a capacity-planning view.

### `GET /v1/asn-peer-timeseries`

Time-bucketed version of `/v1/asn-peer-stats`, for a stacked-over-time
chart (e.g. a day view): same prefix-scoped outbound/inbound breakdown,
grouped by (time bucket, far-side ASN) instead of just far-side ASN.

Params: same as `/v1/asn-peer-stats`, plus `bucket_secs` (default 300).

```
curl 'http://localhost:8090/v1/asn-peer-timeseries?prefixes=10.2.19.0/24&start=2026-09-12T00:00:00Z&end=2026-09-13T00:00:00Z&bucket_secs=3600&top=15'
```

The top N ASNs are chosen by their TOTAL over the *whole* window, not
per-bucket - ranking per-bucket independently would let a different set
of ASNs appear in each bucket, making an incoherent stacked chart (series
popping in and out). Every (top-N ASN, bucket) pair is emitted even when
that ASN had zero traffic in a given bucket, so the client can pivot this
directly into fixed-length per-ASN series without handling gaps.

**Both endpoints add a Hive partition filter** (`year=`/`month=`/`day=`)
alongside the `time_flow_start_ns` bounds, padded by one full day on each
side. Without it, DataFusion lists and checks every file across the
*entire* archive (not just the requested window) before row-group
statistics get a chance to skip irrelevant data - with several days'
worth of 5-minute files accumulated, that overhead alone dominated a
2-hour `asn-peer-timeseries` query's runtime. **The padding matters and
is not optional**: a first version filtered to just the requested day(s)
with no padding, and it silently dropped real data - `parquet-datalake`'s
`partition_path()` (see `goflow2/parquet-datalake/src/main.rs`) assigns a
file's directory from the *write-window's* start time, not each flow's
own `time_flow_start_ns`, so a 5-minute flush window straddling midnight
is written entirely under the earlier day even though some of its flows
timestamp into the next day. A query for `00:00`-`00:15` filtered to only
that day's partition missed exactly those spillover flows - caught by
comparing `asn-peer-stats`' output for that exact window before and after
adding the filter (inbound went from real, populated results to empty).
One day of padding comfortably covers that spillover (bounded by the
flush window) without needing exact knowledge of the write-time/flow-time
skew, at the cost of scanning up to 2 extra days.

**Known remaining cost**: for a full day at real production volume (this
network's own full prefix list, ~295 CIDRs after collapsing, against a
day with hundreds of millions of raw flow rows), `asn-peer-timeseries`
is CPU-bound (confirmed via a 400%+, multi-core-pegged process, not
blocked on I/O) and can take several minutes - the per-row CIDR-match UDF
call still has to run once per raw row before any grouping happens, and
that per-row cost times the row count dominates once file-listing
overhead is no longer the bottleneck. Not yet addressed; a next step
would be pushing the CIDR check down as a native vectorized DataFusion
operator instead of a scalar UDF, or accepting this as a background/
async job rather than a synchronous request for the heaviest case.

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
