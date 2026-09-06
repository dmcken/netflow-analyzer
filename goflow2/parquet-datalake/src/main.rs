//! goflow2 -> named pipe -> Parquet, direct.
//!
//! Replaces the raw-file + SIGHUP-rotation + pbzip2 pipeline with: goflow2
//! writes its binary (protobuf) transport output to a FIFO instead of a
//! regular file (no goflow2 code change needed - `-transport.file=` just
//! points at a `mkfifo`'d path), and this daemon reads the FIFO
//! continuously, decodes each length-prefixed FlowMessage frame, and
//! flushes a time-partitioned Parquet file on a fixed interval.
//!
//! Frames are decoded by hand-scanning the protobuf wire format rather than
//! using generated prost bindings. prost's default codegen allocates a
//! fresh heap Vec<u8> for every `bytes`-typed field (src_addr, dst_addr,
//! sampler_address, next_hop) on every decode call; at this record rate
//! that showed up directly as the dominant cost in an earlier prototype
//! (~20 minutes to convert one 15-minute window). A FIFO reader that falls
//! behind applies backpressure all the way back to goflow2's Send() calls
//! (see the file transport's atomic-write fix), which risks the same
//! packet-drop failure mode the workers=16 fix was for - so decode here
//! must not be the bottleneck. Scanning the wire format directly and
//! borrowing address fields as slices (never copied until Arrow's builder
//! copies them exactly once) keeps this allocation-free per record.

