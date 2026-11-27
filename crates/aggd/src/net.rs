//! The wire: Beast in from receivers, and Beast, BaseStation and JSON
//! lines out to whoever listens.

use crate::receivers::{rid_of, Client};
use crate::track::{self, Aircraft, External, Ms, Pos};
use crate::{json, now_ms, Ctx};
use modes::{AddrType, Msg, Source};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

pub type Chunk = Arc<Vec<u8>>;

/// The output streams. A stream costs nothing while nobody subscribes.
pub struct Hub {
    pub full: broadcast::Sender<Chunk>,
    pub reduced: broadcast::Sender<Chunk>,
    pub reduced_plus: broadcast::Sender<Chunk>,
    pub sbs: broadcast::Sender<Chunk>,
    pub sbs_jaero: broadcast::Sender<Chunk>,
    pub json: broadcast::Sender<Chunk>,
}

impl Hub {
    pub fn new() -> Self {
        let ch = || broadcast::channel(4096).0;
        Hub { full: ch(), reduced: ch(), reduced_plus: ch(), sbs: ch(), sbs_jaero: ch(), json: ch() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    BeastIn,
    BeastOut,
    BeastReduceOut,
    BeastReducePlusOut,
    SbsIn,
    SbsInMlat,
    SbsInJaero,
    SbsInPrio,
    UatIn,
    SbsOut,
    SbsJaeroOut,
    JsonOut,
}

impl Proto {
    pub fn parse(s: &str) -> Option<Proto> {
        Some(match s {
            "beast_in" => Proto::BeastIn,
            "beast_out" => Proto::BeastOut,
            "beast_reduce_out" => Proto::BeastReduceOut,
            "beast_reduce_plus_out" => Proto::BeastReducePlusOut,
            "sbs_in" => Proto::SbsIn,
            "sbs_in_mlat" => Proto::SbsInMlat,
            "sbs_in_jaero" => Proto::SbsInJaero,
            "sbs_in_prio" => Proto::SbsInPrio,
            "uat_in" => Proto::UatIn,
            "sbs_out" => Proto::SbsOut,
            "sbs_out_jaero" => Proto::SbsJaeroOut,
            "json_out" => Proto::JsonOut,
            _ => return None,
        })
    }
    fn is_input(self) -> bool {
        matches!(self, Proto::BeastIn | Proto::SbsIn | Proto::SbsInMlat | Proto::SbsInJaero | Proto::SbsInPrio | Proto::UatIn)
    }
}

/// One Beast message body: timestamp, signal and frame, unescaped.
struct Frame<'a> {
    kind: u8,
    ts: &'a [u8],
    signal: u8,
    bytes: &'a [u8],
}

enum Event<'a> {
    Frame(Frame<'a>),
    Uuid(&'a [u8]),
    ReceiverId(u64),
    /// a ping coming back: our clock value plus the time the feeder held it
    Ping(u32),
}

/// Split a Beast stream into events. Returns the number of bytes
/// consumed; the rest is an incomplete message to retry with more data.
fn parse<'a>(b: &'a [u8], scratch: &'a mut [u8; 32], mut on: impl FnMut(Event<'_>)) -> usize {
    let mut pos = 0;
    loop {
        let Some(off) = b[pos..].iter().position(|&x| x == 0x1a) else { return b.len() };
        pos += off;
        let Some(&t) = b.get(pos + 1) else { return pos };
        let body = match t {
            0x31 => 9,
            0x32 => 14,
            0x33 | 0x34 => 21,
            0xE3 => 8,
            b'P' => 3,
            0xE4 => {
                // the UUID is plain text, not escaped
                if b.len() < pos + 38 {
                    return pos;
                }
                on(Event::Uuid(&b[pos + 2..pos + 38]));
                pos += 38;
                continue;
            }
            b'W' => {
                if b.len() < pos + 3 {
                    return pos;
                }
                pos += 3;
                continue;
            }
            _ => {
                pos += 1;
                continue;
            }
        };
        let mut i = pos + 2;
        let mut n = 0;
        let mut broken = false;
        while n < body {
            let Some(&x) = b.get(i) else { return pos };
            if x == 0x1a {
                match b.get(i + 1) {
                    None => return pos,
                    Some(0x1a) => i += 1,
                    Some(_) => {
                        broken = true;
                        break;
                    }
                }
            }
            scratch[n] = x;
            n += 1;
            i += 1;
        }
        pos = i;
        if broken {
            continue;
        }
        match t {
            0xE3 => on(Event::ReceiverId(u64::from_be_bytes(scratch[..8].try_into().unwrap()))),
            b'P' => on(Event::Ping((scratch[0] as u32) << 16 | (scratch[1] as u32) << 8 | scratch[2] as u32)),
            0x32 | 0x33 => on(Event::Frame(Frame { kind: t, ts: &scratch[..6], signal: scratch[6], bytes: &scratch[7..body] })),
            _ => {}
        }
    }
}

/// How long after a checked message a reply addressed by parity alone is taken.
const PARITY_AFTER_CHECKED: Ms = 15_000;

const MLAT_MAGIC: [u8; 6] = [0xFF, 0x00, 0x4D, 0x4C, 0x41, 0x54];

fn push_escaped(out: &mut Vec<u8>, data: &[u8]) {
    for &b in data {
        out.push(b);
        if b == 0x1a {
            out.push(0x1a);
        }
    }
}

fn beast_encode(out: &mut Vec<u8>, f: &Frame<'_>) {
    out.push(0x1a);
    out.push(f.kind);
    push_escaped(out, f.ts);
    push_escaped(out, &[f.signal]);
    push_escaped(out, f.bytes);
}

