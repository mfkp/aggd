//! Per-aircraft position history, kept compact in memory and rendered
//! to tar1090's trace JSON on request.

use crate::track::{Aircraft, Ms};
use modes::AddrType;

#[derive(Clone, Copy)]
pub struct Point {
    /// milliseconds since the Unix epoch
    pub t: Ms,
    /// degrees * 1e6
    pub lat: i32,
    pub lon: i32,
    /// feet; i32::MIN unknown, i32::MIN+1 on the ground
    pub alt: i32,
    pub alt_geom: i32,
    /// knots * 10; u16::MAX unknown
    pub gs: u16,
    /// degrees * 10; u16::MAX unknown
    pub track: u16,
    /// ft/min; i16::MIN unknown
    pub rate: i16,
    pub geom_rate: i16,
    pub ias: u16,
    pub flags: u8,
    pub addr_type: u8,
}

pub const NONE: i32 = i32::MIN;
pub const GROUND: i32 = i32::MIN + 1;
pub const FLAG_STALE: u8 = 1;
pub const FLAG_LEG: u8 = 2;
pub const FLAG_RATE_GEOM: u8 = 4;
pub const FLAG_ALT_GEOM: u8 = 8;

/// What else was known about the aircraft at a point: kept for one
/// point in four, as readsb has it (of 264,861 points in its trace
/// files 25.3 % carried the object, 64,270 times in 65,001 three
/// points after the last), so a history view can say who it was and
/// what it was doing. Absent values are the type's maximum (or -1).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Detail {
    /// time of the point it belongs to
    pub t: Ms,
    pub callsign: [u8; 8],
    pub squawk: u16,
    pub category: u8,
    pub emergency: u8,
    pub nav_alt_mcp: u32,
    pub nav_qnh: f32,
    pub nav_heading: f32,
    pub nav_modes: u8,
    pub nic: u8,
    pub rc: u16,
    pub version: i8,
    pub nic_baro: i8,
    pub nac_p: i8,
    pub nac_v: i8,
    pub sil: i8,
    pub sil_type: u8,
    pub gva: i8,
    pub sda: i8,
    pub alert: i8,
    pub spi: i8,
    /// explicit padding, always zero, so the struct has no hidden bytes
    pub pad1: [u8; 2],
    pub track: f32,
    pub tas: u16,
    pub pad2: [u8; 2],
    pub mach: f32,
    pub roll: f32,
    pub mag_heading: f32,
    pub true_heading: f32,
}

/// A point as it is kept in memory: 24 bytes. Time is in tenths of a
/// second, altitude in 25 ft steps, vertical rates in 64 ft/min steps
/// (what the transponder sends), airspeed in 2 kt steps.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Packed {
    t: u32,
    lat: i32,
    lon: i32,
    alt: i16,
    gs: u16,
    track: u16,
    geom_delta: i8,
    rate: i8,
    geom_rate: i8,
    ias: u8,
    flags: u8,
    addr_type: u8,
}

/// 2020-01-01T00:00:00Z, the origin of packed times.
const T0: Ms = 1_577_836_800_000;

impl Packed {
    #[inline]
    pub fn t(&self) -> Ms {
        T0 + self.t as u64 * 100
    }

    pub fn pack(p: &Point) -> Packed {
        let mut flags = p.flags & !FLAG_ALT_GEOM;
        // without a barometric altitude the geometric one takes its place
        let (alt_ft, geom_delta) = match (p.alt, p.alt_geom) {
            (NONE, NONE) => (NONE, i8::MIN),
            (NONE, g) => {
                flags |= FLAG_ALT_GEOM;
                (g, i8::MIN)
            }
            (GROUND, _) => (GROUND, i8::MIN),
            (a, NONE) => (a, i8::MIN),
            (a, g) => (a, ((g - a) / 25).clamp(-127, 127) as i8),
        };
        let alt = match alt_ft {
            NONE => i16::MIN,
            GROUND => i16::MIN + 1,
            a => (a / 25).clamp(i16::MIN as i32 + 2, i16::MAX as i32) as i16,
        };
        let rate8 = |r: i16| if r == i16::MIN { i8::MIN } else { (r / 64).clamp(-127, 127) as i8 };
        Packed {
            t: (p.t.saturating_sub(T0) / 100) as u32,
            lat: p.lat,
            lon: p.lon,
            alt,
            gs: p.gs,
            track: if p.track == u16::MAX { u16::MAX } else { p.track.min(3600) },
            geom_delta,
            rate: rate8(p.rate),
            geom_rate: rate8(p.geom_rate),
            ias: if p.ias == u16::MAX { u8::MAX } else { (p.ias / 2).min(254) as u8 },
            flags,
            addr_type: p.addr_type,
        }
    }

    pub fn unpack(&self) -> Point {
        let alt = match self.alt {
            i16::MIN => NONE,
            v if v == i16::MIN + 1 => GROUND,
            v => v as i32 * 25,
        };
        let geom_alt = self.flags & FLAG_ALT_GEOM != 0;
        let rate16 = |r: i8| if r == i8::MIN { i16::MIN } else { r as i16 * 64 };
        Point {
            t: self.t(),
            lat: self.lat,
            lon: self.lon,
            alt: if geom_alt { NONE } else { alt },
            alt_geom: if geom_alt {
                alt
            } else if self.geom_delta == i8::MIN || alt <= GROUND {
                NONE
            } else {
                alt + self.geom_delta as i32 * 25
            },
            gs: self.gs,
            track: self.track,
            rate: rate16(self.rate),
            geom_rate: rate16(self.geom_rate),
            ias: if self.ias == u8::MAX { u16::MAX } else { self.ias as u16 * 2 },
            flags: self.flags & !FLAG_ALT_GEOM,
            addr_type: self.addr_type,
        }
    }
}

/// A trace is its older part, compressed in chunks, followed by its
/// newest points as they are. An aircraft in flight keeps a few dozen
/// points open and packs the rest away as it goes; one that has gone
/// silent keeps none open. A chunk once written is never rewritten, only
/// dropped whole when it has aged out: the table is then made of buffers
/// that live a day, with no reshuffling to leave holes in the heap.
#[derive(Default)]
pub struct Trace {
    /// the newest points, after those in `cold`
    pub points: Vec<Packed>,
    /// the details belonging to `points` (and possibly a few older)
    pub details: Vec<Detail>,
    /// points before this index, counted over the whole trace, are
    /// already in a history file
    pub written: usize,
    /// oldest first
    cold: Vec<Cold>,
    /// the last point and detail packed away, for when nothing is open
    tail: Option<Box<(Packed, Option<Detail>)>>,
    /// what is in its trace files on disk (--write-traces); absent until
    /// they are first written
    pub files: Option<Box<Files>>,
    /// The day file this process last wrote for the trace: its day
    /// (days since 1970; 0: none yet), the time of the first point the
    /// trace had for it, and whether the file held points the trace
    /// did not. See `persist::write_history`.
    pub file_day: u32,
    pub file_more: bool,
    pub file_first: Ms,
}

/// The newest point in each of an aircraft's trace files on disk, and
/// the length of the trace when trace_full was written.
#[derive(Clone, Copy, Default)]
pub struct Files {
    pub recent: Ms,
    pub full: Ms,
    pub full_len: usize,
}

/// The aircraft a trace file is for, and whether it is trace_recent:
/// trace_(full|recent)_<hex>.json, `~` before the hex of an address
/// that is not an ICAO one.
pub fn file_addr(name: &str) -> Option<(bool, u32)> {
    let name = name.strip_suffix(".json")?;
    let (recent, hexs) = match name.strip_prefix("trace_recent_") {
        Some(h) => (true, h),
        None => (false, name.strip_prefix("trace_full_")?),
    };
    let (hexs, flag) = match hexs.strip_prefix('~') {
        Some(h) => (h, modes::NON_ICAO),
        None => (hexs, 0),
    };
    Some((recent, u32::from_str_radix(hexs, 16).ok()? | flag))
}

struct Cold {
    z: Box<[u8]>,
    points: u32,
    details: u32,
    /// times of the first and last points
    first: Ms,
    last: Ms,
}

pub const PACKED: usize = std::mem::size_of::<Packed>();
pub const DETAIL: usize = std::mem::size_of::<Detail>();
// neither struct may have hidden padding: their bytes are read as they are
const _: () = assert!(PACKED == 24 && DETAIL == 72 && std::mem::align_of::<Detail>() == 8);

/// Open points at which the older ones are packed away, and how many stay.
const OPEN_MAX: usize = 128;
const OPEN_KEEP: usize = 32;

/// How hard zstd works on a chunk. A chunk is packed once and held for a
/// day, so the time is well spent: on a day of real traces (2.5 million
/// points in chunks of 96 at most) level 3 took 13 µs a chunk for 11.21
/// bytes a point, 7: 28 µs for 10.94, 13: 88 µs for 10.68, 19: 460 µs for
/// 10.59. At a thousand points a second that is a thousandth of a core.
const LEVEL: i32 = 13;
/// Where many chunks are packed at once and time counts: saving the open
/// points when stopping, reading the state file of an earlier version.
const LEVEL_FAST: i32 = 3;

