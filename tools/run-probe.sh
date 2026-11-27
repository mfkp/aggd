#!/bin/bash
# Restart aggd and probe it against the reference readsb left running by
# `KEEP=1 tools/bench.sh ...`.   tools/run-probe.sh HEX [aggd args]
HERE=$(cd "$(dirname "$0")/.." && pwd); W=${BENCH_DIR:-/tmp/aggd-bench}; HEX=${1:-4CA2D6}; shift
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own aggd; sleep 0.3
$HERE/target/release/aggd --lat 41.9 --lon 12.5 --net-bi-port 38004 --http-port 38080 --net-sbs-port 38003 --net-sbs-in-port 38033 --net-sbs-jaero-in-port 38034 --db-file $W/run/aircraft.csv.gz --db-file-lt "$@" > $W/aggd.log 2>&1 & own aggd
sleep 1
python3 $HERE/tools/probe.py 127.0.0.1:39004 $W/run/aircraft.json 127.0.0.1:39003 127.0.0.1:38004 http://127.0.0.1:38080/data/aircraft.json 127.0.0.1:38003 --hex $HEX
