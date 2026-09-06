# Parquet data lake: goflow2 -> FIFO -> Parquet, directly

Replaces the `protobuf-archive` pipeline (goflow2 -> raw `.log` file ->
15-min SIGHUP rotation -> `pbzip2 -9 -p8`) with a single continuous step:
goflow2 writes its binary transport output to a named pipe, and `pq-consumer`
reads it, decodes it, and flushes a Parquet file on a fixed interval.

## Why

- **No `pbzip2` CPU cost.** The old pipeline runs `pbzip2 -9 -p8` over a
  multi-GB raw file every 15 minutes, on the same box running the collector.
  This pipeline compresses incrementally (zstd, inside the Parquet writer)
  as data arrives - no separate whole-file compression pass.
- **No SIGHUP rotation, no "file already closed" race.** The two bugs fixed
  in the goflow2 fork (`fix/atomic-file-transport-write`) both came from the
  file-transport's rotate-via-`kill -1` mechanism. This pipeline never
  rotates a file - there's nothing to rotate. (The fork fixes are still
  correct and worth landing upstream for anyone using `-transport=file`
  with `workers>1`; they're just no longer load-bearing here.)
- **The FIFO never touches disk.** It's a kernel-buffered pipe. The only
  disk write in the whole pipeline is the final, already-compressed Parquet
  file - which incidentally also answers the "should the hot path live on
  Optane/RAM" question from an earlier discussion: there isn't a hot-path
  disk write to move anymore.
- **Columns are trimmed at the source of truth**, not left to whatever
  goflow2's fixed `FlowMessage` schema happens to include. See "Columns"
  below for what's kept and why - it's the same set validated against a
  field-presence scan on real production captures.

## Architecture

```
goflow2 (fork, workers=16)  --protobuf-->  FIFO  --read()-->  pq-consumer
  -transport=file                     (kernel pipe,              |
  -transport.file=.../goflow2.fifo     never touches disk)       v
                                                     decode (zero-alloc scan)
                                                     -> Arrow builders
                                                     -> sort by time_flow_start_ns
                                                     -> Parquet + zstd, every 5 min
                                                     -> /mnt/.../parquet/year=/month=/day=/
```

`pq-consumer` decodes frames by hand-scanning the protobuf wire format
rather than using generated protobuf bindings. The generated (prost)
decoder allocates a fresh heap `Vec<u8>` for every `bytes`-typed field
(`src_addr`, `dst_addr`, `sampler_address`, `next_hop`) on every decode
call; at this record rate that was the dominant cost in an earlier
prototype (~20 minutes to convert one 15-minute window - see git history /
conversation notes for the profiling that found this). A FIFO reader that
falls behind applies backpressure all the way back to goflow2's `Send()`
calls, risking the same packet-drop failure mode the `workers=16` fix
was for - so decode here must never be the bottleneck. The scanner
borrows address fields as slices directly from the frame buffer; Arrow's
builder copies them exactly once, when appending.

## Columns

Kept (see the field-presence scan from real production captures):
`time_received_ns`, `time_flow_start_ns`, `time_flow_end_ns`,
`sampler_address`, `src_addr`, `dst_addr`, `bytes`, `packets`, `proto`,
`src_port`, `dst_port`, `in_if`, `out_if`, `tcp_flags`.

Dropped: `type`, `sequence_num`, `next_hop` (transport/routing artifacts,
not flow identity), `etype` (100% derivable from address byte length),
`src_mac`/`dst_mac` (router MACs, no investigative value), `ipv6_flow_label`,
`ip_tos` (sparse, low signal), and everything confirmed at 0% presence in
this deployment (`sampling_rate`, VLAN fields, ICMP fields, `ip_ttl`, the
custom vrf/direction/csum fields, AS/net fields, fragment fields, and the
post-NAT/NAPT fields - see the separate open question about whether the
CG-NAT router is actually exporting those at all).

## Setup

```bash
./setup.sh                    # creates the FIFO and Parquet output directories
cargo build --release         # builds target/release/pq-consumer
```

Deploy the built binary and both config files to the run location (see
top-level repo convention: build in the repo, deploy to `~/`):

```bash
mkdir -p ~/parquet-datalake
cp target/release/pq-consumer ~/parquet-datalake/
cp docker-compose.yml field-map.yaml ~/parquet-datalake/
sudo cp pq-consumer.service /etc/systemd/system/
sudo systemctl daemon-reload
```

### Startup order matters

`pq-consumer` creates the FIFO if it doesn't exist and opens it for
reading, which blocks until a writer connects - so it's safe to start
first. goflow2 opens the FIFO for writing; if `pq-consumer` isn't running
yet, goflow2's `open()` will succeed immediately (FIFOs don't require a
reader to be present to open for writing on Linux, but writes will block
until a reader attaches). Recommended order:

```bash
sudo systemctl start pq-consumer
cd ~/parquet-datalake && docker compose up -d
```

### Stopping / restarting

`pq-consumer` handles SIGTERM/SIGINT by flushing its current in-progress
batch before exiting - `systemctl stop pq-consumer` is safe and won't lose
the partial window. If goflow2's container restarts (image update, etc.),
`pq-consumer` detects the FIFO writer closing (EOF), logs it, and
transparently reopens the FIFO to wait for the new writer - no restart of
the consumer needed.

## Querying

Parquet files are Hive-partitioned (`year=/month=/day=/`), directly
queryable with DuckDB, no import step:

```sql
SELECT * FROM read_parquet('/mnt/netflow/airlink-logs/parquet/**/*.parquet')
WHERE src_addr = ? AND time_flow_start_ns BETWEEN ? AND ?;
```

Address columns are raw 4 or 16 byte binary (not pretty-printed strings) -
smaller on disk, cast to text at query time if needed.