use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arrow::array::{
    ArrayRef, BinaryBuilder, Int64Builder, UInt16Builder, UInt32Builder, UInt8Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use clap::Parser;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

#[derive(Parser)]
struct Cli {
    /// Path to the FIFO goflow2 writes to (created with mkfifo if missing)
    #[arg(long)]
    fifo: PathBuf,

    /// Root directory for the Parquet data lake (Hive-partitioned by day)
    #[arg(long)]
    output_dir: PathBuf,

    /// Flush a Parquet file every N seconds
    #[arg(long, default_value_t = 300)]
    flush_interval_secs: u64,

    /// Zstd compression level (1-22). Measured against real captures:
    /// level 12 takes ~16x longer than level 3 for under 1.1% smaller
    /// output - Parquet's own dictionary/columnar encoding does most of the
    /// compaction work already, so higher zstd levels buy little here. 3
    /// keeps the writer comfortably faster than the flush interval.
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,
}

// ---------- zero-allocation protobuf field scanner ----------

fn read_varint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut pos = offset;
    loop {
        let byte = *data.get(pos)?;
        result |= ((byte & 0x7F) as u64) << shift;
        pos += 1;
        if byte & 0x80 == 0 {
            return Some((result, pos - offset));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Fields pulled out of one FlowMessage. Slices borrow directly from the
/// frame buffer - valid only while that frame's owning buffer is alive.
#[derive(Default)]
struct Flow<'a> {
    time_received_ns: i64,
    time_flow_start_ns: i64,
    time_flow_end_ns: i64,
    sampler_address: &'a [u8],
    src_addr: &'a [u8],
    dst_addr: &'a [u8],
    bytes: i64,
    packets: i64,
    proto: u8,
    src_port: u32,
    dst_port: u32,
    in_if: u32,
    out_if: u32,
    tcp_flags: u32,
    // CGNAT translation - the field that makes an abuse report ("public IP
    // X at time T") traceable back to a customer. Populated via a custom
    // field-map.yaml mapping (IPFIX IEs 225-228, re-emitted by goflow2 as
    // custom protobuf fields 2225-2228). Raw bytes as goflow2 emits them -
    // see the comment on post_nat_src_ipv4_address's handling below for why
    // this isn't a plain IP-length slice.
    post_nat_src_ipv4_address: &'a [u8],
    post_nat_dst_ipv4_address: &'a [u8],
    post_napt_src_transport_port: u32,
    post_napt_dst_transport_port: u32,
}

// FlowMessage field numbers, from pb/flow.proto (netsampler/goflow2).
const F_SAMPLER_ADDRESS: u64 = 11;
const F_TIME_FLOW_START_NS: u64 = 111;
const F_TIME_FLOW_END_NS: u64 = 112;
const F_BYTES: u64 = 9;
const F_PACKETS: u64 = 10;
const F_SRC_ADDR: u64 = 6;
const F_DST_ADDR: u64 = 7;
const F_PROTO: u64 = 20;
const F_SRC_PORT: u64 = 21;
const F_DST_PORT: u64 = 22;
const F_IN_IF: u64 = 18;
const F_OUT_IF: u64 = 19;
const F_TCP_FLAGS: u64 = 26;
const F_TIME_RECEIVED_NS: u64 = 110;
// Custom fields declared in field-map.yaml's `protobuf:` section.
const F_POST_NAT_SRC_IPV4: u64 = 2225;
const F_POST_NAT_DST_IPV4: u64 = 2226;
const F_POST_NAPT_SRC_PORT: u64 = 2227;
const F_POST_NAPT_DST_PORT: u64 = 2228;

/// Scan a FlowMessage's raw wire bytes into a Flow, without allocating.
/// Unknown/unused fields (type, sequence_num, next_hop, etype, MACs,
/// sampling_rate, VLAN, ICMP, TTL, the custom vrf/direction/csum/NAT
/// fields, AS/net fields, fragment fields - see the field presence scan
/// from the earlier analysis) are skipped by advancing past them.
fn scan_flow(frame: &[u8]) -> Option<Flow<'_>> {
    let mut flow = Flow::default();
    let mut offset = 0usize;
    while offset < frame.len() {
        let (tag, consumed) = read_varint(frame, offset)?;
        offset += consumed;
        let field_num = tag >> 3;
        let wire_type = tag & 0x7;
        match wire_type {
            0 => {
                let (val, consumed) = read_varint(frame, offset)?;
                offset += consumed;
                match field_num {
                    F_TIME_RECEIVED_NS => flow.time_received_ns = val as i64,
                    F_TIME_FLOW_START_NS => flow.time_flow_start_ns = val as i64,
                    F_TIME_FLOW_END_NS => flow.time_flow_end_ns = val as i64,
                    F_BYTES => flow.bytes = val as i64,
                    F_PACKETS => flow.packets = val as i64,
                    F_PROTO => flow.proto = val as u8,
                    F_SRC_PORT => flow.src_port = val as u32,
                    F_DST_PORT => flow.dst_port = val as u32,
                    F_IN_IF => flow.in_if = val as u32,
                    F_OUT_IF => flow.out_if = val as u32,
                    F_TCP_FLAGS => flow.tcp_flags = val as u32,
                    F_POST_NAPT_SRC_PORT => flow.post_napt_src_transport_port = val as u32,
                    F_POST_NAPT_DST_PORT => flow.post_napt_dst_transport_port = val as u32,
                    _ => {}
                }
            }
            1 => offset += 8,
            5 => offset += 4,
            2 => {
                let (len, consumed) = read_varint(frame, offset)?;
                offset += consumed;
                let end = offset + len as usize;
                let slice = frame.get(offset..end)?;
                match field_num {
                    F_SAMPLER_ADDRESS => flow.sampler_address = slice,
                    F_SRC_ADDR => flow.src_addr = slice,
                    F_DST_ADDR => flow.dst_addr = slice,
                    F_POST_NAT_SRC_IPV4 => flow.post_nat_src_ipv4_address = slice,
                    F_POST_NAT_DST_IPV4 => flow.post_nat_dst_ipv4_address = slice,
                    _ => {}
                }
                offset = end;
            }
            _ => return None, // unsupported wire type - malformed frame
        }
    }
    Some(flow)
}

// ---------- frame extraction from the FIFO's byte stream ----------

/// Pull complete <varint-len><message><separator> frames off the front of
/// `buf`, returning their byte ranges and how much of `buf` was consumed.
/// The 0x0A separator is trusted (this pipeline only ever has one writer,
/// so the atomic-write fix means it's always present) but verified rather
/// than blindly skipped, since a mismatch means real desync worth knowing
/// about immediately rather than silently producing corrupt output.
fn extract_ready_frames(buf: &[u8]) -> Result<(Vec<(usize, usize)>, usize), Box<dyn Error>> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    loop {
        let Some((len, consumed)) = read_varint(buf, offset) else {
            break;
        };
        let start = offset + consumed;
        let end = start + len as usize;
        if end >= buf.len() {
            break; // need more data, including to confirm the separator
        }
        if buf[end] != 0x0A {
            return Err(format!(
                "frame desync at byte {end}: expected separator 0x0A, found {:#04x}",
                buf[end]
            )
            .into());
        }
        ranges.push((start, end));
        offset = end + 1;
    }
    Ok((ranges, offset))
}

