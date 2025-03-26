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
* TimescaleDB:
   * IP address column type
   * Compress data after a certain point
   * Build schema and loading script
   * pgAdmin for management of backend
   * Report builder options
* Kafka for queue:
   * How well would this work against RabbitMQ?
   * It still writes data to disk... Optane?
   * Protobuf for faster parsing
   * Possibly remove this layer entirely if use pmacct instead of goflow2
   * Trigger script for enriching with external data sources
   * Can use this to deal with creating aggregates
* Mikrotik:
   * v9 / IPFIX does not work with goflow2
