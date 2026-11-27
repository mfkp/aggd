#!/usr/bin/env python3
"""An aircraft placed by a BaseStation feed and also heard directly:
which of its own fields (version, the accuracy figures, category) an
aggregator goes on showing, and what kind of aircraft it calls it.

    sbs_src_probe.py REF_BEAST REF_JSON CAND_BEAST CAND_JSON SBS_PORT [SECONDS]

Both aggregators must connect to SBS_PORT (sbs_in) on this host.
"""
import json, os, socket, subprocess, sys, threading, time, urllib.request
HERE = os.path.dirname(os.path.abspath(__file__))
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
def frames(hexa):
    out = subprocess.run([os.path.join(HERE, "../target/release/aggbench"), "frames", "--hex", hexa], capture_output=True, text=True, check=True).stdout
    return {l.split()[0]: bytes.fromhex(l.split()[1]) for l in out.split("\n") if l}
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
def sbs(hexa, lat, lon, alt=30000):
    t = time.time(); d = time.strftime("%Y/%m/%d", time.gmtime(t)); h = time.strftime("%H:%M:%S", time.gmtime(t)) + ".%03d" % (t % 1 * 1000)
    return f"MSG,3,1,1,{hexa},1,{d},{h},{d},{h},,{alt},,,{lat:.5f},{lon:.5f},,,0,,0,0"
E = 999
# kinds: adsb (positions, velocity, identification, status, target state,
# DF11), es (the same without positions), modes (DF11, DF4), sbs (a
# BaseStation position line every 2 s)
S = {
    "sbs only":                         ("C00001", [("sbs", 0, E)]),
    "adsb, then sbs too from 20":       ("C00002", [("adsb", 0, E), ("sbs", 20, E)]),
    "sbs, then adsb too from 20":       ("C00003", [("sbs", 0, E), ("adsb", 20, E)]),
    "squitters without positions + sbs": ("C00004", [("es", 0, E), ("sbs", 0, E)]),
    "sbs, squitters from 20":           ("C00005", [("sbs", 0, E), ("es", 20, E)]),
    "adsb until 20, sbs throughout":    ("C00006", [("adsb", 0, 20), ("sbs", 0, E)]),
    "adsb until 20, then sbs":          ("C00007", [("adsb", 0, 20), ("sbs", 22, E)]),
    "squitters until 20, sbs throughout": ("C00008", [("es", 0, 20), ("sbs", 0, E)]),
    "mode s + sbs":                     ("C00009", [("modes", 0, E), ("sbs", 0, E)]),
    "adsb positions until 20, squitters on, sbs": ("C0000A", [("adsb", 0, 20), ("es", 21, E), ("sbs", 0, E)]),
}
KEYS = ("type", "version", "category", "nic", "rc", "nac_p", "nac_v", "sil", "sil_type", "gva", "sda", "nic_baro", "flight")
def state(a):
    if a is None: return "absent"
    src = "none" if a.get("lat") is None else ("sbs" if abs(a["lat"] - 45.52) < 0.005 else "own")
    return f"pos={src} " + " ".join(f"{k}={str(a[k]).strip()}" for k in KEYS if k in a)
MAX = int(os.environ.get("MAX", 6))
def main():
    rb, rj, cb, cj, sp = sys.argv[1:6]
    length = int(sys.argv[6]) if len(sys.argv) > 6 else 150
    fan = Fan(int(sp)); t = time.time()
    while len(fan.conns) < 2 and time.time() - t < 90: time.sleep(0.5)
    socks = [socket.create_connection(hp(x)) for x in (rb, cb)]
    fr = {n: frames(h) for n, (h, _) in S.items()}
    t0 = time.time(); last = {}; log = {n: [] for n in S}
    for sec in range(length + 1):
        while time.time() - t0 < sec: time.sleep(0.02)
        ticks = int(time.time() * 12e6) & 0xFFFFFFFFFFFF; buf = b""
        for n, (h, parts) in S.items():
            f = fr[n]
            for kind, start, until in parts:
                if not start <= sec <= until: continue
                if kind == "modes": buf += beast(f["df11"], ticks) + beast(f["df4"], ticks + 1200)
                elif kind in ("adsb", "es"):
                    if kind == "adsb": buf += beast(f["pos_odd" if sec % 2 else "pos_even"], ticks)
                    buf += beast(f["velocity"], ticks + 1200) + beast(f["df11"], ticks + 2400)
                    if sec % 5 == 0: buf += beast(f["ident"], ticks + 3600) + beast(f["op_status"], ticks + 4800) + beast(f["target_state"], ticks + 6000)
                elif sec % 2 == 0: fan.send(sbs(h, 45.52, 9.25))
        for s in socks: s.sendall(buf)
        time.sleep(0.6)
        r, c = load(rj), load(cj)
        for n, (h, _) in S.items():
            now = (state(r.get(h.lower())), state(c.get(h.lower())))
            if last.get(n) != now:
                log[n].append((sec, now)); last[n] = now
    bad = 0
    for n in S:
        seq = lambda j: [(t, x[j]) for i, (t, x) in enumerate(log[n]) if (i == 0 or log[n][i - 1][1][j] != x[j]) and x[j] != "absent"]
        ref, cand = seq(0), seq(1)
        same = [x for _, x in ref] == [x for _, x in cand] and all(abs(a[0] - b[0]) <= 3 for a, b in zip(ref, cand))
        bad += not same
        print(f"{'same' if same else 'DIFF'} {n}")
        for who, l in (("ref ", ref), ("cand", cand)):
            if who == "cand" and same: continue
            for t, x in l[:MAX]: print(f"   {who} {t:3d}s {x}")
            if len(l) > MAX: print(f"   {who} ... {len(l) - MAX} more changes, the last at {l[-1][0]}s: {l[-1][1]}")
    print(f"{len(S) - bad} of {len(S)} scenarios agree")
main()
