# Netflow analyzer

A complete netflow analysis setup, requirements as follows:

1. Be able to provide NAT/CG-NAT mappings
2. Handle LARGE amounts of flows (One network I manage does at least 1.4 billion flows in a 24 hour period to give some context):
    * There are 2 major sections:
        1. Archive:
            * Should be able to keep track of data for at least a month.
            * The following data points are required:
                * sampler_address: IP Address (4 or 6)
                * flow_start_time: Nanosecond precision (64-bit unsigned)
                * flow_end_time:   Nanosecond precision (64-bit unsigned)
                * bytes:   64-bit unsigned
                * packets: 64-bit unsigned
                * src_addr: IP Address (4 or 6)
                * dst_addr: IP Address (4 or 6)
                * Ethenet Type: IPv4 / IPv6 / etc
                * Layer 4 Protocol: TCP / UDP / etc
                * src_port: 16-bit unsigned
                * dst_port: 16-bit unsigned
                * post_nat_src_ipv4_address: IPv4 address (32-bit unsigned)
                * post_nat_dst_ipv4_address: IPv4 address (32-bit unsigned)
                * post_napt_src_transport_port: 16-bit unsigned
                * post_napt_dst_transport_port: 16-bit unsigned
        2. Analytics:
            * Keep a year worth of data
3. Handling multiple routers would be nice but not specifically required as I'm trying to use containers as much as possible so that the setup can be replicated as many times as required (maybe with different ports). In light of point #2 its possible that this would be a good idea for scalability.

At the moment, I'm at least experimenting with two major solutions:
1. goflow2
    * Newer solution written in Go
2. pmacct
    * Older solution written in C
    * Can speak directly to some databases