/// Message class for rate limiting on the reduced stream, and how often
/// that class may pass, in units of the reduce interval.
fn reduce_kind(m: &Msg) -> (usize, u64) {
    match m.df {
        17 | 18 => match m.tc {
            1..=4 => (2, 8),
            5..=18 | 20..=22 => (m.cpr.map_or(0, |c| c.odd as usize), 1),
            19 => (3, 1),
            28 => (4, 4),
            29 => (5, 4),
            31 => (6, 8),
            _ => (7, 8),
        },
        11 => (8, 4),
        4 | 20 => (9, 2),
        5 | 21 => (10, 4),
        _ => (11, 4),
    }
}

/// UTC date and time of day for BaseStation lines.
fn sbs_time(ms: Ms) -> String {
    let secs = ms / 1000;
    let days = (secs / 86400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    let s = secs % 86400;
    format!("{:04}/{:02}/{:02},{:02}:{:02}:{:02}.{:03}", y, m, d, s / 3600, s / 60 % 60, s % 60, ms % 1000)
}

/// "2026/11/16", "11:46:13.267" (UTC) to milliseconds since the epoch.
fn sbs_parse_time(date: &str, time: &str) -> Option<Ms> {
    let mut d = date.trim().split('/').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.trim().split(':');
    let (h, mi) = (t.next()?.parse::<u64>().ok()?, t.next()?.parse::<u64>().ok()?);
    let sec = t.next()?.parse::<f64>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || !(0.0..61.0).contains(&sec) {
        return None;
    }
    // days from civil
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days >= 0).then(|| days as u64 * 86_400_000 + h * 3_600_000 + mi * 60_000 + (sec * 1000.0) as u64)
}

/// One BaseStation line for a message: its type by format, and
/// whatever of the twelve data fields the message itself carried.
fn sbs_line(out: &mut Vec<u8>, a: &mut Aircraft, m: &Msg, pos: Pos, now: Ms) {
    use std::io::Write;
    let ty = match m.df {
        17 | 18 => match m.tc {
            1..=4 => 1,
            5..=8 => 2,
            0 | 9..=18 | 20..=22 => 3,
            19 => 4,
            _ => return,
        },
        4 | 20 => 5,
        5 | 21 => 6,
        0 | 16 => 7,
        11 => 8,
        _ => return,
    };
    // (Identification goes out each time it is heard, as readsb sends it:
    // said only when it changed, a consumer that connected after an
    // aircraft's first never learned its callsign.)
    let t = sbs_time(now);
    let tilde = if a.addr & modes::NON_ICAO != 0 { "~" } else { "" };
    let _ = write!(out, "MSG,{},1,1,{}{:06X},1,{},{},", ty, tilde, a.addr & 0xFF_FFFF, t, t);
    let opt = |v: Option<String>| v.unwrap_or_default();
    let cs = opt(m.callsign.map(|c| String::from_utf8_lossy(&c).into_owned()));
    let alt = if ty == 2 { String::new() } else { opt(m.alt_baro.or(m.acas_alt).map(|v| v.to_string())) };
    let gs = opt(m.gs.map(|v| format!("{v:.0}")));
    let trk = opt(m.track.map(|v| format!("{v:.0}")));
    let (lat, lon) = if matches!(ty, 2 | 3) && pos != Pos::None {
        (format!("{:.6}", a.pos.v.0), format!("{:.6}", a.pos.v.1))
    } else {
        (String::new(), String::new())
    };
    let vr = opt(m.baro_rate.or(m.geom_rate).map(|v| v.to_string()));
    let sq = opt(m.squawk.map(|v| format!("{v:04x}")));
    let flag = |v: Option<bool>| v.map_or("", |b| if b { "-1" } else { "0" });
    let bit = |v: Option<bool>| v.map_or("", |b| if b { "1" } else { "0" });
    let (alert, emergency, spi) = match ty {
        3 | 5 => (bit(m.alert), "", bit(m.spi)),
        6 => (bit(m.alert), if matches!(m.squawk, Some(0x7500 | 0x7600 | 0x7700)) { "1" } else { "0" }, bit(m.spi)),
        _ => ("", "", ""),
    };
    let ground = match ty {
        // a surveillance reply says "on the ground" reliably, "airborne" not
        5 | 6 => if m.ground == Some(true) { "-1" } else { "" },
        7 => "",
        _ => flag(m.ground),
    };
    let _ = write!(out, "{cs},{alt},{gs},{trk},{lat},{lon},{vr},{sq},{alert},{emergency},{spi},{ground}\r\n");
}

/// A BaseStation position line for a position that came in solved.
fn sbs_external(out: &mut Vec<u8>, a: &Aircraft, e: &External, now: Ms) {
    use std::io::Write;
    let t = sbs_time(now);
    let tilde = if a.addr & modes::NON_ICAO != 0 { "~" } else { "" };
    let opt = |v: Option<String>| v.unwrap_or_default();
    let _ = write!(
        out,
        "MSG,3,1,1,{}{:06X},1,{},{},{},{},{},{},{:.6},{:.6},{},{},,,,\r\n",
        tilde,
        a.addr & 0xFF_FFFF,
        t,
        t,
        opt(e.callsign.map(|c| String::from_utf8_lossy(&c).into_owned())),
        opt(e.alt_baro.map(|v| v.to_string())),
        opt(e.gs.map(|v| format!("{v:.0}"))),
        opt(e.track.map(|v| format!("{v:.0}"))),
        a.pos.v.0,
        a.pos.v.1,
        opt(e.baro_rate.map(|v| v.to_string())),
        opt(e.squawk.map(|v| format!("{v:04x}"))),
    );
}

/// Per-connection buffers for the output streams, sent once per read.
#[derive(Default)]
struct Out {
    full: Vec<u8>,
    reduced: Vec<u8>,
    reduced_plus: Vec<u8>,
    sbs: Vec<u8>,
    sbs_jaero: Vec<u8>,
    json: Vec<u8>,
    /// the receiver last named in `full` and in `reduced` since they
    /// were sent (--net-receiver-id)
    full_rid: Option<u64>,
    reduced_rid: Option<u64>,
}

