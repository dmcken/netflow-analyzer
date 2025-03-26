# Netflow analyzer

A complete netflow analysis setup, requirements as follows:

1. Be able to provide NAT/CG-NAT mappings
2. Handle LARGE amounts of flows.
3. Handling multiple routers would be nice but not specifically required as I'm trying to use containers as much as possible so that the setup can be replicated as many times as required (maybe with different ports). In light of point #2 its possible that this would be a good idea for scalability.

At the moment, I'm at least experimenting with two major solutions:
* goflow2
    * Newer solution written in Go
* pmacct
    * Older solution written in C
    * Can speak directly to some databases