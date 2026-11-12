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
#
# Retention is a target, not a guarantee. Every run first prunes archived
# days beyond the newest ARCHIVE_KEEP_DAYS (routine trimming). If archive
# usage is still at/above ARCHIVE_PURGE_PCT after that, it keeps purging the
# oldest archived day outright - below ARCHIVE_KEEP_DAYS if it has to -
# until usage drops under ARCHIVE_PURGE_TARGET_PCT. Before copying a day to
# the archive, archive_one_day runs the same purge on demand if there isn't
# room for that one day specifically (this also means a day is no longer
# retried-and-abandoned every cycle just because the archive happened to
# fill up mid-copy - see 2026-11-12's day=01/02/04, which kept losing that
# race for weeks). The policy throughout: never let either disk filling up
# block a recent flow write or a new day's archive copy - losing old
# archived history is preferable to that. ARCHIVE_ALERT_PCT, checked after
# all purging, is the "even purging everything wasn't enough, go add more
# storage" signal - logged to syslog at warning level, same as every
# individual purge.

set -euo pipefail

HOT_ROOT="${HOT_ROOT:-/mnt/netflow/airlink-logs/parquet}"
ARCHIVE_ROOT="${ARCHIVE_ROOT:-/mnt/archive/parquet}"
HIGH_WATERMARK=85   # start archiving once hot usage crosses this %
LOW_WATERMARK=70    # stop once hot usage drops back below this %
ARCHIVE_KEEP_DAYS="${ARCHIVE_KEEP_DAYS:-25}"       # archived days to keep, routine target
ARCHIVE_PURGE_PCT="${ARCHIVE_PURGE_PCT:-85}"       # start emergency purging once archive usage crosses this %
ARCHIVE_PURGE_TARGET_PCT="${ARCHIVE_PURGE_TARGET_PCT:-70}"  # emergency-purge until usage drops below this %
ARCHIVE_ALERT_PCT="${ARCHIVE_ALERT_PCT:-90}"       # warn if archive usage is still this high after purging
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

