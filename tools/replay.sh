#!/bin/bash
# Start a fresh readsb (Docker) and a fresh aggd, play a recorded Beast
# stream to both at the speed it arrived, and compare them as it plays.
#   tools/replay.sh TAPE [COMPARE_EVERY_SECONDS] [extra aggd args]
# KEEP=1 leaves both running at the end.
HERE=$(cd "$(dirname "$0")/.." && pwd); W=${BENCH_DIR:-/tmp/aggd-bench}/replay; mkdir -p $W; chmod 777 $W
TAPE=$1; EVERY=${2:-120}; shift; shift 2>/dev/null
IMG=ghcr.io/sdr-enthusiasts/docker-adsb-ultrafeeder:latest
DB=${BENCH_DIR:-/tmp/aggd-bench}/run/aircraft.csv.gz; [ -f $DB ] && cp --update=none $DB $W/aircraft.csv.gz
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own
sg docker -c "docker rm -f aggd-oracle" >/dev/null 2>&1; rm -f $W/aircraft.json
sg docker -c "docker run -d --name aggd-oracle --network host -v $W:/run/readsb --entrypoint readsb $IMG --net --net-only --quiet --max-range 20000 --net-bi-port 39004 --write-json /run/readsb --write-json-every 1 --net-api-port 39152 --net-sbs-port 39003 --json-reliable 1 --db-file /run/readsb/aircraft.csv.gz --db-file-lt ${ORACLE_ARGS:-}" >/dev/null
$HERE/target/release/aggd --net-bi-port 38004 --net-trusted 127.0.0.1 --http-port 38080 --net-sbs-port 38003 --db-file $W/aircraft.csv.gz --db-file-lt "$@" > $W/aggd.log 2>&1 & own aggd
sleep 2
$HERE/target/release/aggbench replay $TAPE 127.0.0.1:39004 127.0.0.1:38004 ${ONLY:+--only $ONLY} > $W/replay.log 2>&1 & own aggbench
RP=$!
while kill -0 $RP 2>/dev/null; do
  for i in $(seq $EVERY); do kill -0 $RP 2>/dev/null || break; sleep 1; done
  kill -0 $RP 2>/dev/null || break
  echo "--- $(date +%T)"; N=$((${N:-0} + 1))
  cp $W/aircraft.json $W/ref-$N.json 2>/dev/null; curl -s http://127.0.0.1:38080/data/aircraft.json -o $W/cand-$N.json
  python3 $HERE/tools/compare.py $W/aircraft.json http://127.0.0.1:38080/data/aircraft.json --rounds ${ROUNDS:-3} 2>&1 | cut -c1-${COLS:-160} | head -${LINES_MAX:-40}
done
[ "${KEEP:-0}" = 1 ] || { stop_own aggd; sg docker -c "docker rm -f aggd-oracle" >/dev/null 2>&1; }
