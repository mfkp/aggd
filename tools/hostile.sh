#!/bin/bash
# Two aggd side by side on the same clean traffic; the first also gets
# receivers that send garbage, lie about positions, and invent aircraft.
# Its output must stay what the clean one's is.
# SKY_ARGS adds generator options: "--jitter-ms 800 --loss 0.3", "--collude".
# AGGD_ARGS adds options to the aggd under attack (e.g. --net-trusted).
#   tools/hostile.sh [SECONDS]
HERE=$(cd "$(dirname "$0")/.." && pwd); W=${BENCH_DIR:-/tmp/aggd-bench}; mkdir -p $W; SECS=${1:-150}
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own; sleep 0.3
$HERE/target/release/aggd --net-bi-port 38004 --http-port 38080 --max-connections-per-ip 100000 ${AGGD_ARGS:-} > $W/hostile-a.log 2>&1 & own aggd
$HERE/target/release/aggd --net-bi-port 37004 --http-port 37080 --max-connections-per-ip 100000 > $W/hostile-b.log 2>&1 & own aggd
sleep 0.5
$HERE/target/release/aggbench sky 127.0.0.1:38004 127.0.0.1:37004 --planes ${PLANES:-600} --receivers ${RX:-30} --garbage 2 --liars ${LIARS:-2} --flood 1 ${SKY_ARGS:-} --seconds $((SECS + 30)) > $W/hostile-sky.log 2>&1 & own aggbench
sleep $SECS
grep -v ' on 0.0.0.0' $W/hostile-a.log | sort | uniq -c
python3 $HERE/tools/compare.py http://127.0.0.1:37080/data/aircraft.json http://127.0.0.1:38080/data/aircraft.json --rounds 3; rc=$?
curl -s http://127.0.0.1:37080/data/aircraft.json -o $W/hostile-ref.json; curl -s http://127.0.0.1:38080/data/aircraft.json -o $W/hostile-cand.json
# exactly the receivers that lied must be the ones distrusted
liars=$(grep 'sky: liar' $W/hostile-sky.log | awk '{print $3}' | sort | tr '\n' ' ')
caught=$(grep 'positions ignored' $W/hostile-a.log | awk '{print $3}' | sort -u | tr '\n' ' ')
if [ "$liars" = "$caught" ]; then echo "distrusted exactly the lying receivers: $caught"; else echo "MISMATCH: lied [$liars] distrusted [$caught]"; rc=1; fi
# the weaker statement, for when the liars outnumber the honest: none of
# the honest distrusted, however many of the liars were
wrong=$(comm -13 <(echo $liars | tr ' ' '\n' | sort) <(echo $caught | tr ' ' '\n' | sort) | grep -c .)
echo "honest receivers distrusted: $wrong; liars distrusted: $(echo $caught | wc -w) of $(echo $liars | wc -w)"
curl -s http://127.0.0.1:38080/data/status.json | grep protection
stop_own
exit $rc