/// The bytes of `v`. Sound for `Packed` and `Detail` only: plain numbers,
/// `repr(C)`, no padding.
fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn from_bytes<T: Copy>(b: &[u8]) -> Vec<T> {
    let n = b.len() / std::mem::size_of::<T>();
    let mut v = Vec::<T>::with_capacity(n);
    unsafe {
        std::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr() as *mut u8, n * std::mem::size_of::<T>());
        v.set_len(n);
    }
    v
}

/// Successive points of a flight are nearly alike: each is stored as its
/// difference from the one before, which is what lets them compress.
/// (Storing time and place as the change in that difference was tried on
/// a day of real traces and gained nothing: points are not evenly spaced.)
fn delta(points: &[Packed]) -> Vec<Packed> {
    let mut prev = Packed::default();
    points
        .iter()
        .map(|p| {
            let d = Packed {
                t: p.t.wrapping_sub(prev.t),
                lat: p.lat.wrapping_sub(prev.lat),
                lon: p.lon.wrapping_sub(prev.lon),
                alt: p.alt.wrapping_sub(prev.alt),
                gs: p.gs.wrapping_sub(prev.gs),
                track: p.track.wrapping_sub(prev.track),
                ..*p
            };
            prev = *p;
            d
        })
        .collect()
}

fn undelta(points: &mut [Packed]) {
    let mut prev = Packed::default();
    for p in points {
        p.t = p.t.wrapping_add(prev.t);
        p.lat = p.lat.wrapping_add(prev.lat);
        p.lon = p.lon.wrapping_add(prev.lon);
        p.alt = p.alt.wrapping_add(prev.alt);
        p.gs = p.gs.wrapping_add(prev.gs);
        p.track = p.track.wrapping_add(prev.track);
        prev = *p;
    }
}

/// Lay records of `stride` bytes out by column; with `diff`, each byte
/// as its difference from the same byte of the record before.
fn columns(b: &[u8], stride: usize, diff: bool, out: &mut Vec<u8>) {
    let n = b.len() / stride;
    for c in 0..stride {
        let mut prev = 0u8;
        for r in 0..n {
            let v = b[r * stride + c];
            out.push(v.wrapping_sub(prev));
            if diff {
                prev = v;
            }
        }
    }
}

fn rows(cols: &[u8], stride: usize, diff: bool) -> Vec<u8> {
    let n = cols.len() / stride;
    let mut out = vec![0u8; n * stride];
    for c in 0..stride {
        let mut prev = 0u8;
        for r in 0..n {
            let v = prev.wrapping_add(cols[c * n + r]);
            out[r * stride + c] = v;
            if diff {
                prev = v;
            }
        }
    }
    out
}

impl Cold {
    fn pack(points: &[Packed], details: &[Detail], level: i32) -> Option<Cold> {
        let last = points.last()?.t();
        let mut raw = Vec::with_capacity(points.len() * PACKED + details.len() * DETAIL);
        columns(bytes_of(&delta(points)), PACKED, false, &mut raw);
        columns(bytes_of(details), DETAIL, true, &mut raw);
        let z = zstd::bulk::compress(&raw, level).ok()?;
        Some(Cold { z: z.into_boxed_slice(), points: points.len() as u32, details: details.len() as u32, first: points[0].t(), last })
    }

    /// (our own bytes, so this cannot fail short of memory corruption;
    /// if it did the chunk would read as empty)
    fn unpack(&self, p: &mut Vec<Packed>, d: &mut Vec<Detail>) {
        let (np, nd) = (self.points as usize * PACKED, self.details as usize * DETAIL);
        let Some(raw) = zstd::bulk::decompress(&self.z, np + nd).ok().filter(|r| r.len() == np + nd) else { return };
        let mut points: Vec<Packed> = from_bytes(&rows(&raw[..np], PACKED, false));
        undelta(&mut points);
        p.append(&mut points);
        d.append(&mut from_bytes(&rows(&raw[np..], DETAIL, true)));
    }
}

impl Trace {
    /// A trace of `points`, all packed away.
    pub fn restored(points: &[Packed], written: usize) -> Trace {
        let mut cold: Vec<Cold> = points.chunks(OPEN_MAX - OPEN_KEEP).filter_map(|c| Cold::pack(c, &[], LEVEL_FAST)).collect();
        cold.shrink_to_fit();
        Trace { written, cold, tail: points.last().map(|p| Box::new((*p, None))), ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.cold.iter().map(|c| c.points as usize).sum::<usize>() + self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cold.is_empty() && self.points.is_empty()
    }

    /// Time of the oldest point.
    pub fn first_t(&self) -> Option<Ms> {
        self.cold.first().map(|c| c.first).or_else(|| self.points.first().map(|p| p.t()))
    }

    pub fn last_point(&self) -> Option<Packed> {
        self.points.last().copied().or_else(|| self.tail.as_ref().map(|t| t.0))
    }

    pub fn last_detail(&self) -> Option<Detail> {
        self.details.last().copied().or_else(|| self.tail.as_ref().and_then(|t| t.1))
    }

    /// Bytes of memory the trace holds.
    pub fn bytes(&self) -> usize {
        self.points.capacity() * PACKED
            + self.details.capacity() * DETAIL
            + self.cold.capacity() * std::mem::size_of::<Cold>()
            + self.cold.iter().map(|c| c.z.len()).sum::<usize>()
            + self.tail.as_ref().map_or(0, |t| std::mem::size_of_val(&**t))
    }

    /// The whole trace, without changing it.
    pub fn all(&self) -> (std::borrow::Cow<'_, [Packed]>, std::borrow::Cow<'_, [Detail]>) {
        use std::borrow::Cow;
        if self.cold.is_empty() {
            return (Cow::Borrowed(&self.points), Cow::Borrowed(&self.details));
        }
        let (mut p, mut d) = (Vec::with_capacity(self.len()), Vec::new());
        for c in &self.cold {
            c.unpack(&mut p, &mut d);
        }
        p.extend_from_slice(&self.points);
        d.extend_from_slice(&self.details);
        (Cow::Owned(p), Cow::Owned(d))
    }

    /// The newest `n` points (all of them in a shorter trace) and the
    /// details that go with them, unpacking only the chunks they are in:
    /// a recent trace is asked for far more often than a day of one.
    pub fn last(&self, n: usize) -> (std::borrow::Cow<'_, [Packed]>, std::borrow::Cow<'_, [Detail]>) {
        use std::borrow::Cow;
        let open = self.points.len();
        if open >= n || self.cold.is_empty() {
            return (Cow::Borrowed(&self.points[open.saturating_sub(n)..]), Cow::Borrowed(&self.details));
        }
        let (mut first, mut have) = (self.cold.len(), open);
        while first > 0 && have < n {
            first -= 1;
            have += self.cold[first].points as usize;
        }
        let (mut p, mut d) = (Vec::with_capacity(have), Vec::new());
        for c in &self.cold[first..] {
            c.unpack(&mut p, &mut d);
        }
        p.extend_from_slice(&self.points);
        d.extend_from_slice(&self.details);
        p.drain(..p.len().saturating_sub(n));
        (Cow::Owned(p), Cow::Owned(d))
    }

    /// Put `more` among the open points, each at its time. A point the
    /// trace already has at that time stays as it is; one from before
    /// the open points begin is left out.
    fn merge(&mut self, more: &[Packed]) {
        let floor = self.cold.last().map_or(0, |c| c.last);
        let had = self.points.len();
        self.points.extend(more.iter().filter(|p| p.t() > floor));
        if self.points.len() > had {
            // (stable: of two at one time the trace's own comes first)
            self.points.sort_by_key(|p| p.t);
            self.points.dedup_by_key(|p| p.t);
            self.tidy();
        }
    }

    /// Pack away all but the newest `keep` points, as one more chunk.
    pub fn compact(&mut self, keep: usize) {
        if self.points.len() <= keep {
            return;
        }
        let n = self.points.len() - keep;
        // details go with their points
        let nd = match self.points.get(n) {
            Some(first_open) => self.details.partition_point(|x| x.t < first_open.t()),
            None => self.details.len(),
        };
        let Some(cold) = Cold::pack(&self.points[..n], &self.details[..nd], LEVEL) else { return };
        let last_detail = self.details[..nd].last().copied().or_else(|| self.tail.as_ref().and_then(|t| t.1));
        let tail = (self.points[n - 1], last_detail);
        match &mut self.tail {
            Some(t) => **t = tail,
            None => self.tail = Some(Box::new(tail)),
        }
        self.cold.push(cold);
        if keep == 0 {
            self.cold.shrink_to_fit();
            self.points = Vec::new();
            self.details = Vec::new();
        } else {
            // (in place: the open buffers keep their size while in flight)
            self.points.drain(..n);
            self.details.drain(..nd);
        }
    }

    /// Pack away the older points of a trace that has grown long.
    pub fn tidy(&mut self) {
        if self.points.len() >= OPEN_MAX {
            self.compact(OPEN_KEEP);
        }
    }

    /// Drop what is older than `cut`: whole chunks, so up to one chunk of
    /// older points may stay a while longer. Returns how many points went.
    pub fn trim_before(&mut self, cut: Ms) -> usize {
        let chunks = self.cold.iter().take_while(|c| c.last < cut).count();
        let mut n: usize = self.cold.drain(..chunks).map(|c| c.points as usize).sum();
        if self.cold.is_empty() {
            let open = self.points.partition_point(|p| p.t() < cut);
            if open > 0 {
                let first = self.points.get(open).map_or(Ms::MAX, |p| p.t());
                let nd = self.details.partition_point(|d| d.t < first);
                self.details.drain(..nd);
                self.points.drain(..open);
                n += open;
            }
            if self.points.is_empty() {
                self.tail = None;
            }
        }
        self.written = self.written.saturating_sub(n);
        n
    }
}

/// A trace in the state file is its chunks as they are held in memory,
/// so saving compresses nothing again and restoring unpacks nothing:
///
/// ```text
/// u32 newest points not yet in a history file
/// u32 chunks    u8 1 if the open points follow them as one more chunk
/// per chunk: u32 points, u32 details, u64 first, u64 last (ms), u32 bytes, the bytes
/// u8 what follows: 1 the last point (24 bytes), 2 the last detail too (72 bytes)
/// ```
///
/// all little-endian. A chunk's bytes are `Cold::pack`'s: whoever changes
/// that, `Packed` or `Detail` gives the state file a new magic.
pub struct Saved {
    pub trace: Trace,
    /// its last chunk was the points open when it was saved
    pub open: bool,
    /// newest points not yet in a history file
    pub behind: usize,
    /// time of the last point in a chunk packed for good (0: none), the
    /// open points apart
    pub until: Ms,
}

impl Trace {
    /// Time of the last point packed away for good; 0 if none is.
    pub fn packed_until(&self) -> Ms {
        self.cold.last().map_or(0, |c| c.last)
    }