/// Name the receiver the frames that follow came from, where the buffer
/// does not already say so. readsb run with --net-receiver-id puts one
/// such marker (0x1a 0xe3 and eight bytes) before each batch of frames it
/// passes on, on its full and its reduced Beast output alike
/// (tools/rid_probe.py); here it is one at the head of each batch and
/// wherever the receiver changes within it.
fn name_receiver(buf: &mut Vec<u8>, last: &mut Option<u64>, rid: u64) {
    if *last != Some(rid) {
        buf.extend_from_slice(&[0x1a, 0xE3]);
        push_escaped(buf, &rid.to_be_bytes());
        *last = Some(rid);
    }
}

impl Out {
    fn flush(&mut self, hub: &Hub) {
        (self.full_rid, self.reduced_rid) = (None, None);
        let send = |tx: &broadcast::Sender<Chunk>, v: &mut Vec<u8>| {
            if !v.is_empty() {
                let _ = tx.send(Arc::new(std::mem::take(v)));
            }
        };
        send(&hub.full, &mut self.full);
        send(&hub.reduced, &mut self.reduced);
        send(&hub.reduced_plus, &mut self.reduced_plus);
        send(&hub.sbs, &mut self.sbs);
        send(&hub.sbs_jaero, &mut self.sbs_jaero);
        send(&hub.json, &mut self.json);
    }
}

fn json_port_line(out: &mut Vec<u8>, ctx: &Ctx, a: &mut Aircraft, now: Ms, has_pos: bool) {
    let interval = if has_pos { ctx.cfg.json_port_interval_ms } else { 10_000 };
    if now.saturating_sub(a.json_port_last) < interval {
        return;
    }
    a.json_port_last = now;
    let mut s = String::with_capacity(700);
    json::aircraft(&mut s, a, &ctx.view(now), None);
    // the line is the aircraft object with "now" in front
    let line = format!("{{\"now\" : {:.3},{}\n", now as f64 / 1000.0, &s[1..]);
    out.extend_from_slice(line.as_bytes());
}

/// What a connection has learned about its receivers since the last
/// merge into the registry.
type Extents = HashMap<u64, crate::receivers::Seen>;

fn note_position(ext: &mut Extents, rid: u64, lat: f64, lon: f64, trusted: bool) {
    let e = ext.entry(rid).or_default();
    e.trusted |= trusted;
    e.lat_min = e.lat_min.min(lat);
    e.lat_max = e.lat_max.max(lat);
    e.lon_min = e.lon_min.min(lon);
    e.lon_max = e.lon_max.max(lon);
    e.positions += 1;
}

/// `rid` alone reported a position for `addr` that contradicts what
/// several receivers agree on.
fn note_blame(ext: &mut Extents, rid: u64, addr: u32) {
    let e = ext.entry(rid).or_default();
    e.bad += 1;
    if e.blamed.len() < 32 && !e.blamed.contains(&addr) {
        e.blamed.push(addr);
    }
}

/// The per-connection state `handle_frame` works with.
struct Link<'a> {
    client: &'a Client,
    out: Out,
    ext: Extents,
    meter: crate::guard::Meter,
    trusted: bool,
    /// receivers whose positions are not used, and the registry epoch
    /// this copy was taken at
    distrusted: Vec<u64>,
    epoch: u64,
}

