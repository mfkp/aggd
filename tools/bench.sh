#!/bin/bash
# Run aggd and readsb side by side on the same synthetic traffic and
# report CPU and memory for each.
#   tools/bench.sh PLANES RECEIVERS [full|bare] [SECONDS]
# full: readsb with the options a tar1090 aggregator runs (globe index,
#       binCraft, traces); bare: aircraft.json and the API only.
# A client polls aggd's aircraft.json once a second throughout, since
# aggd renders on request where readsb writes every second regardless.
set -u
PLANES=${1:-300}; RX=${2:-12}; MODE=${3:-full}; SECS=${4:-60}
HERE=$(cd "$(dirname "$0")/.." && pwd)
W=${BENCH_DIR:-/tmp/aggd-bench}; mkdir -p $W/run $W/hist; chmod -R 777 $W 2>/dev/null
IMG=ghcr.io/sdr-enthusiasts/docker-adsb-ultrafeeder:latest
D="sg docker -c"
EXTRA=""; [ "$MODE" = full ] && EXTRA="--write-json-globe-index --write-globe-history /var/globe_history --heatmap 30"
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own
$D "docker rm -f aggd-oracle" >/dev/null 2>&1
[ -f $W/run/aircraft.csv.gz ] || { $D "docker create --name aggd-dbcopy $IMG" >/dev/null && $D "docker cp aggd-dbcopy:/usr/local/share/tar1090/aircraft.csv.gz $W/run/aircraft.csv.gz"; $D "docker rm aggd-dbcopy" >/dev/null; }
$D "docker run -d --name aggd-oracle --network host -v $W/run:/run/readsb -v $W/hist:/var/globe_history --entrypoint readsb $IMG --net --net-only --quiet --lat 41.9 --lon 12.5 --max-range 20000 --net-bi-port 39004 ${INGEST---net-ingest} --net-receiver-id --write-receiver-id-json --write-json /run/readsb --write-json-every 1 --net-json-port 39047 --net-json-port-interval 1 --net-api-port 39152 --json-reliable 1 --net-sbs-port 39003 --net-bo-port 39005 --net-beast-reduce-out-port 39006 --net-sbs-in-port 39033 --net-sbs-jaero-in-port 39034 --db-file /run/readsb/aircraft.csv.gz --db-file-lt ${ORACLE_ARGS:-} $EXTRA" >/dev/null
$HERE/target/release/aggd --lat 41.9 --lon 12.5 --net-bi-port 38004 --http-port 38080 --net-json-port 38047 --net-json-port-interval 1 --net-sbs-port 38003 --net-bo-port 38005 --net-beast-reduce-out-port 38006 --net-sbs-in-port 38033 --net-sbs-jaero-in-port 38034 --db-file $W/run/aircraft.csv.gz --db-file-lt --max-connections-per-ip 100000 ${AGGD_ARGS:-} > $W/aggd.log 2>&1 & own aggd
sleep 2
# the generator connects once: wait until readsb is listening (its
# container can take longer than aggd to come up on a busy machine)
for i in $(seq 60); do (exec 3<>/dev/tcp/127.0.0.1/39004) 2>/dev/null && break; sleep 0.5; done
$HERE/target/release/aggbench sky 127.0.0.1:38004 127.0.0.1:39004 --planes $PLANES --receivers $RX --box "${BENCH_BOX:-36,47,6,19}" ${SKY_ARGS:-} > $W/sky.log 2>&1 & own aggbench
(while :; do curl -s -o /dev/null -H 'Accept-Encoding: gzip' http://127.0.0.1:38080/data/aircraft.json; sleep 1; done) & POLL=$!
sleep ${WARMUP:-45}
# the reference's own process, not any other readsb on this machine
RP=$($D "docker inspect -f '{{.State.Pid}}' aggd-oracle"); AP=$(pgrep -x aggd | head -1)
cpu() { awk '{print $14+$15}' /proc/$1/stat; }
rss() { awk '/VmRSS/{print $2}' /proc/$1/status; }
r0=$(cpu $RP); a0=$(cpu $AP); t0=$(date +%s.%N)
sleep $SECS
r1=$(cpu $RP); a1=$(cpu $AP); t1=$(date +%s.%N)
HZ=$(getconf CLK_TCK)
python3 - <<PY
dt=$t1-$t0
r=($r1-$r0)/$HZ/dt*100; a=($a1-$a0)/$HZ/dt*100
print("traffic: $(tail -1 $W/sky.log)")
print(f"readsb ($MODE): CPU {r:6.1f} % of one core   RSS {$(rss $RP)/1024:7.1f} MB")
print(f"aggd          : CPU {a:6.1f} % of one core   RSS {$(rss $AP)/1024:7.1f} MB")
print(f"ratio readsb/aggd: CPU {r/max(a,0.01):.1f}x   RSS {$(rss $RP)/$(rss $AP):.1f}x")
PY
python3 $HERE/tools/compare.py $W/run/aircraft.json http://127.0.0.1:38080/data/aircraft.json --rounds 3
kill $POLL 2>/dev/null
[ "${KEEP:-0}" = 1 ] || { stop_own; $D "docker rm -f aggd-oracle" >/dev/null 2>&1; }