    /// The newest points not yet in a history file. (Counted from the
    /// end, the number holds when old points are dropped from the front.)
    pub fn behind(&self) -> usize {
        self.len().saturating_sub(self.written)
    }

    /// Append to `out` what the trace holds after `after`: the chunks
    /// packed since, and the open points as one more; all of it with 0.
    /// The trace itself stays as it is.
    pub fn save(&self, after: Ms, out: &mut Vec<u8>) {
        let open = Cold::pack(&self.points, &self.details, LEVEL_FAST);
        let cold = &self.cold[self.cold.partition_point(|c| c.last <= after)..];
        out.extend_from_slice(&(self.behind() as u32).to_le_bytes());
        out.extend_from_slice(&(cold.len() as u32).to_le_bytes());
        out.push(open.is_some() as u8);
        for c in cold.iter().chain(open.iter()) {
            out.extend_from_slice(&c.points.to_le_bytes());
            out.extend_from_slice(&c.details.to_le_bytes());
            out.extend_from_slice(&c.first.to_le_bytes());
            out.extend_from_slice(&c.last.to_le_bytes());
            out.extend_from_slice(&(c.z.len() as u32).to_le_bytes());
            out.extend_from_slice(&c.z);
        }
        match (self.last_point(), self.last_detail()) {
            (Some(p), Some(d)) => {
                out.push(2);
                out.extend_from_slice(bytes_of(&[p]));
                out.extend_from_slice(bytes_of(&[d]));
            }
            (Some(p), None) => {
                out.push(1);
                out.extend_from_slice(bytes_of(&[p]));
            }
            (None, _) => out.push(0),
        }
    }

    /// Read back what `save` wrote, moving `b` past it. `None` if the
    /// bytes are not a trace; a chunk is checked by its size only, not
    /// unpacked (the file carries a checksum for the rest).
    pub fn load(b: &mut &[u8]) -> Option<Saved> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if b.len() < n {
                return None;
            }
            let (head, rest) = b.split_at(n);
            *b = rest;
            Some(head)
        }
        fn u32_(b: &mut &[u8]) -> Option<u32> {
            Some(u32::from_le_bytes(take(b, 4)?.try_into().ok()?))
        }
        fn u64_(b: &mut &[u8]) -> Option<u64> {
            Some(u64::from_le_bytes(take(b, 8)?.try_into().ok()?))
        }
        let behind = u32_(b)? as usize;
        let chunks = u32_(b)? as usize;
        let open = match take(b, 1)?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        // (a chunk takes 28 bytes at least: a count that cannot be true
        // must not reserve memory)
        let mut cold: Vec<Cold> = Vec::with_capacity((chunks + open as usize).min(b.len() / 28));
        for _ in 0..chunks + open as usize {
            let (points, details, first, last) = (u32_(b)?, u32_(b)?, u64_(b)?, u64_(b)?);
            let n = u32_(b)? as usize;
            let z = take(b, n)?;
            let size = points as u64 * PACKED as u64 + details as u64 * DETAIL as u64;
            let follows = cold.last().map_or(true, |c| c.last < first);
            if points == 0 || first > last || !follows || zstd::zstd_safe::get_frame_content_size(z).ok().flatten() != Some(size) {
                return None;
            }
            cold.push(Cold { z: z.into(), points, details, first, last });
        }
        let tail = match take(b, 1)?[0] {
            0 => None,
            1 => Some(Box::new((from_bytes::<Packed>(take(b, PACKED)?)[0], None))),
            2 => Some(Box::new((from_bytes::<Packed>(take(b, PACKED)?)[0], Some(from_bytes::<Detail>(take(b, DETAIL)?)[0])))),
            _ => return None,
        };
        // what was saved had a last point, and its chunks end with it
        if cold.last().map_or(0, |c| c.last) > tail.as_ref()?.0.t() {
            return None;
        }
        let until = cold.iter().rev().nth(open as usize).map_or(0, |c| c.last);
        let mut trace = Trace { points: Vec::new(), details: Vec::new(), written: 0, cold, tail, ..Default::default() };
        trace.written = trace.len().saturating_sub(behind);
        Some(Saved { trace, open, behind, until })
    }

    /// Bring a trace saved whole up to date with what a later
    /// `save(after)` of it added: `was_open` says its own last chunk was
    /// the points then open, which the newer chunks contain.
    pub fn extend(&mut self, was_open: bool, newer: Saved) {
        if was_open && !newer.trace.cold.is_empty() {
            self.cold.pop();
        }
        // (a file put together from two that do not belong together must
        // not give a trace that runs backwards)
        let until = self.packed_until();
        self.cold.extend(newer.trace.cold.into_iter().filter(|c| c.first > until));
        self.cold.shrink_to_fit();
        self.tail = newer.trace.tail;
        self.written = self.len().saturating_sub(newer.behind);
    }
}

fn angle_diff(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}

/// A minute in which every position a second apart is kept: an aircraft's
/// first, and the one after it leaves or reaches the ground.
pub const DENSE_MS: Ms = 60_000;
/// Silence after which the last position heard before it is kept, and
/// after which the first one heard again is marked stale.
const GAP_KEEPS_LAST_MS: Ms = 10_000;
const GAP_STALE_MS: Ms = 15_000;

/// Positions kept whole before a landing: readsb's trace of an aircraft
/// that has just touched down holds every position of its last 47 s in
/// the air (94 and 95 of them at two a second, landing at 20 and 60 kt
/// and off a 900 ft/min approach), where until then it held one every
/// few seconds.
const RING: usize = 96;
/// Barometric altitude below which the newest positions are held back
/// for that: an aircraft higher up is not about to land. (La Paz, the
/// highest airport with airline traffic, is at 13,300 ft.)
const RING_BELOW_FT: i32 = 15_000;

/// The newest positions of an aircraft low enough to land, every one of
/// them, whether or not the trace kept it: 2.3 KB while it is low.
pub struct Ring {
    at: [Packed; RING],
    /// how many are held, and where the next goes
    held: u8,
    next: u8,
}

impl Ring {
    fn new() -> Box<Ring> {
        Box::new(Ring { at: [Packed::default(); RING], held: 0, next: 0 })
    }

    fn push(&mut self, p: Packed) {
        self.at[self.next as usize] = p;
        self.next = (self.next + 1) % RING as u8;
        self.held = (self.held + 1).min(RING as u8);
    }
}

/// Metres between two points a short way apart.
fn apart_m(a: &Point, b: &Point) -> f64 {
    let dlat = (a.lat - b.lat) as f64 * 0.111_195;
    let dlon = (a.lon - b.lon) as f64 * 0.111_195 * (a.lat as f64 * 1e-6).to_radians().cos();
    dlat.hypot(dlon)
}

