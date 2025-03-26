use chrono::{DateTime, Utc};
use futures::stream::StreamExt;
use futures::SinkExt;
use rdkafka::{ClientConfig, consumer::{Consumer, StreamConsumer}};
use rdkafka::message::{Message};
use rust_protobuf::Message as ProtobufMessage;
use std::collections::HashMap;
use std::fs::{File, create_dir_all};
use std::io::{self, Write};
use std::net::IpAddr;
use tokio;

#[derive(Debug, Clone)]
pub struct GoflowNetflowRecord {
    pub r#type: String,
    pub time_received_ns: String,  // ISO 8601 format as String
    pub sequence_num: i64,
    pub sampling_rate: u32,
    pub sampler_address: String,
    pub time_flow_start_ns: i64,
    pub time_flow_end_ns: i64,
    pub bytes: i64,
    pub packets: i64,
    pub src_addr: IpAddr,
    pub src_net: String,
    pub dst_addr: IpAddr,
    pub dst_net: String,
    pub etype: String,           // String for the Ethernet type
    pub proto: String,           // String for the protocol
    pub src_port: i32,
    pub dst_port: i32,
    pub in_if: u16,
    pub out_if: u16,
    pub src_mac: String,
    pub dst_mac: String,
    pub icmp_name: String,
    pub post_nat_src_ipv4_address: Option<String>,
    pub post_nat_dst_ipv4_address: Option<String>,
    pub post_napt_src_transport_port: Option<i32>,
    pub post_napt_dst_transport_port: Option<i32>,
}

// The protobuf definition you need to use (generated from your .proto file)
mod netflow_protobuf {
    include!(concat!(env!("OUT_DIR"), "/goflow_netflow_pb.rs"));
}

async fn process_kafka_messages() -> io::Result<()> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", "goflow-consumer-group")
        .set("bootstrap.servers", "localhost:9092")
        .create()?;

    consumer.subscribe(&["goflow-netflow-topic"])?;

    let mut records_by_date: HashMap<String, Vec<GoflowNetflowRecord>> = HashMap::new();

    let mut stream = consumer.start();

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => {
                let payload = msg.payload().unwrap();
                let record = match netflow_protobuf::GoflowNetflowRecord::parse_from_bytes(payload) {
                    Ok(parsed_record) => parsed_record,
                    Err(_) => continue, // Skip this record if it cannot be parsed
                };

                // Convert the string time_received_ns to DateTime<Utc>
                let time_received_ns = match DateTime::parse_from_rfc3339(&record.time_received_ns) {
                    Ok(dt) => dt.with_timezone(&Utc),
                    Err(_) => continue, // Skip if the date format is incorrect
                };

                let goflow_record = GoflowNetflowRecord {
                    r#type: record.r#type,
                    time_received_ns: record.time_received_ns,
                    sequence_num: record.sequence_num,
                    sampling_rate: record.sampling_rate,
                    sampler_address: record.sampler_address,
                    time_flow_start_ns: record.time_flow_start_ns,
                    time_flow_end_ns: record.time_flow_end_ns,
                    bytes: record.bytes,
                    packets: record.packets,
                    src_addr: record.src_addr.parse().unwrap(),
                    src_net: record.src_net,
                    dst_addr: record.dst_addr.parse().unwrap(),
                    dst_net: record.dst_net,
                    etype: record.etype,
                    proto: record.proto,
                    src_port: record.src_port,
                    dst_port: record.dst_port,
                    in_if: record.in_if,
                    out_if: record.out_if,
                    src_mac: record.src_mac,
                    dst_mac: record.dst_mac,
                    icmp_name: record.icmp_name,
                    post_nat_src_ipv4_address: record.post_nat_src_ipv4_address,
                    post_nat_dst_ipv4_address: record.post_nat_dst_ipv4_address,
                    post_napt_src_transport_port: record.post_napt_src_transport_port,
                    post_napt_dst_transport_port: record.post_napt_dst_transport_port,
                };

                let date_str = time_received_ns.format("%Y-%m-%d").to_string();
                records_by_date.entry(date_str).or_insert_with(Vec::new).push(goflow_record);
            }
            Err(e) => {
                eprintln!("Error receiving Kafka message: {:?}", e);
            }
        }
    }

    // Write the records to files
    for (date, records) in records_by_date {
        let file_name = format!("output/{}.json", date);
        create_dir_all("output")?; // Ensure the output directory exists
        let mut file = File::create(file_name)?;

        for record in records {
            let json_record = serde_json::to_string(&record)?;
            writeln!(file, "{}", json_record)?;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = process_kafka_messages().await {
        eprintln!("Error processing Kafka messages: {:?}", e);
    }
}
