#!/usr/bin/env bash
# Wait (bounded) until the local stack is settled:
#   - no Restate invocation is in flight (timers and suspended/paused ones don't count), and
#   - the monitor bridge's durable JetStream consumer has nothing pending or unacknowledged,
#     i.e. every RdfChanged event has been detected and proposed on,
# for three polls in a row. Ready is not settled: right after bring-up the first sweep still
# ingests, monitors and proposes, and a test started then misses its own window.
#
# Slates are not waited for: they run off the bridge's path and delay no event.
#
# Usage: scripts/wait-settled.sh [timeout-seconds]   (default: EKG_SETTLE_TIMEOUT or 300)
set -euo pipefail

timeout="${1:-${EKG_SETTLE_TIMEOUT:-300}}"
admin="${RESTATE_ADMIN_URL:-http://localhost:9070}"
nats_monitor="${NATS_MONITOR_URL:-http://localhost:8222}"
consumer="${EKG_MONITOR_CONSUMER:-ekg-monitor-bridge}"

busy_sql="SELECT count(*) AS n FROM sys_invocation WHERE status IN ('pending', 'ready', 'running', 'backing-off')"

# In-flight Restate invocations, or empty when the admin query fails.
restate_busy() {
  curl -fsS -X POST "${admin%/}/query" -H 'content-type: application/json' \
    -H 'accept: application/json' -d "{\"query\": \"${busy_sql}\"}" 2>/dev/null \
    | grep -o '"n":[0-9]*' | cut -d: -f2 || true
}

# The bridge consumer's pending + unacknowledged messages, or empty when unreadable (the
# monitoring endpoint is down, or the consumer is not bound yet).
bridge_backlog() {
  curl -fsS "${nats_monitor%/}/jsz?consumers=true" 2>/dev/null | python3 -c '
import json, sys
name = sys.argv[1]
try:
    doc = json.load(sys.stdin)
except ValueError:
    sys.exit(0)
for account in doc.get("account_details") or []:
    for stream in account.get("stream_detail") or []:
        for c in stream.get("consumer_detail") or []:
            if c.get("name") == name:
                print(int(c.get("num_pending", 0)) + int(c.get("num_ack_pending", 0)))
                sys.exit(0)
' "$consumer" || true
}

echo "waiting up to ${timeout}s for the stack to settle (Restate idle, monitor bridge drained)..."
waited=0
quiet=0
while :; do
  busy="$(restate_busy)"
  backlog="$(bridge_backlog)"
  if [ "${busy:-x}" = 0 ] && [ "${backlog:-x}" = 0 ]; then
    quiet=$((quiet + 1))
  else
    quiet=0
  fi
  [ "$quiet" -ge 3 ] && break
  if [ "$waited" -ge "$timeout" ]; then
    echo "✗ stack still busy after ${timeout}s (${busy:-unknown} Restate invocation(s) in flight, ${backlog:-unknown} bridge event(s) pending): check the Restate UI (${admin%/}/ui/) and ${nats_monitor%/}/jsz?consumers=true" >&2
    exit 1
  fi
  if [ $((waited % 30)) = 0 ]; then
    echo "  ${busy:-?} invocation(s) in flight, ${backlog:-?} bridge event(s) pending (${waited}s)"
  fi
  sleep 3
  waited=$((waited + 3))
done
echo "  settled after ~${waited}s"