warn() {
    logger -p user.warning -t "$LOG_TAG" "$1"
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') WARNING: $1"
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

archive_usage_pct() {
    df --output=pcent "$ARCHIVE_ROOT" | tail -1 | tr -dc '0-9'
}

archive_free_bytes() {
    df --output=avail -B1 "$ARCHIVE_ROOT" | tail -1 | tr -dc '0-9'
}

# Archived day directories, oldest first, as "year=YYYY/month=MM/day=DD".
list_archived_days() {
    find "$ARCHIVE_ROOT" -mindepth 3 -maxdepth 3 -type d -name 'day=*' | while read -r d; do
        rel="${d#"$ARCHIVE_ROOT"/}"
        echo "$(echo "$rel" | tr -dc '0-9') $rel"
    done | sort | awk '{print $2}'
}

# Deletes one archived day: the hot-tier symlink first, then the archived
# directory itself. Refuses (rather than deleting live data) if the hot
# path isn't the symlink this script would have created for it.
purge_oldest_archived_day() {
    local rel="$1" reason="$2"
    local link="$HOT_ROOT/$rel" dst="$ARCHIVE_ROOT/$rel"

    if [[ -L "$link" ]]; then
        if [[ "$(readlink "$link")" != "$dst" ]]; then
            log "ERROR: $link points to $(readlink "$link"), not $dst - not purging $rel"
            return 1
        fi
        rm "$link"
    elif [[ -e "$link" ]]; then
        log "ERROR: $link is a real directory, not an archive symlink - not purging $rel"
        return 1
    fi
    rm -rf -- "$dst"
    warn "purged archived $rel ($reason) - that day's flow data is now gone from both tiers"
}

# Ensures at least $1 bytes are free on the archive disk, purging the
# oldest archived days (ignoring ARCHIVE_KEEP_DAYS) until there's room or
# nothing left to purge. Returns failure only if it ran out of days.
purge_archive_for_space() {
    local need_bytes="$1" free days rel
    free=$(archive_free_bytes)
    if (( free >= need_bytes )); then
        return 0
    fi
    log "archive free space (${free}b) below what's needed (${need_bytes}b) - purging oldest archived days to make room"
    days=$(list_archived_days)
    while (( free < need_bytes )); do
        rel=$(printf '%s\n' "$days" | head -1)
        if [[ -z "$rel" ]]; then
            log "ERROR: purged every archived day and still only have ${free}b free (need ${need_bytes}b)"
            return 1
        fi
        days=$(printf '%s\n' "$days" | tail -n +2)
        purge_oldest_archived_day "$rel" "making room for a new day" || true
        free=$(archive_free_bytes)
    done
}

archive_one_day() {
    local rel="$1"
    local src="$HOT_ROOT/$rel"
    local dst="$ARCHIVE_ROOT/$rel"

    local src_bytes need_bytes
    src_bytes=$(du -sb "$src" | cut -f1)
    need_bytes=$(( src_bytes * 110 / 100 ))   # 10% margin for fs overhead/rounding
    if ! purge_archive_for_space "$need_bytes"; then
        log "ERROR: no room for $rel (need ~${need_bytes}b) even after purging everything purgeable - skipping, will retry next cycle"
        return 1
    fi

    log "archiving $rel: copying to $dst"
    mkdir -p "$(dirname "$dst")"
    rsync -a "$src/" "$dst/"

    local dst_size src_count dst_count
    dst_size=$(du -sb "$dst" | cut -f1)
    src_count=$(find "$src" -type f | wc -l)
    dst_count=$(find "$dst" -type f | wc -l)

    if [[ "$src_bytes" != "$dst_size" || "$src_count" != "$dst_count" ]]; then
        log "ERROR: verification mismatch for $rel (src ${src_bytes}b/${src_count}f dst ${dst_size}b/${dst_count}f) - leaving original in place, removing partial copy"
        rm -rf "$dst"
        return 1
    fi

    rm -rf "$src"
    ln -s "$dst" "$src"
    log "archived $rel ok (${dst_size} bytes, ${dst_count} files) - freed on hot storage, symlinked for continued querying"
}

prune_archive() {
    local days count excess rel usage

    # Routine retention: trim back to the newest ARCHIVE_KEEP_DAYS.
    days=$(list_archived_days)
    count=$(printf '%s\n' "$days" | grep -c . || true)
    excess=$(( count - ARCHIVE_KEEP_DAYS ))
    if (( excess > 0 )); then
        log "archive holds ${count} days (keep ${ARCHIVE_KEEP_DAYS}), pruning ${excess} oldest"
        for rel in $(printf '%s\n' "$days" | head -n "$excess"); do
            purge_oldest_archived_day "$rel" "beyond the ${ARCHIVE_KEEP_DAYS}-day retention target" || true
        done
    fi

    # Emergency: the disk itself is too full even within retention - purge
    # below ARCHIVE_KEEP_DAYS rather than risk the next archive copy (or
    # worse, pq-consumer's live writes once the hot tier backs up) failing.
    usage=$(archive_usage_pct)
    if (( usage >= ARCHIVE_PURGE_PCT )); then
        log "archive storage at ${usage}% (purge at ${ARCHIVE_PURGE_PCT}%) - purging oldest archived days, below the ${ARCHIVE_KEEP_DAYS}-day target if needed"
        days=$(list_archived_days)
        while (( usage >= ARCHIVE_PURGE_TARGET_PCT )); do
            rel=$(printf '%s\n' "$days" | head -1)
            if [[ -z "$rel" ]]; then
                warn "purged every archived day and archive storage is still at ${usage}% - nothing left on this tier to free"
                break
            fi
            days=$(printf '%s\n' "$days" | tail -n +2)
            purge_oldest_archived_day "$rel" "archive usage emergency, ${usage}% >= ${ARCHIVE_PURGE_PCT}%" || true
            usage=$(archive_usage_pct)
        done
        log "archive storage now at ${usage}%"
    fi

    if (( usage >= ARCHIVE_ALERT_PCT )); then
        warn "archive storage at ${usage}% even after purging (alert at ${ARCHIVE_ALERT_PCT}%) - add more storage"
    fi
}

main() {
    local usage
    prune_archive

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
