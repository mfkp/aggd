#!/usr/bin/env python3
"""Collect every trace point an aggregator publishes, as it publishes them.

    trace_watch.py SOURCE OUT_DIR [--every SECONDS] [--seconds N] [--recent-only]

readsb rewrites trace_recent (its newest 92 points) every few seconds
but trace_full only every several minutes, and at exit neither: a short
run leaves no complete trace on disk. This reads SOURCE/traces/*/trace_recent_*
again and again (trace_full too when it changes) and keeps the union of
the points seen, written to OUT_DIR/traces/xx/trace_full_HEX.json in the
same layout, for trace_compare.py and the like. SOURCE is a directory.
Stops after --seconds, or on SIGTERM/SIGINT; the files are written then
and every minute.
"""
import glob, gzip, json, os, signal, sys, time


def opt(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default


def main():
    src, out = sys.argv[1], sys.argv[2]
    every, seconds = float(opt("--every", 3)), float(opt("--seconds", 0))
    recent_only = "--recent-only" in sys.argv
    seen, mtimes, head = {}, {}, {}
    stop = []
    for s in (signal.SIGTERM, signal.SIGINT):
        signal.signal(s, lambda *_: stop.append(1))

    def flush():
        for h, pts in seen.items():
            if not pts:
                continue
            # the same point read from trace_recent and from trace_full can
            # differ by a hundredth or two: each file counts from its own start
            ts = []
            for t in sorted(pts):
                if ts and t - ts[-1] <= 15 and pts[t][:2] == pts[ts[-1]][:2]:
                    continue
                ts.append(t)
            t0 = ts[0] / 100
            d = f"{out}/traces/{h[-2:]}"
            os.makedirs(d, exist_ok=True)
            doc = dict(head.get(h, {}), icao=h, timestamp=t0, trace=[[round(t / 100 - t0, 2)] + pts[t] for t in ts])
            with open(f"{d}/.tmp", "w") as f:
                json.dump(doc, f)
            os.replace(f"{d}/.tmp", f"{d}/trace_full_{h}.json")

    # carry on from an earlier run's output
    for f in glob.glob(f"{out}/traces/*/trace_full_*.json"):
        try:
            d = json.load(open(f))
        except Exception:
            continue
        seen[d["icao"]] = {round((d["timestamp"] + p[0]) * 100): p[1:] for p in d["trace"]}
        head[d["icao"]] = {k: v for k, v in d.items() if k not in ("trace", "timestamp", "icao")}
    start = last_flush = time.time()
    while not stop and (not seconds or time.time() - start < seconds):
        for f in glob.glob(f"{src}/traces/*/trace_{'recent' if recent_only else '*'}_*.json"):
            try:
                m = os.path.getmtime(f)
                if mtimes.get(f) == m:
                    continue
                raw = open(f, "rb").read()
                if raw[:2] == b"\x1f\x8b":
                    raw = gzip.decompress(raw)
                d = json.loads(raw)
            except Exception:
                continue  # being replaced as we read: next round
            mtimes[f] = m
            h = d["icao"]
            pts = seen.setdefault(h, {})
            head[h] = {k: v for k, v in d.items() if k not in ("trace", "timestamp")}
            for p in d["trace"]:
                pts[round((d["timestamp"] + p[0]) * 100)] = p[1:]
        if time.time() - last_flush > 60:
            flush()
            last_flush = time.time()
        time.sleep(every)
    flush()


if __name__ == "__main__":
    main()
