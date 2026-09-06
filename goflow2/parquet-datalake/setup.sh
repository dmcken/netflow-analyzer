#!/bin/bash
# One-time setup: create the FIFO directory and the Parquet output directory.
# The FIFO file itself is auto-created by pq-consumer on first run.
set -euo pipefail

FIFO_DIR="/mnt/netflow/airlink-logs/fifo"
PARQUET_DIR="/mnt/netflow/airlink-logs/parquet"

mkdir -p "$FIFO_DIR" "$PARQUET_DIR"
echo "Created $FIFO_DIR and $PARQUET_DIR"
echo "The FIFO itself (goflow2.fifo) is created automatically by pq-consumer on first run."
