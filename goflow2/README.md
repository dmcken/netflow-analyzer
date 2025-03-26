# Goflow2 capture and analysis






To implement:
* Archiver
    * Save the records to log rotated files.
* Analyzer
    * Major reports 
        * Local networks - What are local hosts / customers accessing?
        * Destination ASNs - What networks are being accessed?
            * Combinations of local network and destination and local IP.
        * Time aggregate:
            * 1 or 5 minute averages.
            * 1 minute => 1440 samples / day
            * 5 minute => 288 samples / day



Notes:

How things look in the container:
```bash
docker compose exec -it goflow2 sh
cd /var/log/goflow/
mv goflow2.log goflow2_`date +%Y%m%d_%H%M`.log
kill -1 1





/ # ps
PID   USER     TIME  COMMAND
    1 root      5h55 ./goflow2 -mapping=/field-map.yaml -transport=file -transport.file=/var/log/goflow/goflow2.log -format=json
   27 root      0:00 sh
   33 root      0:00 ps

```

Rust data structures:

```rust
//#[derive(Debug)]
// Trimmed down record
struct NetflowRecord {
    time_received_ns: DateTime<Utc>,
    sequence_num: i64,        
    time_flow_start_ns: i64,
    time_flow_end_ns: i64,
    bytes: i64,
    packets: i64,
    src_addr: IpAddr,
    dst_addr: IpAddr,
    etype: i32,
    proto: i16,
    src_port: i32,
    dst_port: i32,
    post_nat_src_ipv4_address: Option<IpAddr>,
    post_nat_dst_ipv4_address: Option<IpAddr>,
    post_napt_src_transport_port: Option<i32>,
    post_napt_dst_transport_port: Option<i32>,
}

// Raw JSON version of the record with the extra fields mapped.
// These fields are defined here:
// https://github.com/netsampler/goflow2/blob/main/docs/protocols.md
#[derive(Serialize, Deserialize, Debug)]
struct JSONNetflowRecord {
    // Define your structure based on the expected JSON format
    /*
    {
        "type":"IPFIX",
        "time_received_ns":"2025-03-15T17:10:51.064235982Z",
        "sequence_num":2259237964,
        "sampling_rate":0,
        "sampler_address":"10.255.255.254",
        "time_flow_start_ns":1742058651000000000,
        "time_flow_end_ns":1742058651000000000,
        "bytes":52,
        "packets":1,
        "src_addr":"10.8.31.235",
        "src_net":"0.0.0.0/0",
        "dst_addr":"1.1.1.1",
        "dst_net":"0.0.0.0/0",
        "etype":"IPv4",
        "proto":"UDP",
        "src_port":41015,
        "dst_port":53,
        "in_if":4,
        "out_if":35,
        "src_mac":"2c:c8:1b:ac:cf:81",
        "dst_mac":"dc:2c:6e:8c:c6:f3",
        "icmp_name":"unknown",
        "post_nat_src_ipv4_address":"6799ef23",
        "post_nat_dst_ipv4_address":"01010101",
        "post_napt_src_transport_port":41015,
        "post_napt_dst_transport_port":53
    }
    */

    r#type: String,           // Looks to be the source (IPFIX), enum?
    time_received_ns: String, // Date in what looks to be ISO8901
    sequence_num: i64,        // Does this need to be increased?
    sampling_rate: u32,       // Most cases should fit in 32-bit
    sampler_address: String,
    time_flow_start_ns: i64,
    time_flow_end_ns: i64,
    bytes: i64,
    packets: i64,
    src_addr: IpAddr,
    src_net: String,
    dst_addr: IpAddr,
    dst_net: String,
    etype: String,            // IPv4, IPv6, any others? enum?
    proto: String,            // UDP, TCP, others? enum?
    src_port: i32,
    dst_port: i32,
    in_if: u16,
    out_if: u16,
    src_mac: String,
    dst_mac: String,
    icmp_name: String,
    post_nat_src_ipv4_address: Option<String>,
    post_nat_dst_ipv4_address: Option<String>,
    post_napt_src_transport_port: Option<i32>,
    post_napt_dst_transport_port: Option<i32>,
}
```
