#!/bin/bash
# Moves the oldest closed day-partitions of the flow-api parquet archive from
# the hot SSD (/mnt/netflow) to the archive HDD (/mnt/archive) once SSD usage
# crosses HIGH_WATERMARK, continuing until it drops below LOW_WATERMARK. The
# original location is left as a symlink into the archive, so flow-api (a
# single --data-dir DataFusion ListingTable scan) keeps querying archived
# days transparently - verified empirically against day=06 on 2026-09-28
# (DataFusion's local object_store follows directory symlinks).
#
# A day is only eligible once it is "closed": its own directory name (a past
# calendar date) is older than TODAY, so the currently-being-written day is
# never touched mid-write.

set -euo pipefail

HOT_ROOT="/mnt/netflow/airlink-logs/parquet"
ARCHIVE_ROOT="/mnt/archive/parquet"
HIGH_WATERMARK=85   # start archiving once hot usage crosses this %
LOW_WATERMARK=70    # stop once hot usage drops back below this %
LOG_TAG="archive_flow_days"
LOCK_FILE="/tmp/archive_flow_days.lock"

# Runs every 30 min via cron; a single day-partition move can take longer
# than that, so re-exec ourselves under a flock rather than risk two copies
# racing on the same directory.
if [[ "${BASH_SOURCE[0]}" == "${0}" && "${FLOCKED:-}" != "1" ]]; then
    exec env FLOCKED=1 flock -n "$LOCK_FILE" "$0" "$@"
fi

log() {
    logger -t "$LOG_TAG" "$1"
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') $1"
}

hot_usage_pct() {
    df --output=pcent "$HOT_ROOT" | tail -1 | tr -dc '0-9'
}

# List real (non-symlink) day directories older than today, oldest first,
# as "year=YYYY/month=MM/day=DD" relative paths.
list_archivable_days() {
    local today
    today=$(date -u '+%Y-%m-%d')
    find "$HOT_ROOT" -mindepth 3 -maxdepth 3 -type d -name 'day=*' | while read -r d; do
        rel="${d#"$HOT_ROOT"/}"
        y=$(echo "$rel" | sed -n 's#year=\([0-9]*\)/.*#\1#p')
        m=$(echo "$rel" | sed -n 's#.*month=\([0-9]*\)/.*#\1#p')
        dd=$(echo "$rel" | sed -n 's#.*day=\([0-9]*\)#\1#p')
        day_date="$y-$m-$dd"
        if [[ "$day_date" < "$today" ]]; then
            echo "$day_date $rel"
        fi
    done | sort | awk '{print $2}'
}

archive_one_day() {
    local rel="$1"
    local src="$HOT_ROOT/$rel"
    local dst="$ARCHIVE_ROOT/$rel"

    log "archiving $rel: copying to $dst"
    mkdir -p "$(dirname "$dst")"
    rsync -a "$src/" "$dst/"

    local src_size dst_size src_count dst_count
    src_size=$(du -sb "$src" | cut -f1)
    dst_size=$(du -sb "$dst" | cut -f1)
    src_count=$(find "$src" -type f | wc -l)
    dst_count=$(find "$dst" -type f | wc -l)

    if [[ "$src_size" != "$dst_size" || "$src_count" != "$dst_count" ]]; then
        log "ERROR: verification mismatch for $rel (src ${src_size}b/${src_count}f dst ${dst_size}b/${dst_count}f) - leaving original in place, removing partial copy"
        rm -rf "$dst"
        return 1
    fi

    rm -rf "$src"
    ln -s "$dst" "$src"
    log "archived $rel ok ($dst_size bytes, $dst_count files) - freed on hot storage, symlinked for continued querying"
}

main() {
    local usage
    usage=$(hot_usage_pct)
    log "hot storage at ${usage}% (high=${HIGH_WATERMARK}% low=${LOW_WATERMARK}%)"

    if (( usage < HIGH_WATERMARK )); then
        log "below high watermark, nothing to do"
        return 0
    fi

    local days
    days=$(list_archivable_days)
    if [[ -z "$days" ]]; then
        log "WARNING: hot storage above high watermark but no closed day-partitions are archivable (all remaining days may already be archived, or are today's in-progress day)"
        return 0
    fi

    while (( usage >= LOW_WATERMARK )); do
        local next_day
        next_day=$(echo "$days" | head -1)
        if [[ -z "$next_day" ]]; then
            log "WARNING: ran out of archivable days before reaching low watermark (currently ${usage}%)"
            break
        fi
        days=$(echo "$days" | tail -n +2)

        if archive_one_day "$next_day"; then
            usage=$(hot_usage_pct)
            log "hot storage now at ${usage}%"
        fi
    done

    log "done, hot storage at ${usage}%"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