/// Whether the position `p` adds enough to the trace, whose newest point
/// is `last`, to be kept. This is readsb's rule (3.16, `--json-trace-interval`
/// 10, 20 and 30) as far as it could be measured from outside, with
/// scripted flights that each changed one thing at one rate
/// (`aggbench flight`, 2 positions a second; `tools/trace_probe.sh`
/// plays them to both), and the whole then compared on live traffic:
///
/// - a point every `interval`; in a `dense` minute every second if the
///   aircraft is somewhere else than at the last point, and every three
///   where it is not (hovering, parked; at 1 and 2 kt, as its place
///   came to change, every 1.5 to 3 s);
/// - a change between ground and air;
/// - altitude: 75 ft and more up to 7,000 ft, 200 ft up to 12,000 ft,
///   375 ft above (a climb at 1,000 ft/min: a point every 4.5 s, 11 s
///   and 19.5 s); and less than that once it has lasted, when the
///   change times the seconds it took passes 31.25, 62.5 and 200 times
///   the interval's seconds (at 30 s, 500 ft/min above 12,000 ft is a
///   point every 27 s at 225 ft; the constants held at 10 and 20 s);
/// - track: any turn once the degrees, times the seconds it took and
///   half a second more, reach 18 (0.1, 1 and 4 degrees a second: a
///   point every 13.5, 4 and 2 s), and a sharp one sooner: 10 degrees
///   in half a second, 8.75 in one, 7.5 in one and a half (10 and 14
///   degrees a second: a point every second; 8, 7 and 6: every 1.5 s).
///   Nineteen rates from 0.07 to 20 degrees a second were flown; the
///   same at 100 and 500 kt and at every interval;
/// - ground speed: 10 kt, at any interval;
/// - vertical rate: more than 200 ft/min different from the last point's
///   (256 was kept and 192 not, and 224 in live traffic: a transponder
///   whose rate jitters in level flight gets a point every second or two);
/// - on the ground, by speed: once the knots, times the seconds and
///   half a second more, reach 80 (30, 40, 60 and 70 kt: a point every
///   2.5, 1.5, 1 and 1 s; from 80 kt every position), and when slower
///   once they reach 18.5 and a tenth of the interval has passed (1 and
///   2 kt: every 18 and 9 s; 5 to 20 kt: every 3.5 s at 30 s). A turn
///   on the ground brought no points of its own.
///
/// Not matched: readsb also keeps a point 350 ft from the last above
/// 12,000 ft about one time in four, by a rule not found; and an
/// aircraft that sends no track gets points from a track readsb works
/// out from its positions.
pub fn due(last: &Point, p: &Point, interval: Ms, dense: bool) -> bool {
    let dt_ms = p.t.saturating_sub(last.t);
    // (times are kept in tenths of a second)
    if dt_ms < 100 {
        return false;
    }
    let dt = dt_ms as f64 / 1000.0;
    let every = interval as f64 / 1000.0;
    let ground = p.alt == GROUND;
    if ground != (last.alt == GROUND) {
        return true;
    }
    let still = ground && (p.gs == u16::MAX || p.gs < 10);
    // (an aircraft standing still: 30 s at 30, and 18 to 20 s at 10)
    if dt_ms >= if still { interval.max(20_000) } else { interval } {
        return true;
    }
    if dense && (dt_ms >= 3000 || (dt_ms >= 1000 && (p.lat, p.lon) != (last.lat, last.lon))) {
        return true;
    }
    if !ground && p.track != u16::MAX && last.track != u16::MAX {
        let turn = angle_diff(p.track as f32 / 10.0, last.track as f32 / 10.0) as f64;
        if turn * (dt + 0.5) >= 18.0 || (dt <= 1.5 && turn >= 11.25 - 2.5 * dt) {
            return true;
        }
    }
    if ground {
        // (without a speed, the distance: a knot for a second is 0.514 m)
        let run = if p.gs != u16::MAX { p.gs as f64 / 10.0 * (dt + 0.5) } else { apart_m(last, p) / 0.514_444 + 0.5 };
        return run >= 80.0 || (run >= 18.5 && dt >= (every / 10.0).max(2.0) + 0.25);
    }
    // barometric altitude where both points have it, else geometric
    let (alt, was) = if p.alt != NONE && last.alt != NONE { (p.alt, last.alt) } else { (p.alt_geom, last.alt_geom) };
    if alt != NONE && was != NONE {
        let change = (alt - was).abs() as f64;
        let (at_once, lasting) = if alt <= 7000 {
            (50.0, 31.25)
        } else if alt <= 12_000 {
            (175.0, 62.5)
        } else {
            (350.0, 200.0)
        };
        if change > at_once || change * dt > lasting * every {
            return true;
        }
    }
    if p.gs != u16::MAX && last.gs != u16::MAX && (p.gs as i32 - last.gs as i32).abs() >= 100 {
        return true;
    }
    p.rate != i16::MIN && last.rate != i16::MIN && (p.rate as i32 - last.rate as i32).abs() > 200
}

/// The aircraft as a trace point would record it now.
fn point_now(a: &Aircraft, now: Ms) -> Point {
    let (lat, lon) = a.pos.v;
    let (rate, rflag) = match (a.baro_rate.get(now), a.geom_rate.get(now)) {
        (Some(r), _) => (r.clamp(-32000, 32000) as i16, 0),
        (None, Some(r)) => (r.clamp(-32000, 32000) as i16, FLAG_RATE_GEOM),
        _ => (i16::MIN, 0),
    };
    Point {
        t: now,
        lat: (lat * 1e6).round() as i32,
        lon: (lon * 1e6).round() as i32,
        alt: if a.on_ground(now) { GROUND } else { a.alt_baro.get(now).unwrap_or(NONE) },
        alt_geom: a.alt_geom.get(now).unwrap_or(NONE),
        gs: a.gs.get(now).map_or(u16::MAX, |g| (g * 10.0).round().min(65000.0) as u16),
        track: a.track.get(now).map_or(u16::MAX, |t| (t * 10.0).round() as u16),
        rate,
        geom_rate: a.geom_rate.get(now).map_or(i16::MIN, |r| r.clamp(-32000, 32000) as i16),
        ias: a.ias.get(now).unwrap_or(u16::MAX),
        flags: rflag,
        addr_type: a.addr_type as u8,
    }
}

fn push(trace: &mut Trace, p: Packed) {
    // (the open buffer grows to its limit and no further: doubling would
    // take it to twice that on every long flight)
    let open = &mut trace.points;
    if open.len() == open.capacity() && open.len() >= OPEN_MAX / 2 {
        open.reserve_exact(OPEN_MAX - open.len());
    }
    open.push(p);
    trace.tidy();
}

/// Called every few seconds for every aircraft: one that has gone silent
/// has its trace end where it was last heard, not at the last point the
/// rule happened to keep. (readsb: 10 s without a position and the last
/// one is in the trace; 9 s and it is not.)
pub fn settle(a: &mut Aircraft, now: Ms) {
    if a.trace_prev.map_or(false, |p| now.saturating_sub(p.t()) >= GAP_KEEPS_LAST_MS) {
        let prev = a.trace_prev.take().unwrap();
        if a.trace.last_point().map_or(false, |l| prev.t() > l.t()) {
            push(&mut a.trace, prev);
            a.trace_plain = a.trace_plain.saturating_add(1);
        }
    }
}

/// Record the aircraft's new position if it adds to the trace (`due`).
/// After a silence the position last heard before it is recorded too,
/// and the first one after it is marked stale: readsb does the first
/// from 10 s without a position and the second from 15 (gaps of 9 to 14
/// and of 15 to 200 s in a steady flight).
pub fn add(a: &mut Aircraft, now: Ms, interval: Ms) {
    let mut point = point_now(a, now);
    let track = a.track.get(now);
    let prev = a.trace_prev.replace(Packed::pack(&point));
    // (an aircraft that landed 30 s after it was first heard had a point
    // a second up to its landing, as any new aircraft, and no more)
    let dense = now < a.trace_dense_until;
    let low = !dense && point.alt != GROUND && if point.alt != NONE { point.alt } else { point.alt_geom } < RING_BELOW_FT;
    match &mut a.trace_ring {
        Some(ring) if low || point.alt == GROUND => ring.push(Packed::pack(&point)),
        None if low => a.trace_ring.insert(Ring::new()).push(Packed::pack(&point)),
        _ => a.trace_ring = None,
    }
    let mut landed = false;
    if let Some(mut last) = a.trace.last_point().map(|p| p.unpack()) {
        let silent = now.saturating_sub(prev.map_or(last.t, |p| p.t()));
        if silent >= GAP_KEEPS_LAST_MS {
            if let Some(prev) = prev.filter(|p| p.t() > last.t) {
                push(&mut a.trace, prev);
                a.trace_plain = a.trace_plain.saturating_add(1);
                last = prev.unpack();
            }
        }
        let stale = silent >= GAP_STALE_MS;
        if !stale && !due(&last, &point, interval, dense) {
            return;
        }
        if stale {
            point.flags |= FLAG_STALE;
        }
        let dt = now.saturating_sub(last.t);
        if dt > 1_800_000 || (last.alt == GROUND && point.alt != GROUND && dt > 600_000) {
            point.flags |= FLAG_LEG;
        }
        // (taking off at 140 kt and landing at 20 and 60: a point a
        // second for the 60 s that followed)
        if (last.alt == GROUND) != (point.alt == GROUND) {
            a.trace_dense_until = now + DENSE_MS;
            landed = point.alt == GROUND;
        }
    } else {
        // (as readsb marks the first point of every trace)
        point.flags |= FLAG_STALE;
    }
    if point.alt == GROUND && !landed {
        // (on the ground nothing is held back)
        a.trace_ring = None;
    }
    let flags = point.flags;
    let first_or_leg = a.trace.is_empty() || flags & FLAG_LEG != 0;
    let packed = Packed::pack(&point);
    if first_or_leg || a.trace_plain >= 3 {
        a.trace_plain = 0;
        let b = |f: &crate::track::F<bool>| f.get(now).map_or(-1, |v| v as i8);
        let n = |f: &crate::track::F<u8>| f.get(now).map_or(-1, |v| v as i8);
        let d = Detail {
            // the point's own time, which is kept to a tenth of a
            // second: the two are matched by it when the trace is
            // served. (Stamped to the millisecond, 5 details of 8,453
            // points were ever shown.)
            t: packed.t(),
            callsign: a.callsign.get(now).unwrap_or([0; 8]),
            squawk: a.squawk.get(now).unwrap_or(u16::MAX),
            category: a.category,
            emergency: a.emergency.get(now).unwrap_or(u8::MAX),
            nav_alt_mcp: a.nav_alt_mcp.get(now).unwrap_or(u32::MAX),
            nav_qnh: a.nav_qnh.get(now).unwrap_or(f32::NAN),
            nav_heading: a.nav_heading.get(now).unwrap_or(f32::NAN),
            nav_modes: a.nav_modes.get(now).unwrap_or(u8::MAX),
            nic: a.nic,
            rc: a.rc.min(65535) as u16,
            version: a.version,
            nic_baro: b(&a.nic_baro),
            nac_p: n(&a.nac_p),
            nac_v: n(&a.nac_v),
            sil: n(&a.sil),
            sil_type: a.sil_type,
            gva: n(&a.gva),
            sda: n(&a.sda),
            alert: b(&a.alert),
            spi: b(&a.spi),
            pad1: [0; 2],
            pad2: [0; 2],
            track: track.unwrap_or(f32::NAN),
            tas: a.tas.get(now).unwrap_or(u16::MAX),
            mach: a.mach.get(now).unwrap_or(f32::NAN),
            roll: a.roll.get(now).unwrap_or(f32::NAN),
            mag_heading: a.mag_heading.get(now).unwrap_or(f32::NAN),
            true_heading: a.true_heading.get(now).unwrap_or(f32::NAN),
        };
        a.trace.details.push(d);
    } else {
        a.trace_plain += 1;
    }
    push(&mut a.trace, packed);
    if landed {
        if let Some(ring) = a.trace_ring.take() {
            a.trace.merge(&ring.at[..ring.held as usize]);
        }
    }
}

