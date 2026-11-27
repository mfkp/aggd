#!/usr/bin/env python3
"""What an aggregator shows for the direction and speed of an aircraft
it has only MLAT positions for: BaseStation position lines with and
without speed and track, moving fast, slowly, and jittering in place.

    mlat_track_probe.py REF_BEAST REF_JSON CAND_BEAST CAND_JSON MLAT_PORT JAERO_PORT [SECONDS]

Both aggregators must connect to MLAT_PORT (sbs_in_mlat) on this host.
"""
import json, math, random, socket, sys, threading, time, urllib.request

def load(src):
    try:
        d = json.load(urllib.request.urlopen(src, timeout=5)) if src.startswith("http") else json.load(open(src))
        return {a["hex"]: a for a in d["aircraft"]}
    except Exception:
        return {}

class Fan:
    def __init__(self, port):
        self.conns = []; self.l = socket.socket(); self.l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.l.bind(("127.0.0.1", port)); self.l.listen(8)
        threading.Thread(target=self.run, daemon=True).start()
    def run(self):
        while True:
            c, _ = self.l.accept(); self.conns.append(c)
    def send(self, line):
        for c in list(self.conns):
            try: c.sendall(line.encode() + b"\r\n")
            except OSError: self.conns.remove(c)

def sbs(hexa, lat, lon, gs=None, trk=None, vr=None, alt=30000):
    t = time.time(); d = time.strftime("%Y/%m/%d", time.gmtime(t)); h = time.strftime("%H:%M:%S", time.gmtime(t)) + ".%03d" % (t % 1 * 1000)
    f = lambda x: "" if x is None else str(x)
    return f"MSG,3,1,1,{hexa},1,{d},{h},{d},{h},,{alt},{f(gs)},{f(trk)},{lat:.5f},{lon:.5f},{f(vr)},,0,,0,0"

# name: (hex, knots, track, seconds between lines, jitter in metres, speed and track in the line)
S = {
    "450 kt north, every second":      ("B00001", 450, 0, 1, 0, False),
    "450 kt east, every 4 s":          ("B00002", 450, 90, 4, 0, False),
    "60 kt south-west, every second":  ("B00003", 60, 225, 1, 0, False),
    "20 kt north, every second":       ("B00004", 20, 0, 1, 0, False),
    "standing, 60 m of jitter":        ("B00005", 0, 0, 1, 60, False),
    "450 kt north, 300 m of jitter":   ("B00006", 450, 0, 1, 300, False),
    "450 kt north, line has speed and track": ("B00007", 450, 0, 1, 0, True),
    "450 kt north, then turns east at 30 s":  ("B00008", 450, 0, 1, 0, False),
}
KEYS = ("gs", "track", "calc_track", "baro_rate", "geom_rate", "mlat", "type")

def main():
    rb, rj, cb, cj, mp, jp = sys.argv[1:7]
    length = int(sys.argv[7]) if len(sys.argv) > 7 else 60
    fan = Fan(int(mp)); Fan(int(jp)); random.seed(1)
    t = time.time()
    while len(fan.conns) < 2 and time.time() - t < 90: time.sleep(0.5)
    place = {n: [45.0 + i * 0.3, 9.0] for i, n in enumerate(S)}
    t0 = time.time(); last = {}; log = {n: [] for n in S}
    for sec in range(length + 1):
        while time.time() - t0 < sec: time.sleep(0.02)
        for n, (h, kt, trk, every, jit, full) in S.items():
            if "turns" in n and sec >= 30: trk = 90
            p = place[n]; m = kt * 0.5144
            p[0] += m * math.cos(math.radians(trk)) / 111_200
            p[1] += m * math.sin(math.radians(trk)) / (111_200 * math.cos(math.radians(p[0])))
            if sec % every: continue
            jl, jo = (random.uniform(-jit, jit) / 111_200 for _ in range(2))
            fan.send(sbs(h, p[0] + jl, p[1] + jo * 1.4, *((kt, trk, 0) if full else ())))
        time.sleep(0.6)
        r, c = load(rj), load(cj)
        for n, (h, *_ ) in S.items():
            show = lambda a: "absent" if a is None else " ".join(f"{k}={a[k]}" for k in KEYS if k in a)
            now = (show(r.get(h.lower())), show(c.get(h.lower())))
            if last.get(n) != now:
                log[n].append((sec, now)); last[n] = now
    for n in S:
        print(n)
        for j, who in ((0, "ref "), (1, "cand")):
            seq = [(t, x[j]) for i, (t, x) in enumerate(log[n]) if i == 0 or log[n][i - 1][1][j] != x[j]]
            print(f"  {who}: " + "\n        ".join(f"{t}s {x}" for t, x in seq[:int(sys.argv[8]) if len(sys.argv) > 8 else 14]))

main()