fn handle_frame(ctx: &Ctx, link: &mut Link<'_>, rid: u64, f: &Frame<'_>, now: Ms) {
    if f.ts == MLAT_MAGIC {
        // a feeder's own MLAT results are not evidence
        return;
    }
    let Link { client, out, ext, meter, trusted, distrusted, .. } = link;
    let (client, trusted) = (*client, *trusted);
    let frame = if f.kind == 0x32 { &f.bytes[..7] } else { &f.bytes[..14] };
    // Where the frame says it is from, and a key for its content. For
    // formats whose parity carries the address that takes the CRC; for
    // the others the address is in the clear.
    let df = frame[0] >> 3;
    let addr = match df {
        11 | 17 | 18 => (frame[1] as u32) << 16 | (frame[2] as u32) << 8 | frame[3] as u32,
        _ => modes::crc_residual(frame),
    };
    let mut key = 0u64;
    for &b in &frame[frame.len() - 7..] {
        key = key << 8 | b as u64;
    }
    key |= (frame[0] as u64) << 56;
    if frame.len() == 14 && !matches!(df, 17 | 18) {
        // the first four bytes vary too (altitude or identity field)
        key ^= u64::from(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]])).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    let mut shard = ctx.store.shard(addr).lock();
    let mut known = false;
    let mut hints = modes::Hints::default();
    let sh = &mut *shard;
    let own_check = matches!(df, 11 | 17 | 18);
    // A reply addressed by parity alone is only as good as the last
    // message that proved the address: readsb takes one up to 15 s after
    // (taken at 14 s, ignored at 15; tools/wind_probe2.py).
    sh.wake(addr, now);
    if let Some(a) = sh.map.get_mut(&addr).filter(|a| own_check || now.saturating_sub(a.checked) < PARITY_AFTER_CHECKED) {
        known = true;
        a.by_trusted = trusted;
        if matches!(df, 20 | 21) {
            // only what ADS-B itself reported can vouch for a Comm-B reading
            hints.alt_baro = a.alt_baro.get(now);
            hints.gs = a.gs.get(now).filter(|_| a.gs.src >= Source::Tisb);
            hints.track = a.track.get(now).filter(|_| a.track.src >= Source::Tisb);
        }
        if let Some(position) = a.duplicate(key, f.signal, now, rid, own_check) {
            // heard again through another receiver: nothing new to work out
            sh.messages += 1;
            client.messages.fetch_add(1, Ordering::Relaxed);
            a.corroborate_suspect(key, rid, now);
            let (who, n) = a.take_blame(now);
            for blamed in &who[..n as usize] {
                note_blame(ext, *blamed, addr);
            }
            if position {
                a.confirm_position(rid, now);
                client.positions.fetch_add(1, Ordering::Relaxed);
                if a.pos.src == Source::Adsb && !a.pos_surface {
                    note_position(ext, rid, a.pos.v.0, a.pos.v.1, trusted);
                }
            }
            if ctx.hub.full.receiver_count() > 0 {
                if ctx.cfg.forward_receiver_id {
                    name_receiver(&mut out.full, &mut out.full_rid, rid);
                }
                beast_encode(&mut out.full, f);
            }
            // The plain BaseStation stream has a line for every copy
            // heard, as readsb's has (31,500 lines a second where the
            // reduced stream had 4,600, forty receivers on 1,400
            // aircraft); the reduced one says each thing once. Only a
            // consumer of the plain stream costs the decoding.
            if !ctx.cfg.sbs_reduce && ctx.hub.sbs.receiver_count() > 0 {
                if let Some(m) = modes::decode_with(frame, |x| x == addr, &hints) {
                    sbs_line(&mut out.sbs, a, &m, if position { Pos::Duplicate } else { Pos::None }, now);
                }
            }
            return;
        }
    }
    let Some(mut m) = modes::decode_with(frame, |x| known && x == addr, &hints) else {
        ctx.stats.bad.fetch_add(1, Ordering::Relaxed);
        // A receiver checks these formats before it forwards them; one
        // that fails here was corrupted on the way or made up.
        if matches!(df, 11 | 17 | 18) || !matches!(df, 0 | 4 | 5 | 16 | 20 | 21) {
            meter.garbage();
            ctx.guard.counters.garbage.fetch_add(1, Ordering::Relaxed);
        }
        return;
    };
    meter.good();
    client.messages.fetch_add(1, Ordering::Relaxed);
    if m.cpr.is_some() && !distrusted.is_empty() && distrusted.contains(&rid) {
        ctx.guard.counters.distrusted_positions.fetch_add(1, Ordering::Relaxed);
        m.cpr = None;
    }
    if m.addr != addr {
        // a non-ICAO address lives under its own key
        drop(shard);
        shard = ctx.store.shard(m.addr).lock();
    }
    shard.messages += 1;
    shard.wake(m.addr, now);
    let a = match shard.map.get_mut(&m.addr) {
        Some(a) => a,
        None => {
            if m.source < Source::ModeSChecked {
                return;
            }
            let full = ctx.store.count.load(Ordering::Relaxed) >= ctx.guard.limits.max_aircraft;
            if full || (!trusted && !meter.may_create()) {
                ctx.guard.counters.aircraft_refused.fetch_add(1, Ordering::Relaxed);
                return;
            }
            ctx.store.count.fetch_add(1, Ordering::Relaxed);
            shard.map.entry(m.addr).or_insert_with(|| Box::new(Aircraft::new(m.addr, now)))
        }
    };
    let rx_ref = match m.cpr {
        Some(c) if c.surface && !a.pos_valid(now) => ctx.reg.reference(rid),
        _ => None,
    };
    // A receiver with no place of its own (a link that names none, one
    // newly connected): the aggregator's --lat/--lon, as readsb reads a
    // surface position against its own, but only for a place a receiver
    // near there could hear.
    let rx_ref = rx_ref.or(match m.cpr {
        Some(c) if c.surface && !a.pos_valid(now) => ctx.cfg.site,
        _ => None,
    });
    if !a.db_checked {
        a.db_checked = true;
        a.db = ctx.db.as_ref().and_then(|d| d.describe(m.addr));
    }
    let (who, n) = a.take_blame(now);
    for blamed in &who[..n as usize] {
        note_blame(ext, *blamed, m.addr);
    }
    a.by_trusted = trusted;
    let r = track::apply(a, &m, f.signal, now, rid, rx_ref, key);
    a.remember(key, now, r.pos != Pos::None);
    match r.pos {
        Pos::New => {
            client.positions.fetch_add(1, Ordering::Relaxed);
            ctx.stats.positions.fetch_add(1, Ordering::Relaxed);
            ctx.stats.pos_by_type[(a.addr_type as usize).min(13)].fetch_add(1, Ordering::Relaxed);
            if let Some((slat, slon)) = ctx.cfg.site {
                ctx.stats.max_dist.fetch_max(track::dist_m(slat, slon, r.lat, r.lon) as u64, Ordering::Relaxed);
            }
            if m.source == Source::Adsb && !a.pos_surface {
                note_position(ext, rid, r.lat, r.lon, trusted);
            }
            crate::trace::add(a, now, ctx.cfg.trace_interval_ms);
            if ctx.hub.json.receiver_count() > 0 {
                json_port_line(&mut out.json, ctx, a, now, true);
            }
        }
        Pos::Duplicate => {
            client.positions.fetch_add(1, Ordering::Relaxed);
            if m.source == Source::Adsb && !a.pos_surface {
                note_position(ext, rid, r.lat, r.lon, trusted);
            }
            crate::trace::again(a, now, ctx.cfg.trace_interval_ms);
        }
        Pos::None => {
            if ctx.cfg.json_port_noposition && ctx.hub.json.receiver_count() > 0 && !a.pos_valid(now) {
                json_port_line(&mut out.json, ctx, a, now, false);
            }
        }
    }
    if ctx.hub.full.receiver_count() > 0 {
        if ctx.cfg.forward_receiver_id {
            name_receiver(&mut out.full, &mut out.full_rid, rid);
        }
        beast_encode(&mut out.full, f);
    }
    let want_r = ctx.hub.reduced.receiver_count() > 0;
    let want_rp = ctx.hub.reduced_plus.receiver_count() > 0;
    if want_r || want_rp {
        let (kind, mult) = reduce_kind(&m);
        if now.saturating_sub(a.reduce_last[kind]) >= ctx.cfg.reduce_interval_ms * mult {
            a.reduce_last[kind] = now;
            if want_r {
                if ctx.cfg.forward_receiver_id {
                    name_receiver(&mut out.reduced, &mut out.reduced_rid, rid);
                }
                beast_encode(&mut out.reduced, f);
            }
            if want_rp {
                out.reduced_plus.extend_from_slice(&[0x1a, 0xE3]);
                push_escaped(&mut out.reduced_plus, &rid.to_be_bytes());
                beast_encode(&mut out.reduced_plus, f);
            }
        }
    }
    if ctx.hub.sbs.receiver_count() > 0 {
        // (an altitude reply from an aircraft whose own ADS-B altitude is
        // current says nothing new: readsb's reduced stream had none)
        let redundant = matches!(m.df, 4 | 20) && a.adsb_pos != 0 && now.saturating_sub(a.adsb_pos) < track::STALE;
        let pass = !ctx.cfg.sbs_reduce || !redundant && {
            let (kind, mult) = reduce_kind(&m);
            let due = now.saturating_sub(a.sbs_last[kind]) >= ctx.cfg.reduce_interval_ms * mult;
            if due {
                a.sbs_last[kind] = now;
            }
            due
        };
        if pass {
            sbs_line(&mut out.sbs, a, &m, r.pos, now);
        }
    }
}

