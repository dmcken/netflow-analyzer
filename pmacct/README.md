# netflow-analyzer

A complete netflow analysis setup, based on the following:

* Orchestration - docker compose
* Database - timescaledb
* Netflow collection - pmacct
* Web front end - tech not picked yet.

## Setup:

### Database:

1. Copy the .env_example to .env
   ```
   cp .env_example .env
   ```
1. Edit .evn setting the following values:
   - Password entries ( All created using a secure password generator similar to [this](https://bitwarden.com/password-generator/) ).
     1. POSTGRES_PASSWORD
     1. POSTGRES_PMACCT_PW
     1. PGADMIN_DEFAULT_PASSWORD
1. Start the database:
   ```
   docker compose up -d db
   ```
   Ensure the database started successfully, using the following to check the logs (CTRL-C to exit the log view):
   ```
   docker compose logs -f db
   ```
   You are looking for a line that says <code>database system is ready to accept connections</code>, if present then you are good to continue, otherwise the database needs to be troubleshooted.
7.



### Notes:

* Kafka for queue:
   * How well would this work against RabbitMQ?
   * It still writes data to disk... Optane?
   * Protobuf for faster parsing
   * Possibly remove this layer entirely if use pmacct instead of goflow2
   * Trigger script for enriching with external data sources
   * Can use this to deal with creating aggregates
* Mikrotik:
   * v9 / IPFIX does not work with goflow2
* Database:
   * TimescaleDB:
      * Data types:
         * Postgres IP address column type
         * Like postgres does not have unsigned integer data types.
      * Compress data after a certain point
      * Build schema and loading script
      * pgAdmin for management of backend
      * Report builder options
   * [Apache Iceberg](https://iceberg.apache.org/):
      * Preferred alternative to delta lake.
      * PartitionsOld partitions can be archived on cold storage.
      * SQL based interfaces.
      * [Data types](https://iceberg.apache.org/spec/#schemas-and-data-types):
         * Does not have unsigned integer data types.
         * fixed(N) - Fixed-length byte array of length L
         * timestamp_ns - Nanosecond precision timestamp.
      * Modules:
         * iceberg-python - Dashboards
         * iceberg-rust   - Raw processing
         * iceberg-core
         * iceberg-common
         * iceberg-api
         * iceberg-paraquet - Cold storage may use paraquet.

