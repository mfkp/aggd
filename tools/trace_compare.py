#!/usr/bin/env python3
"""Compare the traces two aggregators hold for the same aircraft.

    trace_compare.py REFERENCE CANDIDATE [--kind recent|full] [--since UNIX] [--until UNIX]
                     [--min N] [--examples N] [--snap DIR]

REFERENCE and CANDIDATE are each a directory holding traces/xx/trace_*.json
(readsb's --write-json directory, or one day of a globe_history:
.../globe_history/YYYY/MM/DD) or the base URL of an HTTP server that
serves them (http://127.0.0.1:8080/data). The aircraft are those the
reference has. --since/--until keep only points in that span, so two
runs that did not start together can be compared over what they share.
--snap DIR saves what was read from a URL, laid out as a directory, so
the comparison can be repeated on the same data.

Prints, per aircraft, the candidate's number of points as a share of the
reference's (median, p10, p90), points per aircraft-hour, and what the
points only one of them has were doing: time since the previous point of
the same trace, and what had changed since it.
"""
import collections, glob, gzip, json, math, os, sys, urllib.request


def opt(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default


def read(src, kind, hexid):
    name = f"traces/{hexid[-2:]}/trace_{kind}_{hexid}.json"
    try:
        if src.startswith("http"):
            raw = urllib.request.urlopen(f"{src}/{name}", timeout=10).read()
        else:
            raw = open(f"{src}/{name}", "rb").read()
    except Exception:
        return None
    if raw[:2] == b"\x1f\x8b":
        raw = gzip.decompress(raw)
    try:
        return json.loads(raw)
    except Exception:
        return None


def hexes(src, kind):
    return sorted(os.path.basename(f)[len(f"trace_{kind}_"):-5] for f in glob.glob(f"{src}/traces/*/trace_{kind}_*.json"))


def points(doc, since, until):
    """[(unix time, lat, lon, alt, gs, track, flags)]"""
    if not doc:
        return []
    t0 = doc["timestamp"]
    return [(t0 + p[0], p[1], p[2], p[3], p[4], p[5], p[6]) for p in doc["trace"] if since <= t0 + p[0] <= until]


def adiff(a, b):
    d = abs(a - b) % 360
    return min(d, 360 - d)


def dist_m(a, b):
    c = math.cos(math.radians((a[1] + b[1]) / 2))
    return math.hypot((a[1] - b[1]) * 111195, (a[2] - b[2]) * 111195 * c)


def why(prev, p, interval):
    """What a point says that the one before it in the same trace did not."""
    if prev is None:
        return "first"
    dt = p[0] - prev[0]
    if p[6] & 2:
        return "new leg"
    if p[6] & 1:
        return "stale"
    if (p[3] == "ground") != (prev[3] == "ground"):
        return "ground<->air"
    if dt >= interval - 1.5:
        return "interval"
    out = []
    if p[3] == "ground":
        out.append("on ground")
    if isinstance(p[3], (int, float)) and isinstance(prev[3], (int, float)) and abs(p[3] - prev[3]) >= 100:
        out.append("alt")
    if p[5] is not None and prev[5] is not None and adiff(p[5], prev[5]) >= 0.5 and p[3] != "ground":
        out.append("track")
    if p[4] is not None and prev[4] is not None and abs(p[4] - prev[4]) >= 4 and p[3] != "ground":
        out.append("speed")
    return "+".join(out) or "nothing seen"


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, int(q * len(v)))] if v else float("nan")


def main():
    ref, cand = sys.argv[1], sys.argv[2]
    kind = opt("--kind", "recent")
    since, until = float(opt("--since", 0)), float(opt("--until", 1e12))
    least, nex = int(opt("--min", 10)), int(opt("--examples", 0))
    interval = float(opt("--interval", 30))
    window = float(opt("--window", 1.5))
    snap = opt("--snap")
    if ref.startswith("http"):
        sys.exit("the reference must be a directory: it says which aircraft to compare")
    ratios, tot = [], collections.Counter()
    only = {"ref": collections.Counter(), "cand": collections.Counter()}
    hours = {"ref": 0.0, "cand": 0.0}
    examples = []
    for h in hexes(ref, kind):
        # the candidate first: what it serves is as of now, the reference's file a little older
        cdoc = read(cand, kind, h)
        rdoc = read(ref, kind, h)
        if snap and cdoc and cand.startswith("http"):
            os.makedirs(f"{snap}/traces/{h[-2:]}", exist_ok=True)
            json.dump(cdoc, open(f"{snap}/traces/{h[-2:]}/trace_{kind}_{h}.json", "w"))
        r, c = points(rdoc, since, until), points(cdoc, since, until)
        if not r or not c:
            tot["only in reference" if r else "only in candidate" if c else "in neither"] += 1
            continue
        # only the span both cover: files are written at different moments,
        # and "recent" is a number of points, not a length of time
        lo, hi = max(r[0][0], c[0][0]), min(r[-1][0], c[-1][0])
        r_in = [p for p in r if lo - window <= p[0] <= hi + window]
        c_in = [p for p in c if lo - window <= p[0] <= hi + window]
        if len(r_in) < least:
            tot["too short"] += 1
            continue
        tot["aircraft"] += 1
        tot["ref points"] += len(r_in)
        tot["cand points"] += len(c_in)
        ratios.append(len(c_in) / len(r_in))
        for name, pts in (("ref", r_in), ("cand", c_in)):
            # time on the map: gaps of five minutes and more are not flight
            hours[name] += sum(min(b[0] - a[0], 300) for a, b in zip(pts, pts[1:]) if b[0] - a[0] < 300) / 3600
        for name, mine, other in (("ref", r_in, c_in), ("cand", c_in, r_in)):
            j = 0
            for i, p in enumerate(mine):
                while j < len(other) and other[j][0] < p[0] - window:
                    j += 1
                if j < len(other) and other[j][0] <= p[0] + window:
                    tot[f"{name} matched"] += name == "ref"
                    continue
                prev = mine[i - 1] if i else None
                w = why(prev, p, interval)
                only[name][w] += 1
                if name == "ref" and len(examples) < nex:
                    examples.append((h, w, prev, p))
    if not ratios:
        sys.exit(f"nothing to compare: {dict(tot)}")
    print(f"{kind}: {tot['aircraft']} aircraft compared ({', '.join(f'{v} {k}' for k, v in tot.items() if k in ('only in reference', 'only in candidate', 'too short', 'in neither'))})")
    print(f"points: reference {tot['ref points']}, candidate {tot['cand points']} ({100 * tot['cand points'] / tot['ref points']:.1f} %)")
    print(f"candidate/reference per aircraft: median {100 * pct(ratios, .5):.0f} %, p10 {100 * pct(ratios, .1):.0f} %, p90 {100 * pct(ratios, .9):.0f} %")
    for name in ("ref", "cand"):
        n = tot[f"{name} points"]
        print(f"{'reference' if name == 'ref' else 'candidate'}: {n / max(hours[name], 1e-9):.0f} points per aircraft-hour over {hours[name]:.1f} h")
    print(f"reference points the candidate has within {window} s: {100 * tot['ref matched'] / tot['ref points']:.1f} %")
    for name, label in (("ref", "only the reference has"), ("cand", "only the candidate has")):
        n = sum(only[name].values())
        print(f"{label}: {n}")
        for w, k in only[name].most_common(12):
            print(f"  {k:>7} {100 * k / max(n, 1):5.1f} %  {w}")
    for h, w, prev, p in examples:
        print(h, w, "dt", None if prev is None else round(p[0] - prev[0], 1), prev and prev[3:], p[3:])


if __name__ == "__main__":
    main()
