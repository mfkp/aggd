#!/usr/bin/env python3
"""Two places under one address: what an aggregator shows when position
messages for an aircraft come from somewhere it cannot have got to. The
same scenarios are played to a reference and a candidate, each through
one receiver and through two that relay the same frames.

    jump_probe.py REF_BEAST REF_JSON CAND_BEAST CAND_JSON [SECONDS]
"""
import json, os, socket, subprocess, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
A = (41.90, 12.50)

def hp(s):
    h, p = s.rsplit(":", 1); return h, int(p)

def beast(frame, ticks):
    body = ticks.to_bytes(6, "big") + bytes([120]) + frame
    return b"\x1a" + (b"2" if len(frame) == 7 else b"3") + body.replace(b"\x1a", b"\x1a\x1a")

def load(src):
    try:
        d = json.load(urllib.request.urlopen(src, timeout=5)) if src.startswith("http") else json.load(open(src))
        return {a["hex"]: a for a in d["aircraft"]}
    except Exception:
        return {}

def frames(hexa, lat, lon):
    out = subprocess.run([os.path.join(HERE, "../target/release/aggbench"), "frames", "--hex", hexa, "--lat", str(lat), "--lon", str(lon)], capture_output=True, text=True, check=True).stdout
    return {l.split()[0]: bytes.fromhex(l.split()[1]) for l in out.split("\n") if l}

FAR = (35.0, -100.0)      # 9,000 km
NEAR = (44.6, 12.5)       # 300 km
E = 999
# name: (place of the second aircraft, [(who, from, until, messages a second)])
S = {
    "both, far":            (FAR,  [("a", 0, E, 2), ("b", 20, 60, 2)]),
    "both, 300 km":         (NEAR, [("a", 0, E, 2), ("b", 20, 60, 2)]),
    "second four times as often": (FAR, [("a", 0, E, 1), ("b", 20, 60, 4)]),
    "first stops, second starts": (FAR, [("a", 0, 20, 2), ("b", 20, E, 2)]),
    "first stops, second 300 km": (NEAR, [("a", 0, 20, 2), ("b", 20, E, 2)]),
    "one pair from afar":   (FAR,  [("a", 0, E, 2), ("b", 20, 20, 2)]),
    "three seconds from afar": (FAR, [("a", 0, 19, 2), ("b", 20, 22, 4), ("a", 23, E, 2)]),
    "ten seconds from afar": (FAR, [("a", 0, 19, 2), ("b", 20, 29, 2), ("a", 30, E, 2)]),
}

if os.environ.get("SET") == "switch":
    # how long after the first goes quiet the second is believed, by
    # distance (degrees of latitude north) and message rate
    S = {}
    for km in (30, 60, 120, 300, 1000, 3000):
        for rate in (1, 2, 4):
            S[f"{km} km, {rate}/s"] = ((A[0] + km / 111.2 * (1 if km < 3000 else -1), A[1]), [("a", 0, 20, 2), ("b", 21, E, rate)])
    S["9000 km, 1/s"] = (FAR, [("a", 0, 20, 2), ("b", 21, E, 1)])
    S["9000 km, 4/s"] = (FAR, [("a", 0, 20, 2), ("b", 21, E, 4)])

def state(a, b):
    if a is None: return "absent"
    if a.get("lat") is None: return "nowhere"
    for name, p in (("first", A), ("second", b)):
        if abs(a["lat"] - p[0]) < 0.5 and abs(a["lon"] - p[1]) < 0.5: return name
    return "%.1f,%.1f" % (a["lat"], a["lon"])

def main():
    rb, rj, cb, cj = sys.argv[1:5]
    length = int(sys.argv[5]) if len(sys.argv) > 5 else 80
    # two receivers into each aggregator
    socks = [[socket.create_connection(hp(x)) for _ in range(2)] for x in (rb, cb)]
    cases = {}
    for i, (n, (b, parts)) in enumerate(S.items()):
        for nrx in (1, 2):
            h = "A1%02X%02X" % (nrx, i)
            cases[f"{n}, {nrx} rx"] = (h.lower(), b, parts, nrx, {"a": frames(h, *A), "b": frames(h, *b)})
    t0 = time.time(); step = 0; last = {}; log = {n: [] for n in cases}
    while step / 4 <= length:
        if time.time() - t0 < step / 4:
            time.sleep(0.01); continue
        sec = step // 4; q = step % 4
        ticks = int(time.time() * 12e6) & 0xFFFFFFFFFFFF; bufs = [b"", b""]
        for n, (h, b, parts, nrx, fr) in cases.items():
            for who, start, until, rate in parts:
                if not start <= sec <= until: continue
                if q % (4 // rate): continue
                k = (step // (4 // rate))
                # the two aircraft never in the same quarter second
                off = 0 if who == "a" else 6_000_000 // 4
                f = beast(fr[who]["pos_odd" if k % 2 else "pos_even"], ticks + off)
                if q == 0: f += beast(fr[who]["velocity"], ticks + off + 1200) + beast(fr[who]["df11"], ticks + off + 2400)
                for r in range(nrx): bufs[r] += f
        for pair in socks:
            for r in range(2):
                if bufs[r]: pair[r].sendall(bufs[r])
        if q == 3:
            time.sleep(0.15)
            r, c = load(rj), load(cj)
            for n, (h, b, *_ ) in cases.items():
                now = (state(r.get(h), b), state(c.get(h), b))
                if last.get(n) != now:
                    log[n].append((sec, now)); last[n] = now
        step += 1
    bad = 0
    for n in cases:
        seq = lambda j: [(t, x[j]) for i, (t, x) in enumerate(log[n]) if (i == 0 or log[n][i - 1][1][j] != x[j])]
        ref, cand = ([x for x in seq(j) if x[1] != "absent"] for j in (0, 1))
        # how soon a new aircraft is first shown is another matter
        ref, cand = ([x for i, x in enumerate(l) if x[0] >= 10 or i == len(l) - 1 or l[i + 1][0] >= 10] for l in (ref, cand))
        ref, cand = ([(0 if t < 10 else t, x) for t, x in l] for l in (ref, cand))
        same = [x for _, x in ref] == [x for _, x in cand] and all(abs(a[0] - b[0]) <= 15 for a, b in zip(ref, cand))
        bad += not same
        show = lambda l: "  ".join(f"{t}s {x}" for t, x in l)
        print(f"{'same' if same else 'DIFF'} {n}\n      ref:  {show(ref)}" + ("" if same else f"\n      cand: {show(cand)}"))
    print(f"{len(cases) - bad} of {len(cases)} scenarios agree")
    sys.exit(1 if bad else 0)

main()
