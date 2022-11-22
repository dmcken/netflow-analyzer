# netflow-analyzer

A complete netflow analysis setup, based on the following:

* Orchestration - docker compose
* Database - timescaledb
* Netflow collection - pmacct
* Web front end - tech not picked yet.

## Setup:

### Database:

1. Copy the .env_example to .env  
<code>cp .env_example .env</code>
2. Set a password to POSTGRES_PASSWORD created using a secure password generator similar to [this](https://bitwarden.com/password-generator/)
3. Start the database:  
<code>docker compose up -d db</code>
4. Ensure the database started successfully:  
<code>docker compose logs -f db</code>  
You are looking for a line that says "database system is ready to accept connections", if you see that then you are good to continue, otherwise the database needs to be checked.
5. 
