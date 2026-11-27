#!/usr/bin/env python3
"""Receiver ids on the Beast outputs: what an aggregator run with
--net-receiver-id puts before each frame on its full and reduced Beast
outputs, for a receiver that gave a UUID, one that gave none, and one
whose frames arrive already marked with a receiver id.

    rid_probe.py IN FULL_OUT REDUCED_OUT
"""
import os, socket, subprocess, sys, threading, time
HERE = os.path.dirname(os.path.abspath(__file__))
def hp(s):
    h, p = s.rsplit(":", 1); return h, int(p)
def esc(b): return b.replace(b"\x1a", b"\x1a\x1a")
def beast(frame, ticks, sig=100):
    return b"\x1a" + (b"2" if len(frame) == 7 else b"3") + esc(ticks.to_bytes(6, "big") + bytes([sig]) + frame)
def frames(hexa):
    out = subprocess.run([os.path.join(HERE, "../target/release/aggbench"), "frames", "--hex", hexa], capture_output=True, text=True, check=True).stdout
    return {l.split()[0]: bytes.fromhex(l.split()[1]) for l in out.split("\n") if l}
class Tap(threading.Thread):
    def __init__(self, addr):
        super().__init__(daemon=True); self.buf = b""; self.s = socket.create_connection(hp(addr)); self.start()
    def run(self):
        while True:
            d = self.s.recv(65536)
            if not d: return
            self.buf += d
def parse(b):
    """[(receiver id or None, type, payload hex)] and the other escapes seen"""
    i, out, rid, other = 0, [], None, []
    L = {0x31: 9, 0x32: 14, 0x33: 21, 0xE3: 8, 0xE4: 36}
    while i + 1 < len(b):
        if b[i] != 0x1A: i += 1; continue
        t = b[i + 1]; i += 2
        if t not in L: other.append("%02x" % t); continue
        body = bytearray()
        while len(body) < L[t] and i < len(b):
            if b[i] == 0x1A:
                if i + 1 < len(b) and b[i + 1] == 0x1A: i += 1
                else: break
            body.append(b[i]); i += 1
        if t == 0xE3: rid = body.hex()
        elif t == 0xE4: other.append("e4:" + body.decode("latin1"))
        else: out.append((rid, t, body[7:].hex())); rid = None
    return out, other
def main():
    full, red = Tap(sys.argv[2]), Tap(sys.argv[3])
    a = socket.create_connection(hp(sys.argv[1])); b = socket.create_connection(hp(sys.argv[1])); c = socket.create_connection(hp(sys.argv[1]))
    a.sendall(b"\x1a\xe4" + b"11111111-2222-3333-4444-555555555555"); time.sleep(0.5)
    who = {}
    for sec in range(6):
        t = int(time.time() * 12e6) & 0xFFFFFFFFFFFF
        for k, (conn, hexa, pre) in enumerate(((a, "A1A001", b""), (b, "A1A002", b""), (c, "A1A003", b"\x1a\xe3" + esc(bytes.fromhex("0123456789abcdef"))))):
            f = frames(hexa)
            for j, n in enumerate(("df11", "pos_odd" if sec % 2 else "pos_even", "velocity")):
                who[f[n].hex()] = ("uuid", "plain", "marked")[k]
                conn.sendall(pre + beast(f[n], t + k * 1000 + j * 100))
        time.sleep(1)
    time.sleep(1.5)
    for name, tap in (("full", full), ("reduced", red)):
        out, other = parse(tap.buf)
        print(f"{name}: {len(out)} frames, other escapes {sorted(set(other))}")
        seen = {}
        for rid, t, payload in out:
            seen.setdefault((who.get(payload, "?"), rid), 0); seen[(who.get(payload, "?"), rid)] += 1
        for (w, rid), n in sorted(seen.items(), key=str): print(f"   from the {w} receiver: {n} frames, receiver id {rid}")
main()