// ---------- Parquet batch builder ----------

struct Batch {
    time_received_ns: Int64Builder,
    time_flow_start_ns: Int64Builder,
    time_flow_end_ns: Int64Builder,
    sampler_address: BinaryBuilder,
    src_addr: BinaryBuilder,
    dst_addr: BinaryBuilder,
    bytes: Int64Builder,
    packets: Int64Builder,
    proto: UInt8Builder,
    src_port: UInt16Builder,
    dst_port: UInt16Builder,
    in_if: UInt32Builder,
    out_if: UInt32Builder,
    tcp_flags: UInt8Builder,
    post_nat_src_ipv4_address: BinaryBuilder,
    post_nat_dst_ipv4_address: BinaryBuilder,
    post_napt_src_transport_port: UInt16Builder,
    post_napt_dst_transport_port: UInt16Builder,
    rows: usize,
    window_start_ns: i64,
    window_end_ns: i64,
}

impl Batch {
    fn new() -> Self {
        Self {
            time_received_ns: Int64Builder::new(),
            time_flow_start_ns: Int64Builder::new(),
            time_flow_end_ns: Int64Builder::new(),
            sampler_address: BinaryBuilder::new(),
            src_addr: BinaryBuilder::new(),
            dst_addr: BinaryBuilder::new(),
            bytes: Int64Builder::new(),
            packets: Int64Builder::new(),
            proto: UInt8Builder::new(),
            src_port: UInt16Builder::new(),
            dst_port: UInt16Builder::new(),
            in_if: UInt32Builder::new(),
            out_if: UInt32Builder::new(),
            tcp_flags: UInt8Builder::new(),
            post_nat_src_ipv4_address: BinaryBuilder::new(),
            post_nat_dst_ipv4_address: BinaryBuilder::new(),
            post_napt_src_transport_port: UInt16Builder::new(),
            post_napt_dst_transport_port: UInt16Builder::new(),
            rows: 0,
            window_start_ns: i64::MAX,
            window_end_ns: i64::MIN,
        }
    }

