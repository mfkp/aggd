# Sourced by the scripts here. They start aggd and aggbench and must stop
# them again, but only their own: run on a machine where the real aggd
# is working, a script that stopped every process of that name would
# take the aggregator down with it.
#   some-command & own NAME     note what was just started
#   stop_own [NAME]             stop what was noted (all, or those of one name)
own() { mkdir -p "${BENCH_DIR:-/tmp/aggd-bench}"; echo "$1 $!" >> "${BENCH_DIR:-/tmp/aggd-bench}/pids"; }
stop_own() {
    local f="${BENCH_DIR:-/tmp/aggd-bench}/pids" n p
    [ -f "$f" ] || return 0
    while read -r n p; do
        if [ -z "${1:-}" ] || [ "$1" = "$n" ]; then
            # the same process still, not another that has since been given the number
            [ "$(cat /proc/$p/comm 2>/dev/null)" = "$n" ] && kill "$p" 2>/dev/null
        else
            echo "$n $p"
        fi
    done < "$f" > "$f.keep"
    mv "$f.keep" "$f"
}