fn merge_extents(ctx: &Ctx, ext: &mut Extents, now: Ms) {
    for (rid, e) in ext.drain() {
        ctx.reg.position_box(rid, e, now);
    }
}

/// Read one Beast connection until it closes. `local`: a connection we
/// opened ourselves, which is not subject to the limits for feeders.
pub async fn beast_ingest(ctx: Arc<Ctx>, mut s: TcpStream, peer: std::net::SocketAddr, local: bool) {
    let slot = match ctx.guard.admit(peer.ip(), now_ms()) {
        Ok(slot) => slot,
        Err(_) if local => return,
        Err(_) => return,
    };
    let trusted = local || slot.trusted;
    let _ = s.set_nodelay(true);
    let client = ctx.reg.connect(peer, now_ms());
    let rate = ctx.guard.limits.max_rate;
    let mut link = Link {
        client: &client,
        out: Out::default(),
        ext: Extents::new(),
        meter: crate::guard::Meter::new(now_ms(), rate),
        trusted,
        distrusted: Vec::new(),
        epoch: u64::MAX,
    };
    let mut buf = vec![0u8; 1 << 16];
    let mut have = 0usize;
    let mut last_merge = now_ms();
    let mut last_ping: Ms = 0;
    let mut rid = client.rid.load(Ordering::Relaxed);
    let mut pending_rid: Option<u64> = None;
    loop {
        let n = match tokio::time::timeout(Duration::from_secs(120), s.read(&mut buf[have..])).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => break,
        };
        client.bytes.fetch_add(n as u64, Ordering::Relaxed);
        ctx.stats.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        have += n;
        let now = now_ms();
        if let crate::guard::Verdict::Ban(ms, why) = link.meter.tick(now, rate, ctx.guard.limits.max_new_aircraft) {
            if !trusted {
                ctx.guard.ban(peer.ip(), now, ms, why);
                let gone = ctx.store.purge_receiver(rid);
                if gone > 0 {
                    eprintln!("aggd: forgot {gone} aircraft only that link reported");
                }
                break;
            }
        }
        if !local && now.saturating_sub(last_ping) >= 15_000 {
            // 0x1a 'P' and our clock in milliseconds, 24 bits
            let mut ping = vec![0x1a, b'P'];
            push_escaped(&mut ping, &((now & 0xFF_FFFF) as u32).to_be_bytes()[1..]);
            let _ = s.try_write(&ping);
            last_ping = now;
        }
        let epoch = ctx.reg.epoch.load(Ordering::Relaxed);
        if epoch != link.epoch {
            link.distrusted = ctx.reg.distrusted(now);
            link.epoch = epoch;
        }
        let mut scratch = [0u8; 32];
        let used = parse(&buf[..have], &mut scratch, |ev| match ev {
            Event::Uuid(u) => {
                if let Ok(u) = std::str::from_utf8(u) {
                    if u.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
                        rid = rid_of(u);
                        client.rid.store(rid, Ordering::Relaxed);
                        *client.uuid.lock() = u.to_string();
                    }
                }
            }
            // only our own links may speak for other receivers
            // (the id stands for every frame up to the next one named:
            // readsb names the receiver once before each batch it
            // passes on, not before each frame)
            Event::ReceiverId(r) => pending_rid = trusted.then_some(r),
            Event::Ping(v) => {
                // the feeder returns our clock value advanced by the time
                // it held it: what is left is the round trip
                let rtt = ((now & 0xFF_FFFF) as u32).wrapping_sub(v) & 0xFF_FFFF;
                if rtt < 60_000 {
                    client.rtt_ms.store(rtt as i64, Ordering::Relaxed);
                }
            }
            Event::Frame(f) => {
                let r = pending_rid.unwrap_or(rid);
                if !trusted && !link.meter.take() {
                    ctx.guard.counters.rate_dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                handle_frame(&ctx, &mut link, r, &f, now);
            }
        });
        buf.copy_within(used..have, 0);
        have -= used;
        if have == buf.len() {
            have = 0; // a message cannot be this long: garbage
        }
        link.out.flush(&ctx.hub);
        if now.saturating_sub(last_merge) >= 1000 {
            merge_extents(&ctx, &mut link.ext, now);
            last_merge = now;
        }
    }
    merge_extents(&ctx, &mut link.ext, now_ms());
    ctx.reg.disconnect(&client);
    drop(slot);
}

