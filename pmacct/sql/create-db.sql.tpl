
-- Database
CREATE DATABASE pmacct;
\c pmacct

-- User
CREATE USER pmacct;
ALTER USER pmacct WITH PASSWORD '$POSTGRES_PMACCT_PW';
