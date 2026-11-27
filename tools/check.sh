#!/bin/bash
# Everything that can be checked on one machine, in order of cost:
# unit tests, malformed input on every port, hostile receivers, and a
# side-by-side run against readsb (needs Docker and the ultrafeeder image).
#   tools/check.sh [quick]
set -u
HERE=$(cd "$(dirname "$0")/.." && pwd); cd $HERE
export BENCH_DIR=${BENCH_DIR:-/tmp/aggd-bench}; mkdir -p $BENCH_DIR
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"   # cargo is not always on PATH
fail=0; step() { echo; echo "== $1"; }
step "build and unit tests"
cargo build --release 2>&1 | tail -1
cargo test --release 2>&1 | grep -E "^test result|FAILED|panicked" ; [ ${PIPESTATUS[0]} -eq 0 ] || fail=1
step "malformed input"
. "$(cd "$(dirname "$0")" && pwd)/own.sh"
stop_own; sleep 0.3
target/release/aggd --net-bi-port 38004 --http-port 38080 --net-sbs-in-port 38033 --net-sbs-jaero-in-port 38034 --max-connections-per-ip 1000 > $BENCH_DIR/fuzz.log 2>&1 & own aggd
sleep 0.5; target/release/aggbench sky 127.0.0.1:38004 --planes 300 --receivers 12 --seconds 60 > /dev/null 2>&1 & own aggbench
sleep 3
python3 tools/fuzz.py 127.0.0.1 38004 38033 38080 20 || fail=1
python3 tools/fuzz.py 127.0.0.1 38004 38034 38080 15 || fail=1
[ "$(grep -c panicked $BENCH_DIR/fuzz.log)" = 0 ] || { echo "panic in log"; fail=1; }
stop_own
step "hostile receivers (garbage, liars, flood)"
tools/hostile.sh 150 | tee $BENCH_DIR/hostile.out | tail -4
grep -q "distrusted exactly" $BENCH_DIR/hostile.out || { echo "lying receivers not singled out"; fail=1; }
step "hostile receivers, with 800 ms of delay and 30 % loss on every link"
SKY_ARGS="--jitter-ms 800 --loss 0.3" tools/hostile.sh 170 | tee $BENCH_DIR/hostile-jitter.out | tail -4
grep -q "distrusted exactly" $BENCH_DIR/hostile-jitter.out || { echo "lying receivers not singled out"; fail=1; }
step "hostile receivers that all tell the same lie"
SKY_ARGS="--collude" LIARS=3 RX=40 tools/hostile.sh 170 | tee $BENCH_DIR/hostile-collude.out | tail -4
grep -q "distrusted exactly" $BENCH_DIR/hostile-collude.out || { echo "colluding receivers not singled out"; fail=1; }
step "a colluding majority (25 liars, 10 honest receivers on trusted links)"
AGGD_ARGS="--net-trusted 127.0.0.1" SKY_ARGS="--collude --hostile-from 127.0.0.2" LIARS=25 RX=10 tools/hostile.sh 170 | tee $BENCH_DIR/hostile-majority.out | tail -5
# outnumbered, the honest receivers cannot have every liar convicted, but
# no aircraft may be moved and no honest receiver may be distrusted
grep -q "honest receivers distrusted: 0;" $BENCH_DIR/hostile-majority.out || { echo "an honest receiver was distrusted"; fail=1; }
grep -qE "^  (lat|lon)" $BENCH_DIR/hostile-majority.out && { echo "the lying majority moved an aircraft"; fail=1; }
grep -q "aircraft compared" $BENCH_DIR/hostile-majority.out || { echo "majority scenario did not run"; fail=1; }
[ "${1:-}" = quick ] && { echo; [ $fail = 0 ] && echo "PASS (quick)" || echo "FAIL"; exit $fail; }
step "against readsb: 4000 aircraft, 300 receivers"
INGEST="" KEEP=1 BENCH_BOX=35,60,-10,30 tools/bench.sh 4000 300 full 60 | tee $BENCH_DIR/bench.out
# a comparison of nothing is not agreement: readsb must have been heard
n=$(grep -o "aircraft compared: [0-9]*" $BENCH_DIR/bench.out | tail -1 | grep -o "[0-9]*$")
[ "${n:-0}" -ge 1000 ] || { echo "readsb comparison did not run (${n:-0} aircraft compared)"; fail=1; }
step "against readsb: one message of each kind"
stop_own aggbench; sleep 62
tools/run-probe.sh 43C${RANDOM:0:3} | grep -v "carried over)$" | tee $BENCH_DIR/probe.out | tail -25
grep -q "steps agree" $BENCH_DIR/probe.out || { echo "single-message probe did not run"; fail=1; }
stop_own aggd; sg docker -c "docker rm -f aggd-oracle" > /dev/null 2>&1
echo; [ $fail = 0 ] && echo "PASS (see the comparisons above for what differs from readsb)" || echo "FAIL"
exit $fail