fn parse_sbs(line: &str, proto: Proto) -> Option<(u32, External)> {
    let f: Vec<&str> = line.trim_end().split(',').collect();
    if f.len() < 11 || f[0] != "MSG" {
        return None;
    }
    let mut hexs = f[4].trim();
    let mut addr = 0;
    if let Some(h) = hexs.strip_prefix('~') {
        hexs = h;
        addr = modes::NON_ICAO;
    }
    addr |= u32::from_str_radix(hexs, 16).ok()? & 0xFF_FFFF;
    if addr & 0xFF_FFFF == 0 {
        return None;
    }
    let get = |i: usize| f.get(i).map(|s| s.trim()).filter(|s| !s.is_empty());
    // a number in a range that can be true; anything else is not there
    let num = |i: usize, lo: f64, hi: f64| get(i).and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= lo && *v <= hi);
    let (source, addr_type) = match proto {
        Proto::SbsInMlat => (Source::Mlat, AddrType::Mlat),
        Proto::SbsInJaero => (Source::Jaero, AddrType::Jaero),
        Proto::SbsInPrio => (Source::Prio, AddrType::Other),
        _ => (Source::Sbs, AddrType::Other),
    };
    let mut e = External { source, ..Default::default() };
    if let Some(cs) = get(10) {
        let mut c = *b"        ";
        for (o, b) in c.iter_mut().zip(cs.bytes()) {
            *o = b.to_ascii_uppercase();
        }
        if c.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b' ') {
            e.callsign = Some(c);
        }
    }
    e.alt_baro = num(11, -5000.0, 150_000.0).map(|v| v as i32);
    e.gs = num(12, 0.0, 10_000.0).map(|v| v as f32);
    e.track = num(13, 0.0, 360.0).map(|v| v as f32);
    if let (Some(lat), Some(lon)) = (num(14, -90.0, 90.0), num(15, -180.0, 180.0)) {
        if lat != 0.0 || lon != 0.0 {
            e.pos = Some((lat, lon));
            e.addr_type = Some(addr_type);
        }
    }
    e.baro_rate = num(16, -100_000.0, 100_000.0).map(|v| v as i32);
    e.squawk = get(17).and_then(|s| u16::from_str_radix(s, 16).ok());
    e.alert = get(18).map(|s| s != "0");
    e.spi = get(20).map(|s| s != "0");
    e.ground = get(21).map(|s| s != "0");
    // ADS-C reports arrive late; the line says when the position was true
    if proto == Proto::SbsInJaero {
        e.time = sbs_parse_time(f[6], f[7]);
    }
    Some((addr, e))
}

/// One line of dump978 raw output: a UAT ADS-B message.
fn uat_line(ctx: &Ctx, line: &str, out: &mut Out) {
    let Some(u) = modes::uat::parse_line(line) else { return };
    let now = now_ms();
    // only ICAO-qualified addresses (own or TIS-B) are aircraft addresses
    let addr = if matches!(u.qualifier, 0 | 2) { u.addr } else { u.addr | modes::NON_ICAO };
    ctx.stats.sbs.fetch_add(1, Ordering::Relaxed);
    let mut shard = ctx.store.shard(addr).lock();
    shard.messages += 1;
    shard.wake(addr, now);
    if !shard.map.contains_key(&addr) {
        if ctx.store.count.load(Ordering::Relaxed) >= ctx.guard.limits.max_aircraft {
            return;
        }
        ctx.store.count.fetch_add(1, Ordering::Relaxed);
    }
    let a = shard.map.entry(addr).or_insert_with(|| Box::new(Aircraft::new(addr, now)));
    if !a.db_checked {
        a.db_checked = true;
        a.db = ctx.db.as_ref().and_then(|d| d.describe(addr));
    }
    if track::apply_uat(a, &u, now) == Pos::New {
        ctx.stats.positions.fetch_add(1, Ordering::Relaxed);
        ctx.stats.pos_by_type[(a.addr_type as usize).min(13)].fetch_add(1, Ordering::Relaxed);
        crate::trace::add(a, now, ctx.cfg.trace_interval_ms);
        if ctx.hub.json.receiver_count() > 0 {
            json_port_line(&mut out.json, ctx, a, now, true);
        }
    }
}

