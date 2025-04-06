#!/bin/bash

docker compose exec goflow2 sh -c "chown 1000:1000 /var/log/goflow/"
docker compose exec goflow2 sh -c "chown 1000:1000 /var/log/goflow/goflow2_[0-9]*_[0-9]*.log"

# Archive the old files.
# mv logs/goflow2_20250405_* /mnt/data/netflow-logs/2025/04/05/