/// The aircraft has said where it is once more in the same words. One
/// standing still does, again and again, and is to be recorded as it
/// stands; but so does every transmission that a second receiver
/// relays, and that is no news. The two are told apart by time: a relay
/// comes right behind the position it repeats.
pub fn again(a: &mut Aircraft, now: Ms, interval: Ms) {
    if a.pos.t == now && a.trace_prev.map_or(false, |p| now.saturating_sub(p.t()) >= 400) {
        add(a, now, interval);
    }
}

fn addr_type_str(v: u8) -> &'static str {
    const ALL: [AddrType; 14] = [
        AddrType::AdsbIcao,
        AddrType::AdsbIcaoNt,
        AddrType::AdsrIcao,
        AddrType::TisbIcao,
        AddrType::Jaero,
        AddrType::Mlat,
        AddrType::Other,
        AddrType::ModeS,
        AddrType::AdsbOther,
        AddrType::AdsrOther,
        AddrType::TisbTrackfile,
        AddrType::TisbOther,
        AddrType::ModeA,
        AddrType::Unknown,
    ];
    ALL.get(v as usize).map_or("unknown", |t| t.as_str())
}

fn detail_json(o: &mut String, d: &Detail, p: &Point) {
    use std::fmt::Write;
    let _ = write!(o, "{{\"type\":\"{}\"", addr_type_str(p.addr_type));
    if d.callsign[0] != 0 {
        let _ = write!(o, ",\"flight\":\"{}\"", crate::json::esc(std::str::from_utf8(&d.callsign).unwrap_or("")));
    }
    if p.alt_geom != NONE {
        let _ = write!(o, ",\"alt_geom\":{}", p.alt_geom);
    }
    if d.tas != u16::MAX {
        let _ = write!(o, ",\"tas\":{}", d.tas);
    }
    if !d.mach.is_nan() {
        let _ = write!(o, ",\"mach\":{:.3}", d.mach);
    }
    if !d.track.is_nan() {
        let _ = write!(o, ",\"track\":{:.2}", d.track);
    }
    if !d.roll.is_nan() {
        let _ = write!(o, ",\"roll\":{:.2}", d.roll);
    }
    if !d.mag_heading.is_nan() {
        let _ = write!(o, ",\"mag_heading\":{:.2}", d.mag_heading);
    }
    if !d.true_heading.is_nan() {
        let _ = write!(o, ",\"true_heading\":{:.2}", d.true_heading);
    }
    if p.rate != i16::MIN && p.flags & FLAG_RATE_GEOM == 0 {
        let _ = write!(o, ",\"baro_rate\":{}", p.rate);
    }
    if p.geom_rate != i16::MIN {
        let _ = write!(o, ",\"geom_rate\":{}", p.geom_rate);
    }
    if d.squawk != u16::MAX {
        let _ = write!(o, ",\"squawk\":\"{:04x}\"", d.squawk);
    }
    if d.emergency != u8::MAX {
        let _ = write!(o, ",\"emergency\":\"{}\"", crate::json::EMERGENCY[d.emergency as usize & 7]);
    }
    if d.category != 0 {
        let _ = write!(o, ",\"category\":\"{:02X}\"", d.category);
    }
    if !d.nav_qnh.is_nan() {
        let _ = write!(o, ",\"nav_qnh\":{:.1}", d.nav_qnh);
    }
    if d.nav_alt_mcp != u32::MAX {
        let _ = write!(o, ",\"nav_altitude_mcp\":{}", d.nav_alt_mcp);
    }
    if !d.nav_heading.is_nan() {
        let _ = write!(o, ",\"nav_heading\":{:.2}", d.nav_heading);
    }
    if d.nav_modes != u8::MAX {
        o.push_str(",\"nav_modes\":");
        crate::json::nav_modes(o, d.nav_modes);
    }
    let _ = write!(o, ",\"nic\":{},\"rc\":{}", d.nic, d.rc);
    let mut num = |name: &str, v: i8| {
        if v >= 0 {
            let _ = write!(o, ",\"{name}\":{v}");
        }
    };
    num("version", d.version);
    num("nic_baro", d.nic_baro);
    num("nac_p", d.nac_p);
    num("nac_v", d.nac_v);
    num("sil", d.sil);
    if d.sil_type < 3 {
        let _ = write!(o, ",\"sil_type\":\"{}\"", ["unknown", "perhour", "persample"][d.sil_type as usize]);
    }
    let mut num = |name: &str, v: i8| {
        if v >= 0 {
            let _ = write!(o, ",\"{name}\":{v}");
        }
    };
    num("gva", d.gva);
    num("sda", d.sda);
    num("alert", d.alert);
    num("spi", d.spi);
    o.push('}');
}

/// tar1090's trace file for `points`, a slice of the aircraft's trace.
pub fn render(addr: u32, db: Option<&crate::db::Entry>, points: &[Packed], details: &[Detail], out: &mut String) {
    use std::fmt::Write;
    let mut di = details.partition_point(|d| points.first().map_or(true, |p| d.t < p.t()));
    let _ = write!(out, "{{\"icao\":\"{}\",\n", crate::json::hex(addr));
    if let Some(d) = db {
        if !d.reg.is_empty() {
            let _ = write!(out, "\"r\":\"{}\",\n", crate::json::esc(&d.reg));
        }
        if !d.typ.is_empty() {
            let _ = write!(out, "\"t\":\"{}\",\n", crate::json::esc(&d.typ));
        }
        if d.flags != 0 {
            let _ = write!(out, "\"dbFlags\":{},\n", d.flags);
        }
        if !d.desc.is_empty() {
            let _ = write!(out, "\"desc\":\"{}\",\n", crate::json::esc(&d.desc));
        }
        if !d.own_op.is_empty() {
            let _ = write!(out, "\"ownOp\":\"{}\",\n", crate::json::esc(&d.own_op));
        }
        if !d.year.is_empty() {
            let _ = write!(out, "\"year\":\"{}\",\n", crate::json::esc(&d.year));
        }
    }
    let base = points.first().map_or(0, |p| p.t());
    let _ = write!(out, "\"version\": \"aggd {}\",\n\"timestamp\": {:.3},\n\"trace\":[ \n", env!("CARGO_PKG_VERSION"), base as f64 / 1000.0);
    for (i, p) in points.iter().enumerate() {
        let p = &p.unpack();
        let _ = write!(
            out,
            "[{:.2},{:.6},{:.6},",
            (p.t - base) as f64 / 1000.0,
            p.lat as f64 / 1e6,
            p.lon as f64 / 1e6
        );
        match p.alt {
            NONE => out.push_str("null"),
            GROUND => out.push_str("\"ground\""),
            v => {
                let _ = write!(out, "{v}");
            }
        }
        if p.gs == u16::MAX {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{:.1}", p.gs as f64 / 10.0);
        }
        if p.track == u16::MAX {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{:.1}", p.track as f64 / 10.0);
        }
        let _ = write!(out, ",{}", p.flags);
        if p.rate == i16::MIN {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{}", p.rate);
        }
        // the detail recorded with this point, if there is one
        while di < details.len() && details[di].t < p.t {
            di += 1;
        }
        if di < details.len() && details[di].t == p.t {
            out.push(',');
            detail_json(out, &details[di], p);
            di += 1;
        } else {
            out.push_str(",null");
        }
        let _ = write!(out, ",\"{}\"", addr_type_str(p.addr_type));
        if p.alt_geom == NONE {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{}", p.alt_geom);
        }
        if p.geom_rate == i16::MIN {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{}", p.geom_rate);
        }
        if p.ias == u16::MAX {
            out.push_str(",null");
        } else {
            let _ = write!(out, ",{}", p.ias);
        }
        out.push_str(",null]");
        if i + 1 < points.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]}\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_survive_packing() {
        assert_eq!(std::mem::size_of::<Packed>(), 24);
        let p = Point { t: 1_794_831_525_100, lat: 44_938_272, lon: -18_733_297, alt: 35_025, alt_geom: 35_600, gs: 4321, track: 3176, rate: -1280, geom_rate: 640, ias: 281, flags: FLAG_LEG, addr_type: 5 };
        let q = Packed::pack(&p).unpack();
        assert_eq!((q.t, q.lat, q.lon, q.alt, q.alt_geom, q.gs, q.track, q.rate, q.geom_rate, q.flags, q.addr_type), (p.t, p.lat, p.lon, 35_025, 35_600, 4321, 3176, -1280, 640, FLAG_LEG, 5));
        assert_eq!(q.ias, 280);
        // nothing known, on the ground, and geometric altitude only
        let e = Point { alt: NONE, alt_geom: NONE, gs: u16::MAX, track: u16::MAX, rate: i16::MIN, geom_rate: i16::MIN, ias: u16::MAX, ..p };
        let q = Packed::pack(&e).unpack();
        assert_eq!((q.alt, q.alt_geom, q.gs, q.track, q.rate, q.geom_rate, q.ias), (NONE, NONE, u16::MAX, u16::MAX, i16::MIN, i16::MIN, u16::MAX));
        assert_eq!(Packed::pack(&Point { alt: GROUND, ..p }).unpack().alt, GROUND);
        let q = Packed::pack(&Point { alt: NONE, alt_geom: 12_000, ..p }).unpack();
        assert_eq!((q.alt, q.alt_geom), (NONE, 12_000));
    }
}