/// Read BaseStation lines: solved positions from MLAT, JAERO or another feed.
pub async fn sbs_ingest(ctx: Arc<Ctx>, s: TcpStream, proto: Proto, local: bool) {
    let peer = s.peer_addr().map(|p| p.ip()).unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
    let _slot = match ctx.guard.admit(peer, now_ms()) {
        Ok(slot) => Some(slot),
        Err(_) if local => None,
        Err(_) => return,
    };
    let mut reader = BufReader::with_capacity(1 << 15, s);
    let mut out = Out::default();
    let mut raw = Vec::with_capacity(256);
    loop {
        raw.clear();
        // a line is short; a sender of endless ones is cut off
        let mut limited = (&mut reader).take(1024);
        let line = match tokio::time::timeout(Duration::from_secs(600), limited.read_until(b'\n', &mut raw)).await {
            Ok(Ok(n)) if n > 0 && raw.ends_with(b"\n") => String::from_utf8_lossy(&raw).into_owned(),
            _ => return,
        };
        ctx.stats.bytes_in.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
        if proto == Proto::UatIn {
            uat_line(&ctx, &line, &mut out);
            out.flush(&ctx.hub);
            continue;
        }
        let Some((addr, e)) = parse_sbs(&line, proto) else { continue };
        let wall = now_ms();
        // a stated time counts when it is in the recent past
        let now = match e.time {
            Some(t) if t <= wall && wall - t < track::JAERO_TTL.load(Ordering::Relaxed) => t,
            _ => wall,
        };
        ctx.stats.sbs.fetch_add(1, Ordering::Relaxed);
        {
            let mut shard = ctx.store.shard(addr).lock();
            shard.messages += 1;
            shard.wake(addr, now);
            let a = shard.map.entry(addr).or_insert_with(|| Box::new(Aircraft::new(addr, now)));
            if !a.db_checked {
                a.db_checked = true;
                a.db = ctx.db.as_ref().and_then(|d| d.describe(addr));
            }
            if a.addr_type == AddrType::Unknown {
                a.addr_type = e.addr_type.unwrap_or(AddrType::Other);
                a.addr_type_t = wall;
            }
            if track::apply_external(a, &e, now, wall) == Pos::New {
                ctx.stats.positions.fetch_add(1, Ordering::Relaxed);
                ctx.stats.pos_by_type[(a.addr_type as usize).min(13)].fetch_add(1, Ordering::Relaxed);
                let want_rp = ctx.hub.reduced_plus.receiver_count() > 0;
                if proto == Proto::SbsInMlat && ctx.cfg.forward_mlat && (ctx.hub.full.receiver_count() > 0 || ctx.hub.reduced.receiver_count() > 0 || want_rp) {
                    // the result as extended squitters, stamped as MLAT
                    let frames = modes::encode::synthetic_position(addr & 0xFF_FFFF, a.pos.v.0, a.pos.v.1, e.alt_baro, e.gs.zip(e.track), e.baro_rate);
                    let mut bytes = Vec::with_capacity(72);
                    for f in &frames {
                        beast_encode(&mut bytes, &Frame { kind: 0x33, ts: &MLAT_MAGIC, signal: 0, bytes: f });
                    }
                    if ctx.cfg.forward_receiver_id {
                        // from no receiver: the solver's
                        name_receiver(&mut out.full, &mut out.full_rid, 0);
                        name_receiver(&mut out.reduced, &mut out.reduced_rid, 0);
                    }
                    out.full.extend_from_slice(&bytes);
                    out.reduced.extend_from_slice(&bytes);
                    if want_rp {
                        // from no receiver: the solver's, not the last one named
                        for f in &frames {
                            out.reduced_plus.extend_from_slice(&[0x1a, 0xE3]);
                            push_escaped(&mut out.reduced_plus, &0u64.to_be_bytes());
                            beast_encode(&mut out.reduced_plus, &Frame { kind: 0x33, ts: &MLAT_MAGIC, signal: 0, bytes: f });
                        }
                    }
                }
                let jaero = proto == Proto::SbsInJaero;
                let to_main = ctx.hub.sbs.receiver_count() > 0 && (proto != Proto::SbsInMlat || ctx.cfg.forward_mlat_sbs) && !jaero;
                let to_jaero = jaero && ctx.hub.sbs_jaero.receiver_count() > 0;
                if to_main || to_jaero {
                    let dst = if to_jaero { &mut out.sbs_jaero } else { &mut out.sbs };
                    sbs_external(dst, a, &e, wall);
                }
                crate::trace::add(a, now, ctx.cfg.trace_interval_ms);
                if ctx.hub.json.receiver_count() > 0 {
                    json_port_line(&mut out.json, &ctx, a, now, true);
                }
            }
        }
        out.flush(&ctx.hub);
    }
}

/// Feed a subscriber from a stream until it closes.
///
/// The stream arrives as many small batches (one per read of each
/// feeder's connection, thousands a second on a busy network). Written
/// one by one, each its own packet, a consumer could not keep up with
/// the full stream however fast its line, fell a whole backlog behind
/// and was cut off again and again. So everything that is waiting goes
/// out in one write. A consumer that is too slow all the same loses the
/// oldest batches (whole frames, so the stream stays well-formed) and
/// carries on from the oldest kept: it stays connected, a bounded time
/// behind, and costs no memory of its own.
async fn pump(ctx: Arc<Ctx>, mut s: TcpStream, mut rx: broadcast::Receiver<Chunk>, hello: Vec<u8>, heartbeat: &'static [u8]) {
    use broadcast::error::{RecvError, TryRecvError};
    const WRITE_MAX: usize = 256 * 1024;
    let _ = s.set_nodelay(true);
    if !hello.is_empty() && s.write_all(&hello).await.is_err() {
        return;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Ok(chunk)) => {
                buf.clear();
                buf.extend_from_slice(&chunk);
                while buf.len() < WRITE_MAX {
                    match rx.try_recv() {
                        Ok(c) => buf.extend_from_slice(&c),
                        Err(TryRecvError::Lagged(n)) => {
                            ctx.stats.out_skipped.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(_) => break,
                    }
                }
                // a consumer that takes nothing at all for this long is gone
                match tokio::time::timeout(Duration::from_secs(20), s.write_all(&buf)).await {
                    Ok(Ok(())) => {
                        ctx.stats.bytes_out.fetch_add(buf.len() as u64, Ordering::Relaxed);
                    }
                    _ => return,
                }
                if buf.capacity() > 2 * WRITE_MAX {
                    buf = Vec::with_capacity(16 * 1024);
                }
            }
            Ok(Err(RecvError::Lagged(n))) => {
                ctx.stats.out_skipped.fetch_add(n, Ordering::Relaxed);
            }
            Ok(Err(RecvError::Closed)) => return,
            Err(_) => {
                if !heartbeat.is_empty() && s.write_all(heartbeat).await.is_err() {
                    return;
                }
            }
        }
    }
}

