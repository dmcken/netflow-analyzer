#!/bin/bash

cat sql/create-db.sql     | docker compose exec -T db psql -U postgres -d template1
cat sql/create-tables.sql | docker compose exec -T db psql -U postgres -d pmacct
