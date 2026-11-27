#!/bin/bash
# Feed readsb (in Docker) and aggd the same real Beast stream and leave
# both running for comparison.
#   tools/shadow.sh HOST PORT [extra aggd args]
# Compare with: tools/compare.py $BENCH_DIR/shadow/aircraft.json http://127.0.0.1:38080/data/aircraft.json --rounds 20
HERE=$(cd "$(dirname "$0")/.." && pwd); W=${BENCH_DIR:-/tmp/aggd-bench}/shadow; mkdir -p $W; chmod 777 $W
HOST=$1; PORT=$2; shift 2
IMG=ghcr.io/sdr-enthusiasts/docker-adsb-ultrafeeder:latest
DB=${BENCH_DIR:-/tmp/aggd-bench}/run/aircraft.csv.gz; [ -f $DB ] && cp -n $DB $W/aircraft.csv.gz
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own
sg docker -c "docker rm -f aggd-oracle" >/dev/null 2>&1
sg docker -c "docker run -d --name aggd-oracle --network host -v $W:/run/readsb --entrypoint readsb $IMG --net --net-only --quiet --max-range 20000 --net-connector $HOST,$PORT,beast_in --write-json /run/readsb --write-json-every 1 --net-api-port 39152 --net-sbs-port 39003 --json-reliable 1 --db-file /run/readsb/aircraft.csv.gz --db-file-lt ${ORACLE_ARGS:-}" >/dev/null
$HERE/target/release/aggd --net-connector $HOST,$PORT,beast_in --http-port 38080 --net-sbs-port 38003 --db-file $W/aircraft.csv.gz --db-file-lt "$@" > $W/aggd.log 2>&1 & own aggd
echo "shadow running: readsb -> $W/aircraft.json, aggd -> http://127.0.0.1:38080/data/aircraft.json"