#[cfg(test)]
mod cold_tests {
    use super::*;

    fn point(i: u32) -> Packed {
        Packed { t: 1000 + i * 200, lat: 45_000_000 + i as i32 * 913, lon: 9_000_000 - i as i32 * 1291, alt: (i % 90) as i16 - 40, gs: 4400 + (i % 7) as u16, track: (i * 3 % 3600) as u16, geom_delta: -3, rate: (i % 5) as i8 - 2, geom_rate: i8::MIN, ias: 140, flags: (i % 3) as u8, addr_type: 0 }
    }

    fn detail(i: u32) -> Detail {
        let mut d: Detail = unsafe { std::mem::zeroed() };
        d.t = point(i).t();
        d.callsign = *b"AZA1234 ";
        d.nav_qnh = f32::NAN;
        d.mach = 0.78 + i as f32 / 1e4;
        d.sil = -1;
        d
    }

    /// Built a point at a time, as in flight, with the older part packed
    /// away along the way: the trace read back is the trace put in.
    #[test]
    fn a_trace_packed_away_reads_back_the_same() {
        let (mut t, mut plain) = (Trace::default(), Trace::default());
        for i in 0..1000u32 {
            for x in [&mut t, &mut plain] {
                x.points.push(point(i));
                if i % 6 == 0 {
                    x.details.push(detail(i));
                }
            }
            t.tidy();
            assert!(t.points.len() < OPEN_MAX);
            assert_eq!(bytes_of(&[t.last_point().unwrap()]), bytes_of(&[point(i)]));
            assert_eq!(t.last_detail().unwrap().t, detail(i - i % 6).t);
        }
        assert_eq!((t.len(), t.first_t()), (1000, Some(point(0).t())));
        let (p, d) = t.all();
        assert_eq!((bytes_of(&p), bytes_of(&d)), (bytes_of(&plain.points), bytes_of(&plain.details)));
        // silent: nothing stays open, and it is a fraction of the size
        t.compact(0);
        assert!(t.points.is_empty() && t.details.is_empty() && t.len() == 1000);
        assert!(t.bytes() * 3 < plain.bytes(), "{} of {}", t.bytes(), plain.bytes());
        assert_eq!(bytes_of(&[t.last_point().unwrap()]), bytes_of(&[point(999)]));
        assert_eq!(t.last_detail().unwrap().t, detail(996).t);
        let (p, d) = t.all();
        assert_eq!((bytes_of(&p), bytes_of(&d)), (bytes_of(&plain.points), bytes_of(&plain.details)));
        // the same trace restored from a file reads the same
        let r = Trace::restored(&plain.points, 5);
        assert_eq!((bytes_of(&r.all().0), r.written, r.points.len()), (bytes_of(&plain.points), 5, 0));
        // the aircraft returns: new points follow the packed ones
        t.points.push(point(1000));
        assert_eq!((t.len(), t.all().0.len()), (1001, 1001));
        // the day moves on: old chunks go whole, with their details, and
        // nothing newer than the cut goes
        t.written = 700;
        let cut = point(300).t();
        let went = t.trim_before(cut);
        assert!(went > 100 && went <= 300, "{went}");
        assert_eq!((t.len(), t.written, t.first_t()), (1001 - went, 700 - went, Some(point(went as u32).t())));
        let (p, d) = t.all();
        assert_eq!(bytes_of(&p), bytes_of(&plain.points[went..].iter().copied().chain([point(1000)]).collect::<Vec<_>>()));
        assert!(d[0].t >= p[0].t() && d.len() == plain.details.iter().filter(|x| x.t >= p[0].t()).count());
        // and when all of it is old, all of it goes
        assert_eq!(t.trim_before(point(2000).t()), 1001 - went);
        assert!(t.is_empty() && t.last_point().is_none() && t.written == 0);
        // nothing to pack: nothing happens
        let mut e = Trace::default();
        e.compact(0);
        assert!(e.is_empty() && e.first_t().is_none() && e.last_point().is_none() && e.trim_before(5) == 0);
    }

    /// The newest points, taken without unpacking the whole trace, are
    /// the end of the whole trace, and render to the same document.
    #[test]
    fn the_newest_points_are_the_end_of_the_trace() {
        let render_tail = |t: &Trace, n: usize| {
            let (p, d) = t.all();
            let mut out = String::new();
            render(0x4ca123, None, &p[p.len().saturating_sub(n)..], &d, &mut out);
            out
        };
        let render_last = |t: &Trace, n: usize| {
            let (p, d) = t.last(n);
            let mut out = String::new();
            render(0x4ca123, None, &p, &d, &mut out);
            (p.len(), out)
        };
        let mut t = Trace::default();
        for i in 0..700u32 {
            t.points.push(point(i));
            if i % 6 == 0 {
                t.details.push(detail(i));
            }
            t.tidy();
            if i % 37 == 0 || i == 699 {
                // open points only, a chunk and a part, several chunks, all
                for n in [1, 20, 142, 300, 2000] {
                    let (len, doc) = render_last(&t, n);
                    assert_eq!(len, n.min(i as usize + 1), "{i} {n}");
                    assert_eq!(doc, render_tail(&t, n), "{i} {n}");
                }
            }
        }
        // silent, nothing open
        t.compact(0);
        assert_eq!(render_last(&t, 142), (142, render_tail(&t, 142)));
        assert_eq!(render_last(&Trace::default(), 142).0, 0);
    }

