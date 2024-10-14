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
