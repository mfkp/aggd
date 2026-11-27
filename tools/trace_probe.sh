#!/bin/bash
# Which positions does an aggregator keep in a trace? Scripted flights,
# each doing one thing at one rate (aggbench flight: a climb, a turn, a
# gap, a landing ...), are played to a fresh readsb (Docker) and a fresh
# aggd; then the points each kept are counted per flight, with how many
# fall at the same moment in both.
#   tools/trace_probe.sh SECONDS plane-spec...
#   tools/trace_probe.sh 400 alt=30000 alt=15000,vrate=1000,from=90 turn=1,from=90 gap=150-170 \
#       alt=3000,gs=130,vrate=-900,from=30,to=230,flip=230
# With no plane specs, a set that covers every rule found so far.
# INTERVAL (default 30) is --json-trace-interval for both; readsb's
# traces are collected as it publishes them (tools/trace_watch.py: it
# rewrites trace_full only every few minutes and not at exit).
# (So a landing shows fewer points for readsb than it has: the positions
# before touchdown that it keeps whole reach trace_full minutes later.)
# Results in $BENCH_DIR/trace-probe; ports PORT (default 38104), PORT+1
# and PORT+81; the readsb container is ORACLE (default aggd-oracle-trace).
HERE=$(cd "$(dirname "$0")/.." && pwd); W=${BENCH_DIR:-/tmp/aggd-bench}/trace-probe
SECS=${1:-400}; shift
[ $# -gt 0 ] || set -- alt=30000 start=40 start=40,pos=2500 \
  alt=1000,vrate=1000,from=90 alt=8000,vrate=1000,from=90 alt=15000,vrate=1000,from=90 alt=13000,vrate=500,from=90 \
  alt=13000,vrate=3000,from=90 alt=7100,vrate=300,from=90 alt=500,vrate=150,from=90 alt=13500,vrate=-2000,from=90 \
  turn=0.1,from=90 turn=0.5,from=90 turn=1,from=90 turn=3,from=90 turn=8,from=90 turn=14,from=90 \
  accel=0.2,from=90 accel=0.5,from=90 accel=3,from=90,to=200 rj=192,rjp=7 rj=256,rjp=7 \
  gap=150-158 gap=140-152 gap=150-170 gap=150-270 \
  alt=100,gs=0,flip=30 alt=100,gs=2,flip=30 alt=100,gs=10,flip=30 alt=100,gs=30,flip=30 alt=100,gs=60,flip=30 alt=100,gs=100,flip=30 \
  alt=3000,gs=130,vrate=-900,from=30,to=230,flip=230 alt=100,gs=20,flip=150 novel=1 pos=5000
INTERVAL=${INTERVAL:-30}; PORT=${PORT:-38104}; ORACLE=${ORACLE:-aggd-oracle-trace}
IMG=ghcr.io/sdr-enthusiasts/docker-adsb-ultrafeeder:latest
rm -rf $W 2>/dev/null; mkdir -p $W/readsb $W/aggd $W/watch; chmod 777 $W/readsb
sg docker -c "docker rm -f $ORACLE" >/dev/null 2>&1
sg docker -c "docker run -d --name $ORACLE --network host -v $W/readsb:/run/readsb --entrypoint readsb $IMG --net --net-only --quiet --lat 41.9 --lon 12.5 --max-range 0 --net-bi-port $PORT --write-json /run/readsb --write-json-every 1 --write-json-globe-index --json-trace-interval $INTERVAL ${ORACLE_ARGS:-}" >/dev/null
$HERE/target/release/aggd --net-bi-port $((PORT+1)) --net-trusted 127.0.0.1 --http-port $((PORT+81)) --lat 41.9 --lon 12.5 --json-trace-interval $INTERVAL > $W/aggd.log 2>&1 &
AGGD=$!
sleep 3; date +%s > $W/start
python3 $HERE/tools/trace_watch.py $W/readsb $W/watch --every 2 > /dev/null 2>&1 &
WATCH=$!
PLANES=(); for p in "$@"; do PLANES+=(--plane "$p"); done
$HERE/target/release/aggbench flight 127.0.0.1:$PORT 127.0.0.1:$((PORT+1)) --seconds $SECS "${PLANES[@]}" || { kill $AGGD $WATCH; exit 1; }
sleep 4
kill -TERM $WATCH; wait $WATCH 2>/dev/null
python3 - $W $((PORT+81)) "$@" <<'EOF'
import bisect, json, sys, urllib.request
w, port, specs = sys.argv[1], sys.argv[2], sys.argv[3:]
def times(doc):
    return [doc["timestamp"] + p[0] for p in doc["trace"]]
both = [0, 0]
for i, spec in enumerate(specs):
    # (aggbench flight numbers the aircraft from 4D0000 unless a spec says hex=)
    h = next((kv[4:].lower() for kv in spec.split(",") if kv.startswith("hex=")), "%06x" % (0x4D0000 + i))
    try: r = times(json.load(open(f"{w}/watch/traces/{h[-2:]}/trace_full_{h}.json")))
    except Exception: r = []
    try: c = times(json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/data/traces/{h[-2:]}/trace_full_{h}.json", timeout=5)))
    except Exception: c = []
    same = sum(1 for t in r if (lambda j: (j < len(c) and c[j] - t <= 0.75) or (j > 0 and t - c[j - 1] <= 0.75))(bisect.bisect_left(c, t)))
    both[0] += len(r); both[1] += len(c)
    print(f"{h} {spec:50s} readsb {len(r):4d}  aggd {len(c):4d}  at the same moment {same:4d}{'' if abs(len(c) - len(r)) <= max(3, 0.05 * len(r)) else '   <--'}")
print(f"points: readsb {both[0]}, aggd {both[1]} ({100 * both[1] / max(both[0], 1):.1f} %)")
EOF
[ "${KEEP:-0}" = 1 ] || { kill -TERM $AGGD; sg docker -c "docker rm -f $ORACLE" >/dev/null 2>&1; }