    #[test]
    fn trace_file_names() {
        assert_eq!(file_addr("trace_recent_4ca123.json"), Some((true, 0x4ca123)));
        assert_eq!(file_addr("trace_full_~0abc12.json"), Some((false, 0x0abc12 | modes::NON_ICAO)));
        assert_eq!(file_addr(".trace_full_4ca123.json.tmp"), None);
        assert_eq!(file_addr("trace_full_zz.json"), None);
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;
    use crate::track::F;
    use modes::Source;

    const T: Ms = 1_795_680_000_000;

    /// A flight for `secs` seconds from `start`, two positions a second,
    /// each thing changing at its own steady rate; returns the seconds
    /// between the points the rule keeps (but for the first, which is
    /// measured from wherever the start fell between two altitude steps).
    #[allow(clippy::too_many_arguments)]
    fn fly(start: Point, secs: f64, ft_min: f64, deg_s: f64, kt_s: f64, interval: Ms, period_ms: u64) -> Vec<f64> {
        let mut last = start;
        let mut gaps = Vec::new();
        let mut ms = period_ms;
        while ms as f64 <= secs * 1000.0 {
            let s = ms as f64 / 1000.0;
            let gs = start.gs as f64 / 10.0 + kt_s * s;
            let on_ground = start.alt == GROUND;
            let p = Point {
                t: start.t + ms,
                // (north at the ground speed: only the ground rule looks)
                lat: start.lat + (start.gs as f64 / 10.0 * 0.514_444 * s / 0.111_195).round() as i32,
                alt: if on_ground { GROUND } else { ((start.alt as f64 + ft_min * s / 60.0) / 25.0).round() as i32 * 25 },
                gs: (gs * 10.0).round() as u16,
                track: if start.track == u16::MAX { u16::MAX } else { (((start.track as f64 / 10.0 + deg_s * s).rem_euclid(360.0)) * 10.0).round() as u16 },
                ..start
            };
            if due(&last, &p, interval, false) {
                gaps.push((p.t - last.t) as f64 / 1000.0);
                last = p;
            }
            ms += period_ms;
        }
        gaps.remove(0);
        gaps
    }

    fn at(alt: i32) -> Point {
        Point { t: T, lat: 45_000_000, lon: 9_000_000, alt, alt_geom: NONE, gs: 3000, track: 900, rate: 0, geom_rate: i16::MIN, ias: u16::MAX, flags: 0, addr_type: 0 }
    }

    /// The spacing a flight settles to. (Altitude comes in 25 ft steps:
    /// the first points of a slow climb can come a step early, until
    /// each falls just after a step, on readsb as here.)
    fn all(gaps: &[f64], want: f64) -> bool {
        gaps.len() >= 3 && gaps[gaps.len() - 3..].iter().all(|g| *g == want)
    }

    #[test]
    fn steady_flight_gets_a_point_every_interval_and_a_new_aircraft_one_a_second() {
        assert!(all(&fly(at(30_000), 300.0, 0.0, 0.0, 0.0, 30_000, 500), 30.0));
        assert!(all(&fly(at(30_000), 300.0, 0.0, 0.0, 0.0, 10_000, 500), 10.0));
        let (last, mut p) = (at(30_000), at(30_000));
        // the dense minute: every second, whatever the positions' own rate
        p.lon += 2000;
        p.t += 500;
        assert!(!due(&last, &p, 30_000, true));
        p.t += 500;
        assert!(due(&last, &p, 30_000, true) && !due(&last, &p, 30_000, false));
        // standing on the ground: every three seconds, then every interval
        let (mut last, mut p) = (at(GROUND), at(GROUND));
        (last.gs, p.gs) = (0, 0);
        p.t += 2500;
        assert!(!due(&last, &p, 30_000, true));
        p.t += 500;
        assert!(due(&last, &p, 30_000, true) && !due(&last, &p, 30_000, false));
        p.t = T + 29_500;
        assert!(!due(&last, &p, 30_000, false));
        p.t = T + 30_000;
        assert!(due(&last, &p, 30_000, false));
        // (at an interval of 10 s readsb kept one every 18 to 20)
        p.t = T + 19_500;
        assert!(!due(&last, &p, 10_000, false));
        p.t = T + 20_000;
        assert!(due(&last, &p, 10_000, false));
    }

    /// The spacings readsb kept for the same climbs (interval 30 s).
    #[test]
    fn a_climb_gets_a_point_every_75_200_or_375_ft_by_altitude() {
        // 1,000 ft/min: every 4.5 s low, 11 s between 7,000 and 12,000 ft, 19.5 s above
        assert!(all(&fly(at(1000), 120.0, 1000.0, 0.0, 0.0, 30_000, 500), 4.5));
        assert!(all(&fly(at(8000), 120.0, 1000.0, 0.0, 0.0, 30_000, 500), 11.0));
        assert!(all(&fly(at(15_000), 120.0, 1000.0, 0.0, 0.0, 30_000, 500), 19.5));
        assert!(all(&fly(at(20_000), 120.0, -1000.0, 0.0, 0.0, 30_000, 500), 19.5));
        // fast: the altitude alone decides
        assert!(all(&fly(at(15_000), 120.0, 3000.0, 0.0, 0.0, 30_000, 500), 7.5));
        assert!(all(&fly(at(7500), 60.0, 3000.0, 0.0, 0.0, 30_000, 500), 4.0));
        assert!(all(&fly(at(1000), 60.0, 3000.0, 0.0, 0.0, 30_000, 500), 1.5));
        // slow: less altitude is enough once it has taken long
        assert!(all(&fly(at(13_000), 200.0, 500.0, 0.0, 0.0, 30_000, 500), 27.0));
        assert!(all(&fly(at(13_000), 200.0, 400.0, 0.0, 0.0, 30_000, 500), 30.0));
        assert!(all(&fly(at(7100), 200.0, 500.0, 0.0, 0.0, 30_000, 500), 15.5));
        assert!(all(&fly(at(500), 600.0, 150.0, 0.0, 0.0, 30_000, 500), 20.0));
        // and sooner at a shorter interval: 1,200 ft/min above 12,000 ft
        // was a point every 17.5 s at 30 and every 15 s at 20
        assert!(all(&fly(at(13_000), 120.0, 1200.0, 0.0, 0.0, 30_000, 500), 17.5));
        assert!(all(&fly(at(13_000), 120.0, 1200.0, 0.0, 0.0, 20_000, 500), 15.0));
    }

    #[test]
    fn the_bands_change_just_above_7000_and_12000_ft() {
        let p = |alt: i32, s: u64| Point { t: T + s * 1000, ..at(alt) };
        // 75 ft in 4 s: kept at 7,000 ft, not at 7,050
        assert!(due(&p(6925, 0), &p(7000, 4), 30_000, false));
        assert!(!due(&p(6975, 0), &p(7050, 4), 30_000, false));
        // 200 ft in 4 s: kept at 11,975 ft, not at 12,175
        assert!(due(&p(11_775, 0), &p(11_975, 4), 30_000, false));
        assert!(!due(&p(11_975, 0), &p(12_175, 4), 30_000, false));
        // without a barometric altitude the geometric one is used
        let g = |alt: i32, s: u64| Point { alt: NONE, alt_geom: alt, ..p(0, s) };
        assert!(due(&g(20_000, 0), &g(20_375, 4), 30_000, false));
        assert!(!due(&g(20_000, 0), &g(20_350, 4), 30_000, false));
    }

    /// Degrees a second and the seconds between points readsb kept.
    #[test]
    fn a_turn_gets_points_the_sooner_the_faster_it_is() {
        for (rate, want) in [(0.1, 13.5), (0.15, 11.0), (0.2, 9.5), (0.25, 8.5), (0.5, 6.0), (0.7, 5.0), (1.0, 4.0), (1.5, 3.5), (2.0, 3.0), (2.5, 2.5), (3.0, 2.5), (4.0, 2.0), (6.0, 1.5), (7.0, 1.5), (8.0, 1.5), (10.0, 1.0), (14.0, 1.0)] {
            let gaps = fly(at(30_000), 90.0, 0.0, rate, 0.0, 30_000, 500);
            assert!(gaps.iter().all(|g| *g == want), "{rate} degrees a second: {gaps:?}");
        }
        // the same at any speed and interval
        let slow = Point { gs: 1000, ..at(3000) };
        assert!(all(&fly(slow, 60.0, 0.0, 1.0, 0.0, 10_000, 500), 4.0));
        // straight on, or no track: nothing
        let none = Point { track: u16::MAX, ..at(30_000) };
        assert!(all(&fly(none, 150.0, 0.0, 1.0, 0.0, 30_000, 500), 30.0));
    }

    #[test]
    fn a_change_of_10_kt_or_of_200_ft_a_minute_gets_a_point() {
        // 0.5 kt/s: every 20 s; 0.2 kt/s is 6 kt in an interval: nothing
        assert!(all(&fly(at(30_000), 120.0, 0.0, 0.0, 0.5, 30_000, 500), 20.0));
        assert!(all(&fly(at(30_000), 120.0, 0.0, 0.0, 0.2, 30_000, 500), 30.0));
        assert!(all(&fly(at(30_000), 30.0, 0.0, 0.0, 3.0, 30_000, 500), 3.5));
        // vertical rate, as readsb in live traffic: 224 ft/min apart kept, 192 not
        let (last, mut p) = (at(37_000), at(37_000));
        p.t += 500;
        p.rate = 192;
        assert!(!due(&last, &p, 30_000, false));
        p.rate = -224;
        assert!(due(&last, &p, 30_000, false));
        p.rate = i16::MIN;
        assert!(!due(&last, &p, 30_000, false));
    }

    /// Knots on the ground and the seconds between the points readsb kept.
    #[test]
    fn on_the_ground_points_come_the_sooner_the_faster_it_rolls() {
        for (kt, want) in [(1.0, 18.0), (2.0, 9.0), (5.0, 3.5), (10.0, 3.5), (20.0, 3.5), (30.0, 2.5), (40.0, 1.5), (50.0, 1.5), (60.0, 1.0), (70.0, 1.0), (90.0, 0.5), (140.0, 0.5)] {
            let start = Point { gs: (kt * 10.0) as u16, ..at(GROUND) };
            let gaps = fly(start, 90.0, 0.0, 0.0, 0.0, 30_000, 500);
            assert!(gaps.iter().all(|g| *g == want), "{kt} kt: {gaps:?}");
            // turning as it goes: the same
            assert_eq!(fly(start, 90.0, 0.0, 3.0, 0.0, 30_000, 500), gaps);
        }
        // at an interval of 10 s the slow ones come sooner: 2.5 s
        assert!(all(&fly(Point { gs: 150, ..at(GROUND) }, 60.0, 0.0, 0.0, 0.0, 10_000, 500), 2.5));
        // leaving the ground and reaching it are points whenever they happen
        let (g, mut a) = (at(GROUND), at(300));
        a.t += 500;
        assert!(due(&g, &a, 30_000, false) && due(&Point { t: T, ..a }, &Point { t: T + 500, ..g }, 30_000, false));
    }

    fn aircraft(alt: i32) -> Aircraft {
        let mut a = Aircraft::new(0x4ca123, T - 120_000);
        a.alt_baro = F { v: alt, t: T, src: Source::Adsb, long: false };
        a.gs = F { v: 300.0, t: T, src: Source::Adsb, long: false };
        a.track = F { v: 90.0, t: T, src: Source::Adsb, long: false };
        a
    }

    /// A position every half second from `from` to `to` seconds.
    fn hear(a: &mut Aircraft, from: f64, to: f64, interval: Ms) {
        let mut ms = (from * 1000.0) as u64;
        while ms as f64 <= to * 1000.0 {
            let now = T + ms;
            a.pos = F { v: (45.0, 9.0 + ms as f64 * 1e-6), t: now, src: Source::Adsb, long: false };
            (a.alt_baro.t, a.gs.t, a.track.t) = (now, now, now);
            add(a, now, interval);
            ms += 500;
        }
    }

    fn kept(a: &Aircraft) -> Vec<(f64, u8)> {
        a.trace.all().0.iter().map(|p| ((p.t() - T) as f64 / 1000.0, p.unpack().flags)).collect()
    }

    /// As readsb with the same gaps in a steady flight: 9 s of silence
    /// leaves no mark, from 10 s the position heard last before it is a
    /// point, from 15 s the first one after it is too, marked stale.
    #[test]
    fn a_silence_keeps_the_last_position_before_it_and_marks_the_first_after_it() {
        let mut a = aircraft(30_000);
        hear(&mut a, 0.0, 40.0, 30_000);
        hear(&mut a, 49.0, 100.0, 30_000);
        assert_eq!(kept(&a), [(0.0, FLAG_STALE), (30.0, 0), (60.0, 0), (90.0, 0)]);
        let mut a = aircraft(30_000);
        hear(&mut a, 0.0, 40.0, 30_000);
        hear(&mut a, 52.0, 100.0, 30_000);
        assert_eq!(kept(&a), [(0.0, FLAG_STALE), (30.0, 0), (40.0, 0), (70.0, 0), (100.0, 0)]);
        let mut a = aircraft(30_000);
        hear(&mut a, 0.0, 40.0, 30_000);
        hear(&mut a, 55.0, 100.0, 30_000);
        assert_eq!(kept(&a), [(0.0, FLAG_STALE), (30.0, 0), (40.0, 0), (55.0, FLAG_STALE), (85.0, 0)]);
    }

    /// An aircraft that is not heard again: its trace ends where it was
    /// last heard, once ten seconds have shown that it is gone.
    #[test]
    fn a_trace_ends_at_the_last_position_heard() {
        let mut a = aircraft(30_000);
        hear(&mut a, 0.0, 47.5, 30_000);
        settle(&mut a, T + 52_000);
        assert_eq!(kept(&a).last(), Some(&(30.0, 0)));
        settle(&mut a, T + 58_000);
        assert_eq!(kept(&a).last(), Some(&(47.5, 0)));
        // once only, and it is the point the next position is measured from
        settle(&mut a, T + 70_000);
        assert_eq!(kept(&a).len(), 3);
        hear(&mut a, 77.5, 80.0, 30_000);
        assert_eq!(kept(&a)[3..], [(77.5, FLAG_STALE)]);
    }

    /// readsb: after take-off at 140 kt, and after landing at 20 and at
    /// 60 kt, a point a second for a minute; then the usual rule.
    #[test]
    fn leaving_or_reaching_the_ground_starts_a_minute_of_points_a_second() {
        let mut a = aircraft(1500);
        hear(&mut a, 0.0, 100.0, 30_000);
        assert_eq!(kept(&a).len(), 4);
        a.ground = F { v: true, t: T + 100_000, src: Source::Adsb, long: false };
        a.gs.v = 20.0;
        let mut ms = 100_500;
        while ms <= 220_000 {
            (a.ground.t, a.gs.t) = (T + ms, T + ms);
            a.pos = F { v: (45.0, 9.1 + ms as f64 * 1e-7), t: T + ms, src: Source::Adsb, long: false };
            add(&mut a, T + ms, 30_000);
            ms += 500;
        }
        let k = kept(&a);
        // the last 96 positions in the air and at touchdown, every one:
        // 53 s to 100.5 s, where there had been a point every 30 s
        assert_eq!(k[..3], [(0.0, FLAG_STALE), (30.0, 0), (53.0, 0)]);
        assert!(k[2..98].windows(2).all(|w| w[1].0 - w[0].0 == 0.5), "{k:?}");
        // touchdown, then 59 more a second apart, then as any aircraft
        // rolling at 20 kt, every 3.5 s
        assert_eq!(k[97].0, 100.5);
        assert!(k[97..157].windows(2).all(|w| w[1].0 - w[0].0 == 1.0), "{k:?}");
        assert!(k.len() > 170 && k[156..].windows(2).all(|w| w[1].0 - w[0].0 == 3.5), "{k:?}");
        assert!(a.trace_ring.is_none());
    }

    /// Nothing is held back for an aircraft too high to be landing, and
    /// what was held is dropped when it climbs away.
    #[test]
    fn only_an_aircraft_low_enough_to_land_has_its_newest_positions_held() {
        let mut a = aircraft(30_000);
        hear(&mut a, 0.0, 10.0, 30_000);
        assert!(a.trace_ring.is_none());
        a.alt_baro.v = 14_000;
        hear(&mut a, 10.5, 20.0, 30_000);
        assert!(a.trace_ring.is_some());
        a.alt_baro.v = 15_000;
        hear(&mut a, 20.5, 21.0, 30_000);
        assert!(a.trace_ring.is_none());
    }

    /// A parked aircraft says the same thing every time, which the
    /// tracker takes for one transmission heard again: its trace gets a
    /// point every interval all the same (readsb: every 30 s), none of
    /// them marked stale; and a relay through a second receiver, right
    /// behind the first, is not a position of its own.
    #[test]
    fn an_aircraft_standing_still_is_recorded_as_it_stands() {
        let mut a = aircraft(1500);
        a.ground = F { v: true, t: T, src: Source::Adsb, long: false };
        a.gs.v = 0.0;
        a.pos = F { v: (45.0, 9.0), t: T, src: Source::Adsb, long: false };
        add(&mut a, T, 30_000);
        let mut ms = 500;
        while ms <= 100_000 {
            let now = T + ms;
            (a.ground.t, a.gs.t, a.pos.t) = (now, now, now);
            again(&mut a, now, 30_000);
            // the same frame from another receiver, 50 ms on
            a.pos.t = now + 50;
            again(&mut a, now + 50, 30_000);
            ms += 500;
        }
        assert_eq!(kept(&a), [(0.0, FLAG_STALE), (30.0, 0), (60.0, 0), (90.0, 0)]);
    }

    /// readsb, an aircraft that touched down 30 s after it was first
    /// heard: a point a second up to the landing, as for any new
    /// aircraft, not every position.
    #[test]
    fn a_landing_in_the_first_minute_adds_nothing_to_the_points_a_second() {
        let mut a = aircraft(100);
        a.trace_dense_until = T + DENSE_MS;
        hear(&mut a, 0.0, 30.0, 30_000);
        a.ground = F { v: true, t: T + 30_500, src: Source::Adsb, long: false };
        a.pos = F { v: (45.0, 9.1), t: T + 30_500, src: Source::Adsb, long: false };
        add(&mut a, T + 30_500, 30_000);
        let k = kept(&a);
        assert_eq!(k.len(), 32);
        assert!(k[..31].windows(2).all(|w| w[1].0 - w[0].0 == 1.0) && k[31].0 == 30.5, "{k:?}");
    }

    /// Every fourth point says what else was known, and the served
    /// trace shows it at that point, whatever millisecond it was heard at.
    #[test]
    fn one_point_in_four_carries_the_detail_and_it_is_served() {
        let mut a = aircraft(30_000);
        a.callsign = F { v: *b"AZA1234 ", t: T, src: Source::Adsb, long: false };
        for i in 0..20u64 {
            let now = T + i * 30_000 + 37 * i + 5;
            a.pos = F { v: (45.0, 9.0 + i as f64 * 0.05), t: now, src: Source::Adsb, long: false };
            (a.alt_baro.t, a.gs.t, a.track.t, a.callsign.t) = (now, now, now, now);
            add(&mut a, now, 30_000);
        }
        let (points, details) = a.trace.all();
        assert_eq!((points.len(), details.len()), (20, 5));
        let mut out = String::new();
        render(a.addr, None, &points, &details, &mut out);
        let rows: Vec<&str> = out.lines().filter(|l| l.starts_with('[')).collect();
        let with: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.contains("\"flight\":\"AZA1234 \"")).map(|(i, _)| i).collect();
        assert_eq!(with, [0, 4, 8, 12, 16]);
        assert_eq!(rows.len(), 20);
    }

    /// The same as its first minute: an aircraft is new for 60 s after
    /// it is first heard (readsb, with positions 0.3, 0.5, 1 and 2.5 s
    /// apart: points 0.9, 1, 1 and 2.5 s apart until the 60th second).
    #[test]
    fn a_new_aircraft_has_a_point_a_second_for_its_first_minute() {
        let mut a = aircraft(30_000);
        a.created = T;
        a.trace_dense_until = T + DENSE_MS;
        hear(&mut a, 2.0, 125.0, 30_000);
        let k = kept(&a);
        assert_eq!((k[0].0, k[57].0, k[58].0, k[59].0), (2.0, 59.0, 89.0, 119.0));
        assert_eq!(k.len(), 60);
    }
}