const HEARTBEAT: &[u8] = &[0x1a, 0x31, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// `uuid`: the one given for this connection, over the general one.
fn subscribe(ctx: &Ctx, proto: Proto, uuid: Option<&str>) -> (broadcast::Receiver<Chunk>, Vec<u8>, &'static [u8]) {
    match proto {
        Proto::BeastOut => (ctx.hub.full.subscribe(), Vec::new(), HEARTBEAT),
        Proto::BeastReduceOut => (ctx.hub.reduced.subscribe(), Vec::new(), HEARTBEAT),
        Proto::BeastReducePlusOut => {
            let mut hello = Vec::new();
            if let Some(u) = uuid.or(ctx.cfg.uuid.as_deref()) {
                let mut padded = [b'f'; 36];
                for (o, b) in padded.iter_mut().zip(u.trim().bytes()) {
                    *o = b;
                }
                hello.extend_from_slice(&[0x1a, 0xE4]);
                hello.extend_from_slice(&padded);
                hello.extend_from_slice(&[0x1a, b'W', b'O']);
            }
            let rx = if ctx.cfg.forward_receiver_id { ctx.hub.reduced_plus.subscribe() } else { ctx.hub.reduced.subscribe() };
            (rx, hello, HEARTBEAT)
        }
        Proto::SbsOut => (ctx.hub.sbs.subscribe(), Vec::new(), b""),
        Proto::SbsJaeroOut => (ctx.hub.sbs_jaero.subscribe(), Vec::new(), b""),
        _ => (ctx.hub.json.subscribe(), Vec::new(), b""),
    }
}

/// Accept connections on a port and treat each as `proto`.
pub async fn listen(ctx: Arc<Ctx>, addr: String, proto: Proto) -> anyhow::Result<()> {
    let l = TcpListener::bind(&addr).await.map_err(|e| anyhow::anyhow!("bind {addr} ({proto:?}): {e}"))?;
    eprintln!("aggd: {proto:?} on {addr}");
    tokio::spawn(async move {
        loop {
            let Ok((s, peer)) = l.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                match proto {
                    Proto::BeastIn => beast_ingest(ctx, s, peer, false).await,
                    p if p.is_input() => sbs_ingest(ctx, s, p, false).await,
                    p => {
                        let (rx, hello, hb) = subscribe(&ctx, p, None);
                        pump(ctx.clone(), s, rx, hello, hb).await
                    }
                }
            });
        }
    });
    Ok(())
}

/// Keep an outbound connection up, reconnecting with backoff.
pub fn connect(ctx: Arc<Ctx>, host: String, port: u16, proto: Proto, uuid: Option<String>) {
    tokio::spawn(async move {
        let mut delay = 1u64;
        loop {
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(s) => {
                    eprintln!("aggd: connected {host}:{port} ({proto:?})");
                    delay = 1;
                    let peer = s.peer_addr().unwrap_or_else(|_| ([0, 0, 0, 0], 0).into());
                    match proto {
                        Proto::BeastIn => beast_ingest(ctx.clone(), s, peer, true).await,
                        p if p.is_input() => sbs_ingest(ctx.clone(), s, p, true).await,
                        p => {
                            let (rx, hello, hb) = subscribe(&ctx, p, uuid.as_deref());
                            pump(ctx.clone(), s, rx, hello, hb).await
                        }
                    }
                }
                Err(e) => {
                    if delay == 1 {
                        eprintln!("aggd: cannot connect to {host}:{port} ({proto:?}): {e}; retrying");
                    }
                    delay = (delay * 2).min(30)
                }
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beast_stream_parses_across_escapes_and_splits() {
        let f = modes::encode::airborne_position(0x1a1a1a, 45.0, 9.0, 30000, false, 11);
        let mut wire = vec![0x1a, 0xE4];
        wire.extend_from_slice(b"01234567-89ab-cdef-0123-456789abcdef");
        wire.extend_from_slice(&[0x1a, b'W', b'O']);
        let fr = Frame { kind: 0x33, ts: &[0x1a, 0, 0, 0, 0, 0x1a], signal: 0x1a, bytes: &f };
        beast_encode(&mut wire, &fr);
        wire.extend_from_slice(&[0x1a, b'P', 0x1a, 0x1a, 0x02, 0x03]);
        wire.extend_from_slice(HEARTBEAT);
        beast_encode(&mut wire, &fr);
        // fed whole, and fed one byte at a time, the same events come out
        for step in [wire.len(), 1, 7] {
            let (mut frames, mut uuids) = (0, 0);
            let mut buf: Vec<u8> = Vec::new();
            for chunk in wire.chunks(step) {
                buf.extend_from_slice(chunk);
                let mut scratch = [0u8; 32];
                let used = parse(&buf, &mut scratch, |e| match e {
                    Event::Frame(x) => {
                        assert_eq!(x.bytes, &f[..]);
                        assert_eq!(x.signal, 0x1a);
                        frames += 1;
                    }
                    Event::Uuid(_) => uuids += 1,
                    Event::Ping(v) => assert_eq!(v, 0x1a0203),
                    _ => {}
                });
                buf.drain(..used);
            }
            assert_eq!((frames, uuids), (2, 1), "step {step}");
        }
    }

    #[test]
    fn sbs_time_is_utc() {
        assert_eq!(sbs_time(1_794_829_573_267), "2026/11/16,11:46:13.267");
        assert_eq!(sbs_parse_time("2026/11/16", "11:46:13.267"), Some(1_794_829_573_267));
        assert_eq!(sbs_parse_time("2024/02/29", "00:00:00.000"), Some(1_709_164_800_000));
        assert_eq!(sbs_parse_time("garbage", "11:46:13"), None);
    }

    #[test]
    fn sbs_numbers_must_be_possible() {
        let line = "MSG,3,1,1,4CA2D6,1,2026/11/16,11:46:13.267,2026/11/16,11:46:13.267,,1e400,inf,nan,91.5,181.5,-inf,,0,,0,0";
        let (_, e) = parse_sbs(line, Proto::SbsIn).unwrap();
        assert!(e.alt_baro.is_none() && e.gs.is_none() && e.track.is_none() && e.pos.is_none() && e.baro_rate.is_none());
        assert!(parse_sbs("MSG,3,1,1,ZZZZZZ,1,,,,,,,,,,,,,,,,", Proto::SbsIn).is_none());
        assert!(parse_sbs("MSG,3", Proto::SbsIn).is_none());
    }

    #[test]
    fn sbs_position_line() {
        let (addr, e) = parse_sbs("MSG,3,1,1,4CA2D6,1,2026/11/16,11:46:13.267,2026/11/16,11:46:13.267,,37000,,,51.45735,-1.02826,,,0,,0,0", Proto::SbsInMlat).unwrap();
        assert_eq!(addr, 0x4CA2D6);
        assert_eq!(e.alt_baro, Some(37000));
        assert_eq!(e.pos, Some((51.45735, -1.02826)));
        assert_eq!(e.source, Source::Mlat);
    }
}