    fn append(&mut self, f: &Flow) {
        if !matches!(f.src_addr.len(), 4 | 16) || !matches!(f.dst_addr.len(), 4 | 16) {
            return;
        }
        self.time_received_ns.append_value(f.time_received_ns);
        self.time_flow_start_ns.append_value(f.time_flow_start_ns);
        self.time_flow_end_ns.append_value(f.time_flow_end_ns);
        self.sampler_address.append_value(f.sampler_address);
        self.src_addr.append_value(f.src_addr);
        self.dst_addr.append_value(f.dst_addr);
        self.bytes.append_value(f.bytes);
        self.packets.append_value(f.packets);
        self.proto.append_value(f.proto);
        self.src_port
            .append_option((f.src_port != 0).then_some(f.src_port as u16));
        self.dst_port
            .append_option((f.dst_port != 0).then_some(f.dst_port as u16));
        self.in_if.append_value(f.in_if);
        self.out_if.append_value(f.out_if);
        // tcp_flags only means something for TCP (proto 6); null otherwise
        // rather than a misleading 0.
        self.tcp_flags
            .append_option((f.proto == 6).then_some(f.tcp_flags as u8));

        // Empty (proto3-omitted, field truly absent) means this flow wasn't
        // NAT-translated - store as NULL, distinct from an actual value.
        if f.post_nat_src_ipv4_address.is_empty() {
            self.post_nat_src_ipv4_address.append_null();
        } else {
            self.post_nat_src_ipv4_address.append_value(f.post_nat_src_ipv4_address);
        }
        if f.post_nat_dst_ipv4_address.is_empty() {
            self.post_nat_dst_ipv4_address.append_null();
        } else {
            self.post_nat_dst_ipv4_address.append_value(f.post_nat_dst_ipv4_address);
        }
        self.post_napt_src_transport_port
            .append_option((f.post_napt_src_transport_port != 0).then_some(f.post_napt_src_transport_port as u16));
        self.post_napt_dst_transport_port
            .append_option((f.post_napt_dst_transport_port != 0).then_some(f.post_napt_dst_transport_port as u16));

        self.rows += 1;
        self.window_start_ns = self.window_start_ns.min(f.time_flow_start_ns);
        self.window_end_ns = self.window_end_ns.max(f.time_flow_start_ns);
    }

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new("time_received_ns", DataType::Int64, false),
            Field::new("time_flow_start_ns", DataType::Int64, false),
            Field::new("time_flow_end_ns", DataType::Int64, false),
            Field::new("sampler_address", DataType::Binary, false),
            Field::new("src_addr", DataType::Binary, false),
            Field::new("dst_addr", DataType::Binary, false),
            Field::new("bytes", DataType::Int64, false),
            Field::new("packets", DataType::Int64, false),
            Field::new("proto", DataType::UInt8, false),
            Field::new("src_port", DataType::UInt16, true),
            Field::new("dst_port", DataType::UInt16, true),
            Field::new("in_if", DataType::UInt32, false),
            Field::new("out_if", DataType::UInt32, false),
            Field::new("tcp_flags", DataType::UInt8, true),
            Field::new("post_nat_src_ipv4_address", DataType::Binary, true),
            Field::new("post_nat_dst_ipv4_address", DataType::Binary, true),
            Field::new("post_napt_src_transport_port", DataType::UInt16, true),
            Field::new("post_napt_dst_transport_port", DataType::UInt16, true),
        ])
    }

    /// Sort by time_flow_start_ns and write to `path`. Sorting matters for
    /// Parquet row-group min/max stats to actually be useful for
    /// time-bounded queries - see the earlier size-comparison writeup.
    fn write_sorted(mut self, path: &std::path::Path, zstd_level: i32) -> Result<usize, Box<dyn Error>> {
        let schema = Arc::new(Self::schema());
        let columns: Vec<ArrayRef> = vec![
            Arc::new(self.time_received_ns.finish()),
            Arc::new(self.time_flow_start_ns.finish()),
            Arc::new(self.time_flow_end_ns.finish()),
            Arc::new(self.sampler_address.finish()),
            Arc::new(self.src_addr.finish()),
            Arc::new(self.dst_addr.finish()),
            Arc::new(self.bytes.finish()),
            Arc::new(self.packets.finish()),
            Arc::new(self.proto.finish()),
            Arc::new(self.src_port.finish()),
            Arc::new(self.dst_port.finish()),
            Arc::new(self.in_if.finish()),
            Arc::new(self.out_if.finish()),
            Arc::new(self.tcp_flags.finish()),
            Arc::new(self.post_nat_src_ipv4_address.finish()),
            Arc::new(self.post_nat_dst_ipv4_address.finish()),
            Arc::new(self.post_napt_src_transport_port.finish()),
            Arc::new(self.post_napt_dst_transport_port.finish()),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns)?;
        let row_count = batch.num_rows();
        if row_count == 0 {
            return Ok(0);
        }

        let sort_col = arrow::compute::SortColumn {
            values: batch.column_by_name("time_flow_start_ns").unwrap().clone(),
            options: None,
        };
        let indices = arrow::compute::lexsort_to_indices(&[sort_col], None)?;
        let sorted_columns: Vec<ArrayRef> = batch
            .columns()
            .iter()
            .map(|c| arrow::compute::take(c, &indices, None))
            .collect::<Result<_, _>>()?;
        let sorted_batch = RecordBatch::try_new(schema.clone(), sorted_columns)?;

        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(parquet::basic::ZstdLevel::try_new(
                zstd_level,
            )?))
            .build();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("parquet.tmp");
        let file = File::create(&tmp_path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
        writer.write(&sorted_batch)?;
        writer.close()?;
        fs::rename(&tmp_path, path)?; // atomic within the same filesystem
        Ok(row_count)
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

fn partition_path(output_dir: &std::path::Path, window_start_ns: i64, window_end_ns: i64) -> PathBuf {
    let secs = window_start_ns / 1_000_000_000;
    let days = secs / 86_400;
    // Simple proleptic Gregorian date from days-since-epoch, UTC.
    let (year, month, day) = civil_from_days(days);
    output_dir
        .join(format!("year={year:04}"))
        .join(format!("month={month:02}"))
        .join(format!("day={day:02}"))
        .join(format!(
            "goflow2_{}_{}.parquet",
            window_start_ns / 1000,
            window_end_ns / 1000
        ))
}

// Howard Hinnant's days_from_civil, inverted - no chrono/time dependency
// needed for one date computation.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    if !args.fifo.exists() {
        let status = std::process::Command::new("mkfifo").arg(&args.fifo).status()?;
        if !status.success() {
            return Err(format!("mkfifo {} failed", args.fifo.display()).into());
        }
        tracing::info!("created FIFO at {}", args.fifo.display());
    }

    // SIGTERM/SIGINT set a flag checked in the main loop, so whatever is
    // currently batched gets handed to the writer thread before exit
    // rather than lost.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone())?;

    // Sorting + writing a multi-million-row batch to Parquet is slow enough
    // (confirmed: minutes, not seconds) that doing it inline in the read
    // loop stalls reading from the FIFO for that whole time. A stalled
    // reader means the FIFO's kernel buffer fills, which blocks goflow2's
    // Send() calls - the exact backpressure chain that caused the original
    // packet-drop bug this pipeline exists to avoid. So: the read loop only
    // ever appends to an in-memory batch and swaps it out when it's time to
    // flush; the actual sort+write happens on a separate thread that the
    // read loop never waits on (except at clean shutdown, to avoid losing
    // whatever's still queued).
    let (tx, rx) = std::sync::mpsc::channel::<Batch>();
    let writer_output_dir = args.output_dir.clone();
    let writer_zstd_level = args.zstd_level;
    let writer_handle = std::thread::spawn(move || {
        for batch in rx {
            if let Err(e) = write_batch(batch, &writer_output_dir, writer_zstd_level) {
                tracing::error!("failed to write batch: {e}");
            }
        }
    });

    let flush_interval = Duration::from_secs(args.flush_interval_secs);
    let mut batch = Batch::new();
    let mut last_flush = Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(4 * 1024 * 1024);
    let mut read_chunk = vec![0u8; 1 << 20];
    let mut total_rows_since_start = 0u64;
    let mut skipped = 0u64;

    'outer: loop {
        tracing::info!("opening FIFO {} for reading", args.fifo.display());
        let mut fifo = OpenOptions::new().read(true).open(&args.fifo)?;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break 'outer;
            }

            let n = fifo.read(&mut read_chunk)?;
            if n == 0 {
                // Writer closed (goflow2 restarted). Hand off whatever's
                // batched so far before reopening - otherwise it's silently
                // lost the moment the writer disconnects, which is exactly
                // the kind of gap an audit log can't have.
                tracing::warn!("FIFO writer closed; flushing and reopening");
                enqueue(&mut batch, &tx);
                last_flush = Instant::now();
                break;
            }
            buf.extend_from_slice(&read_chunk[..n]);

            let (ranges, consumed) = extract_ready_frames(&buf)?;
            for (start, end) in &ranges {
                match scan_flow(&buf[*start..*end]) {
                    Some(flow) => {
                        batch.append(&flow);
                        total_rows_since_start += 1;
                    }
                    None => skipped += 1,
                }
            }
            buf.drain(0..consumed);

            if !ranges.is_empty() && last_flush.elapsed() >= flush_interval {
                enqueue(&mut batch, &tx);
                last_flush = Instant::now();
            }
        }
    }

    tracing::info!("shutting down, queueing final batch");
    enqueue(&mut batch, &tx);
    drop(tx); // closes the channel so the writer thread's `for batch in rx` ends
    writer_handle.join().expect("writer thread panicked");
    tracing::info!(
        "total rows processed: {total_rows_since_start}, skipped (malformed/bad-address): {skipped}"
    );
    Ok(())
}

/// Swap the current batch out for a fresh one and hand the full one to the
/// writer thread. Never blocks the read loop on the (slow) sort+write.
fn enqueue(batch: &mut Batch, tx: &std::sync::mpsc::Sender<Batch>) {
    if batch.rows == 0 {
        return;
    }
    let full = std::mem::replace(batch, Batch::new());
    let rows = full.rows;
    if tx.send(full).is_err() {
        tracing::error!("writer thread gone, dropped a batch of {rows} rows");
    }
}

fn write_batch(batch: Batch, output_dir: &std::path::Path, zstd_level: i32) -> Result<(), Box<dyn Error>> {
    let window_start_ns = batch.window_start_ns;
    let window_end_ns = if batch.window_end_ns >= batch.window_start_ns {
        batch.window_end_ns
    } else {
        now_ns()
    };
    let path = partition_path(output_dir, window_start_ns, window_end_ns);
    let rows = batch.write_sorted(&path, zstd_level)?;
    tracing::info!("flushed {rows} rows to {}", path.display());
    Ok(())
}
