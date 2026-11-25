//! The aircraft table: what the network currently knows about each
//! airframe, and the rules by which one decoded message changes it.

use modes::{cpr, AddrType, Cpr, Heading, Msg, Source};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Milliseconds since the Unix epoch.
pub type Ms = u64;

/// A field stays valid this long after its last update.
// (Measured against a running readsb whose file is up to a second old: on
// recorded traffic it still showed a value 60.0 s old 9 times in 10 and
// one 61.0 s old 1 time in 10.)
pub const FIELD_TTL: Ms = 60_000;
/// A lesser source may replace a field once the better one has been
/// silent this long (on readsb: more than 14 s, at most 21 s; three
/// times that for the rarely sent callsign).
pub const LESSER_AFTER: Ms = 15_000;
/// How long an aircraft stays an ADS-B aircraft (its kind of source, the
/// version it stated) after its last extended squitter (from readsb's
/// behaviour in experiments and on recorded traffic).
pub const ES_TTL: Ms = 90_000;
/// A lesser source may replace a position once it is this old.
pub const STALE: Ms = 15_000;
/// An ADS-C report may replace a position from another source once it
/// is this old (five minutes, measured on readsb).
pub const JAERO_AFTER: Ms = 300_000;
/// An aircraft placed by MLAT or ADS-C and also heard directly is shown
/// as that kind for this long after the last such position, then as the
/// Mode S aircraft it is, the position staying (measured on readsb with
/// tools/src_probe.py: 45 s after the last MLAT result, 91 s after an
/// ADS-C report; one heard no other way keeps the kind).
pub const MLAT_TYPE_TTL: Ms = 45_000;
pub const JAERO_TYPE_TTL: Ms = 90_000;
/// Even and odd position messages pair up within a window that depends
/// on how far the aircraft can have gone. Measured on readsb with pairs
/// a set time apart: half a minute for an aircraft whose ground speed
/// was heard in the last 20 s (30 s yes, 30.5 no), less above 318 kt so
/// that it covers about 2.65 nautical miles (440 kt: 20 s yes, 21 no;
/// 600 kt: 14 yes, 16 no; 900 kt: 10.5 yes, 12 no), and ten seconds when
/// no recent speed is known (10 s yes, 10.5 no).
const PAIR_AIRBORNE_MAX: Ms = 30_200;
const PAIR_AIRBORNE_BLIND: Ms = 10_200;
const PAIR_SPEED_FRESH: Ms = 20_500;
const PAIR_REACH_NM: f64 = 2.65;
fn pair_airborne(a: &Aircraft, now: Ms) -> Ms {
    match a.gs.get(now) {
        Some(gs) if now.saturating_sub(a.gs.t) <= PAIR_SPEED_FRESH => {
            if gs as f64 * (PAIR_AIRBORNE_MAX as f64) <= PAIR_REACH_NM * 3_600_000.0 {
                PAIR_AIRBORNE_MAX
            } else {
                (PAIR_REACH_NM * 3_600_000.0 / gs as f64) as Ms
            }
        }
        _ => PAIR_AIRBORNE_BLIND,
    }
}
const PAIR_SURFACE: Ms = 25_000;

#[derive(Clone, Copy, Default)]
pub struct F<T> {
    pub v: T,
    pub t: Ms,
    pub src: Source,
    /// a field sent rarely, kept three times as long
    pub long: bool,
}

impl<T: Copy> F<T> {
    #[inline]
    pub fn valid(&self, now: Ms) -> bool {
        self.src != Source::Invalid && now.saturating_sub(self.t) <= self.ttl()
    }
    #[inline]
    fn ttl(&self) -> Ms {
        match self.src {
            Source::Jaero => JAERO_TTL.load(std::sync::atomic::Ordering::Relaxed),
            _ if self.long => 3 * FIELD_TTL,
            _ => FIELD_TTL,
        }
    }
    #[inline]
    pub fn get(&self, now: Ms) -> Option<T> {
        self.valid(now).then_some(self.v)
    }
    /// Take `v` unless a better source set the field in the last
    /// half minute (or it has expired sooner).
    #[inline]
    pub fn accept(&mut self, v: T, src: Source, now: Ms) -> bool {
        let wait = if self.long { 3 * LESSER_AFTER } else { LESSER_AFTER };
        if src < self.src && now.saturating_sub(self.t) < wait.min(self.ttl()) {
            return false;
        }
        self.v = v;
        self.t = now;
        self.src = src;
        true
    }
    /// Take `v` unless a better source set the field within `stale`.
    #[inline]
    pub fn accept_after(&mut self, v: T, src: Source, now: Ms, stale: Ms) -> bool {
        if src < self.src && now.saturating_sub(self.t) < stale {
            return false;
        }
        self.v = v;
        self.t = now;
        self.src = src;
        true
    }
    #[inline]
    pub fn age_s(&self, now: Ms) -> f64 {
        now.saturating_sub(self.t) as f64 / 1000.0
    }
}

/// How long a JAERO (ADS-C) datum stays on the map; set from the command line.
pub static JAERO_TTL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(33 * 60_000);

#[derive(Clone, Copy, Default)]
struct CprSlot {
    c: Option<Cpr>,
    t: Ms,
}

/// Receivers whose position message pairs are kept per aircraft.
const CPR_ROWS: usize = 3;

/// Frames remembered per aircraft for duplicate detection, and for how
/// long (ms). A transponder repeats an unchanged message at most twice
/// a second, so within the window equal bytes are the same transmission
/// heard by another receiver, or one that changes nothing.
const RECENT: usize = 12;
const DUP_WINDOW: u32 = 400;
/// How long a contradicting position waits for a second receiver to
/// relay the same message before its only witness is held responsible
/// (ms): longer than any feeder's path to us.
const BLAME_AFTER: u32 = 2500;
/// A receiver counts towards an aircraft's receiver count this long (ms).
const RX_WINDOW: u32 = 5000;


/// Message classes for the reduced output stream.
pub const KINDS: usize = 12;

pub struct Aircraft {
    pub addr: u32,
    pub addr_type: AddrType,
    pub addr_type_t: Ms,
    pub seen: Ms,
    /// When a message that carries its own checksum was last heard (an
    /// all-call reply or an extended squitter), or one from another kind
    /// of feed. Replies addressed by parity alone lean on it.
    pub checked: Ms,
    pub messages: u32,
    sig: [f32; 8],
    sig_i: u8,
    pub callsign: F<[u8; 8]>,
    pub alt_baro: F<i32>,
    /// an altitude from a surveillance reply, waiting to be confirmed
    alt_candidate: (i32, Ms),
    pub alt_geom: F<i32>,
    pub geom_delta: F<i32>,
    pub gs: F<f32>,
    pub ias: F<u16>,
    pub tas: F<u16>,
    pub mach: F<f32>,
    pub track: F<f32>,
    pub track_rate: F<f32>,
    pub roll: F<f32>,
    pub mag_heading: F<f32>,
    pub true_heading: F<f32>,
    pub baro_rate: F<i32>,
    pub geom_rate: F<i32>,
    pub squawk: F<u16>,
    /// a squawk heard once; it counts when heard again
    squawk_tentative: u16,
    /// callsign last sent on the BaseStation output
    pub sbs_callsign: [u8; 8],
    pub emergency: F<u8>,
    pub category: u8,
    pub ground: F<bool>,
    pub nav_qnh: F<f32>,
    pub nav_alt_mcp: F<u32>,
    pub nav_alt_fms: F<u32>,
    pub nav_heading: F<f32>,
    pub nav_modes: F<u8>,
    pub alert: F<bool>,
    pub spi: F<bool>,
    pub version: i8,
    /// the version stated through a rebroadcast (ADS-R) and through TIS-B
    pub adsr_version: i8,
    pub tisb_version: i8,
    pub nic_a: bool,
    pub nic_c: bool,
    pub hrd_magnetic: bool,
    pub track_not_heading: bool,
    pub nic_baro: F<bool>,
    pub nac_p: F<u8>,
    pub nac_v: F<u8>,
    pub sil: F<u8>,
    /// 0 unknown, 1 per hour, 2 per sample; 255 never stated
    pub sil_type: u8,
    /// when an extended squitter was last heard from it
    pub es_seen: Ms,
    /// when one of its own transmissions last came in through a receiver
    /// (as opposed to a position solved elsewhere: MLAT, ADS-C)
    pub direct: Ms,
    /// when it was first heard
    pub created: Ms,
    /// its own squitters currently count it as an ADS-B aircraft: set by a
    /// decoded position, cleared by a squitter that finds none current
    pub adsb_pos: Ms,
    /// When the version was last set.
    pub ver_t: Ms,
    /// when an extended squitter said it has no position to give
    pub gps_lost: Ms,
    pub gva: F<u8>,
    pub sda: F<u8>,
    pub pos: F<(f64, f64)>,
    pub nic: u8,
    pub rc: u32,
    /// receiver that supplied the current position
    pub pos_rx: u64,
    pub pos_surface: bool,
    pos_bad: u8,
    /// when the position was last backed by a second receiver hearing
    /// the same transmission
    pub pos_confirmed: Ms,
    /// a position message that contradicted a confirmed track, heard so
    /// far from one receiver only: (receiver, frame key, time)
    suspect: Option<Suspect>,
    /// receivers already found at fault, not yet reported
    blame_due: ([u64; 4], u8),
    /// a decoded position that a place solved elsewhere could not have
    /// led to, waiting for another of the aircraft's messages to agree:
    /// (lat, lon, time, frame key)
    far: Option<(f64, f64, Ms, u64)>,
    /// the message in hand came over a link the operator trusts
    /// (`--net-trusted`); set by the caller before each message
    pub by_trusted: bool,
    /// when a trusted link last supplied or confirmed the track's
    /// position (0: never)
    anchor: u32,
    /// the current position was supplied by a trusted link
    pos_trusted: bool,
    /// receivers that supplied or confirmed the current track's
    /// positions lately: (receiver, time)
    pos_wit: [(u64, u32); 6],
    cpr: [RxCpr; CPR_ROWS],
    /// frames accepted in the last moments: (content key, time, was a position)
    recent: [(u64, u32, bool); RECENT],
    recent_n: u8,
    recent_cpr: [u64; 6],
    recent_i: u8,
    /// last forward per message class on the reduced stream
    pub reduce_last: [Ms; KINDS],
    /// the same for a rate-limited BaseStation output
    pub sbs_last: [Ms; KINDS],
    pub json_port_last: Ms,
    /// receivers that heard this aircraft lately: (receiver, time)
    rx_recent: [(u64, u32); 8],
    /// the receiver that heard it last
    pub last_rx: u64,
    /// course over ground worked out from successive positions, for
    /// aircraft that do not report a track
    pub calc_track: F<f32>,
    /// wind (direction from, speed kt) and temperatures (outside, total
    /// °C), worked out when the inputs were all fresh at once
    pub wind: F<(f32, f32)>,
    /// The barometric altitude the wind was worked out at.
    pub wind_alt: Option<i32>,
    pub temps: F<(f32, f32)>,
    /// last ACAS resolution advisory: (time, the 56-bit report)
    pub acas_ra: Option<(Ms, [u8; 7])>,
    /// consecutive surface position messages
    surface_run: u8,
    /// what the heat map last recorded of its callsign and squawk
    pub heat_tag: u64,
    /// message rate estimate for the binary output
    pub rate: f32,
    pub rate_n: u32,
    pub rate_t: Ms,
    pub db: Option<std::sync::Arc<crate::db::Entry>>,
    pub db_checked: bool,
    pub trace: crate::trace::Trace,
    /// Where the aircraft was before it was forgotten: nothing shown,
    /// only a rough place for surface messages to be read against.
    pub last_place: Option<(f64, f64)>,
}

impl Aircraft {
    /// A position an hour old is no longer shown at all: over 50 minutes
    /// of watching, the oldest lastPosition readsb gave was 3655 s, on
    /// restarted and continuous records alike.
    pub fn fade_position(&mut self, now: Ms) {
        if self.pos.src != Source::Invalid && now.saturating_sub(self.pos.t) > LAST_POSITION_KEPT && self.pos.src != Source::Jaero {
            self.last_place = Some(self.pos.v);
            self.pos.src = Source::Invalid;
        }
    }

    pub fn new(addr: u32, now: Ms) -> Self {
        Aircraft {
            addr,
            addr_type: AddrType::Unknown,
            addr_type_t: 0,
            seen: now,
            checked: now,
            messages: 0,
            sig: [0.0; 8],
            sig_i: 0,
            // the identification message comes every five seconds at best,
            // and far less often through a rate-limited feed
            callsign: F { long: true, ..F::default() },
            alt_baro: F::default(),
            alt_candidate: (0, 0),
            alt_geom: F::default(),
            geom_delta: F::default(),
            gs: F::default(),
            ias: F::default(),
            tas: F::default(),
            mach: F::default(),
            track: F::default(),
            track_rate: F::default(),
            roll: F::default(),
            mag_heading: F::default(),
            true_heading: F::default(),
            baro_rate: F::default(),
            geom_rate: F::default(),
            squawk: F::default(),
            squawk_tentative: u16::MAX,
            sbs_callsign: [0; 8],
            emergency: F::default(),
            category: 0,
            ground: F::default(),
            nav_qnh: F::default(),
            nav_alt_mcp: F::default(),
            nav_alt_fms: F::default(),
            nav_heading: F::default(),
            nav_modes: F::default(),
            alert: F::default(),
            spi: F::default(),
            version: -1,
            adsr_version: -1,
            tisb_version: -1,
            nic_a: false,
            nic_c: false,
            hrd_magnetic: false,
            track_not_heading: true,
            nic_baro: F::default(),
            nac_p: F::default(),
            nac_v: F::default(),
            sil: F::default(),
            sil_type: 255,
            es_seen: 0,
            direct: 0,
            created: now,
            adsb_pos: 0,
            ver_t: 0,
            gps_lost: 0,
            gva: F::default(),
            sda: F::default(),
            pos: F::default(),
            nic: 0,
            rc: 0,
            pos_rx: 0,
            pos_surface: false,
            pos_bad: 0,
            pos_confirmed: 0,
            suspect: None,
            blame_due: ([0; 4], 0),
            far: None,
            by_trusted: false,
            anchor: 0,
            pos_trusted: false,
            pos_wit: [(0, 0); 6],
            cpr: [RxCpr::default(); CPR_ROWS],
            recent: [(0, 0, false); RECENT],
            recent_n: 0,
            recent_cpr: [u64::MAX; 6],
            recent_i: 0,
            reduce_last: [0; KINDS],
            sbs_last: [0; KINDS],
            json_port_last: 0,
            rx_recent: [(0, 0); 8],
            last_rx: 0,
            calc_track: F::default(),
            wind: F::default(),
            wind_alt: None,
            temps: F::default(),
            acas_ra: None,
            surface_run: 0,
            heat_tag: 0,
            rate: 0.0,
            rate_n: 0,
            rate_t: now,
            db: None,
            db_checked: false,
            trace: crate::trace::Trace::default(),
            last_place: None,
        }
    }

    /// Whether this exact frame was accepted from another receiver
    /// within the window. Then nothing in it is news: the aircraft was
    /// heard again, and that is all. Some(position): it is a duplicate,
    /// and whether the original was a position message.
    #[inline]
    pub fn duplicate(&mut self, key: u64, signal: u8, now: Ms, rx: u64, checked: bool) -> Option<bool> {
        let t = now as u32;
        for e in &self.recent {
            if e.0 == key && t.wrapping_sub(e.1) < DUP_WINDOW {
                let pos = e.2;
                self.seen = now;
                self.direct = now;
                if checked {
                    self.checked = now;
                }
                self.messages = self.messages.wrapping_add(1);
                self.signal(signal);
                self.heard_by(rx, now);
                return Some(pos);
            }
        }
        None
    }

    /// A second receiver heard the position message the current
    /// position came from.
    #[inline]
    pub fn confirm_position(&mut self, rx: u64, now: Ms) {
        if rx != self.pos_rx {
            self.pos_confirmed = now;
        }
        if self.by_trusted {
            self.anchor = (now as u32).max(1);
        }
        self.witness(rx, now);
    }

    /// A trusted link stood behind the track in the last half minute.
    /// While it does, no number of other receivers moves the aircraft:
    /// agreement among strangers is the one thing a group of them can
    /// forge, and the operator's own receivers are the one thing they
    /// cannot. Only a trusted link's own contradiction lifts it.
    pub fn anchored(&self, now: Ms) -> bool {
        self.anchor != 0 && (now as u32).wrapping_sub(self.anchor) < ANCHOR_HOLD
    }

    /// Note that `rx` stands behind the current track.
    fn witness(&mut self, rx: u64, now: Ms) {
        let t = now as u32;
        let mut oldest = 0;
        for i in 0..self.pos_wit.len() {
            if self.pos_wit[i].0 == rx {
                self.pos_wit[i].1 = t;
                return;
            }
            if t.wrapping_sub(self.pos_wit[i].1) > t.wrapping_sub(self.pos_wit[oldest].1) {
                oldest = i;
            }
        }
        self.pos_wit[oldest] = (rx, t);
    }

    /// How many receivers have stood behind the current track in the
    /// last ten seconds.
    fn witnesses(&self, now: Ms) -> u8 {
        let t = now as u32;
        self.pos_wit.iter().filter(|w| w.0 != 0 && t.wrapping_sub(w.1) < 10_000).count() as u8
    }

    /// Another receiver heard the suspect message too: the aircraft
    /// really sent it, or these receivers err together. Against an
    /// unconfirmed position it wins at once. Against a confirmed one it
    /// counts only once more receivers relay it than half of those
    /// standing behind the track, and the track moves after a few such
    /// messages: the many are believed over the few.
    #[inline]
    pub fn corroborate_suspect(&mut self, key: u64, rx: u64, now: Ms) -> bool {
        let Some(mut s) = self.suspect else { return false };
        if !self.by_trusted && self.anchored(now) {
            return false;
        }
        if s.key != key || s.wit[..s.n as usize].contains(&rx) {
            return false;
        }
        if (s.n as usize) < s.wit.len() {
            s.wit[s.n as usize] = rx;
            s.n += 1;
        }
        if s.strong {
            if s.n < 2 || (s.n as u16) * 2 <= self.witnesses(now) as u16 {
                self.suspect = Some(s);
                return false;
            }
            self.suspect = None;
            self.pos_bad += 1;
            if self.pos_bad < 3 {
                return false;
            }
        }
        self.suspect = None;
        adopt(self, s.lat, s.lon, &s.cpr, s.src, now, s.rx);
        self.pos_confirmed = now;
        self.pos_wit = [(0, 0); 6];
        for w in &s.wit[..s.n as usize] {
            self.witness(*w, now);
        }
        true
    }

    /// The receivers to hold responsible for a position that
    /// contradicted a confirmed one and that too few others heard, once
    /// the time for others to hear it has passed.
    #[inline]
    pub fn take_blame(&mut self, now: Ms) -> ([u64; 4], u8) {
        if self.blame_due.1 > 0 {
            return std::mem::replace(&mut self.blame_due, ([0; 4], 0));
        }
        match self.suspect {
            Some(s) if (now as u32).wrapping_sub(s.t) > BLAME_AFTER => {
                self.suspect = None;
                if s.strong {
                    (s.wit, s.n)
                } else {
                    ([0; 4], 0)
                }
            }
            _ => ([0; 4], 0),
        }
    }

    /// Remember an accepted frame for `duplicate`.
    #[inline]
    pub fn remember(&mut self, key: u64, now: Ms, position: bool) {
        self.recent[self.recent_n as usize % RECENT] = (key, now as u32, position);
        self.recent_n = self.recent_n.wrapping_add(1);
    }

    /// Note that `rx` heard the aircraft now.
    #[inline]
    pub fn heard_by(&mut self, rx: u64, now: Ms) {
        self.last_rx = rx;
        let t = now as u32;
        let mut oldest = 0;
        for i in 0..self.rx_recent.len() {
            if self.rx_recent[i].0 == rx {
                self.rx_recent[i].1 = t;
                return;
            }
            if t.wrapping_sub(self.rx_recent[i].1) > t.wrapping_sub(self.rx_recent[oldest].1) {
                oldest = i;
            }
        }
        self.rx_recent[oldest] = (rx, t);
    }

    /// How many receivers heard the aircraft in the last few seconds.
    pub fn receiver_count(&self, now: Ms) -> u8 {
        let t = now as u32;
        self.rx_recent.iter().filter(|e| e.0 != 0 && t.wrapping_sub(e.1) < RX_WINDOW).count().max(1) as u8
    }

    /// Signal level in dBFS: the mean power of the last eight messages.
    /// With fewer than four heard there is none to speak of (readsb
    /// reports its floor, -49.5, until the fourth).
    pub fn rssi(&self) -> f32 {
        let n = self.messages.min(8) as f32;
        let mean = if self.messages < 4 { 0.0 } else { self.sig.iter().sum::<f32>() / n };
        10.0 * (mean + 1.125e-5).log10()
    }

    pub fn signal(&mut self, s: u8) {
        let l = s as f32 / 255.0;
        self.sig[self.sig_i as usize & 7] = l * l;
        self.sig_i = self.sig_i.wrapping_add(1);
    }

    /// On the ground, as last reported. The air/ground state outlives
    /// the other fields: an aircraft parked at the edge of coverage is
    /// still on the ground while it is listed.
    pub fn on_ground(&self, now: Ms) -> bool {
        self.ground.src != Source::Invalid && self.ground.v && now.saturating_sub(self.ground.t) <= 3 * FIELD_TTL
    }

    /// True heading: as reported, or the magnetic heading corrected for
    /// the declination where the aircraft is. With the time it is from.
    pub fn true_heading_at(&self, now: Ms) -> Option<(f32, Ms)> {
        if let Some(h) = self.true_heading.get(now) {
            return Some((h, self.true_heading.t));
        }
        None
    }

    /// The wind, while it still describes where the aircraft is. Watched
    /// on readsb over recorded traffic: a wind reading goes after a
    /// minute, or as soon as the aircraft is 500 ft above or below where
    /// it was taken (of 272 readings that went, every one short of a
    /// minute had moved 450 ft or more; those that lasted the minute had
    /// not moved).
    pub fn wind_now(&self, now: Ms) -> Option<(f32, f32)> {
        let w = self.wind.get(now)?;
        if now.saturating_sub(self.wind.t) > 60_000 {
            return None;
        }
        if let (Some(then), Some(alt)) = (self.wind_alt, self.alt_baro.get(now)) {
            if (alt - then).abs() >= 500 {
                return None;
            }
        }
        Some(w)
    }

    /// The ADS-B version to show, if any.
    pub fn version_shown(&self, now: Ms) -> Option<i8> {
        if self.version < 0 {
            return None;
        }
        // a device that is not a transponder is shown its version while its squitters last
        let own = matches!(self.addr_type, AddrType::AdsbIcao | AddrType::ModeS) || self.addr_type_t == 0;
        (own || now.saturating_sub(self.es_seen) <= ES_TTL).then_some(self.version)
    }

    /// Whether the position still counts as current.
    pub fn pos_valid(&self, now: Ms) -> bool {
        self.pos.valid(now)
    }
}

/// What one message did, for the outputs downstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pos {
    None,
    /// A position message already heard from another receiver.
    Duplicate,
    New,
}

pub struct Applied {
    pub pos: Pos,
    pub lat: f64,
    pub lon: f64,
}

/// Great-circle distance in metres.
pub fn dist_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// Initial bearing in degrees from point 1 to point 2.
pub fn bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Apply one decoded Mode S message. `rx_ref`: the receiver's rough
/// location when it is known, the last resort for surface positions.
pub fn apply(a: &mut Aircraft, m: &Msg, signal: u8, now: Ms, rx: u64, rx_ref: Option<(f64, f64)>, key: u64) -> Applied {
    a.seen = now;
    a.direct = now;
    if matches!(m.df, 11 | 17 | 18) {
        a.checked = now;
    }
    a.messages = a.messages.wrapping_add(1);
    a.signal(signal);
    a.heard_by(rx, now);
    let src = m.source;
    if let Some(b) = m.acas_ra {
        a.acas_ra = Some((now, b));
    }
    // One surface message does not put an airborne aircraft on the
    // ground; the second in a row does.
    let surface = m.cpr.map_or(false, |c| c.surface);
    if surface {
        a.surface_run = a.surface_run.saturating_add(1);
    } else if m.cpr.is_some() {
        a.surface_run = 0;
    }
    let lone_surface = surface && a.surface_run < 2 && !(a.ground.v && a.ground.src != Source::Invalid);
    if m.df == 18 {
        // a rebroadcast or a non-transponder device is what it is from
        // its first message; a transponder's own squitters count once
        // they have given a position
        if let Some(t) = m.addr_type {
            a.addr_type = t;
            a.addr_type_t = now;
        }
    }
    if matches!(m.df, 17 | 18) {
        a.es_seen = now;
    }
    if m.df == 17 && m.tc == 0 && m.cpr.is_none() {
        a.gps_lost = now;
    }
    // An ADS-B aircraft is one whose own squitters place it, and it has a
    // 45 s clock, started when its version was set. Probed on readsb with
    // some fifty short histories (`tools/version_probe.py`); nothing
    // lapses by the clock alone, only when a message arrives:
    // - any message 90 s after the last own position finds it a plain
    //   Mode S target again, its version forgotten;
    // - a squitter that finds the clock run out restarts it if the
    //   position is still current, and otherwise does the same as above;
    // - on an aircraft that is not an ADS-B one, any message that finds
    //   the clock run out forgets the version.
    if m.df != 18 {
        let ran_out = now.saturating_sub(a.ver_t) > VERSION_HOLD;
        if a.addr_type == AddrType::AdsbIcao && a.adsb_pos != 0 {
            let pos_age = now.saturating_sub(a.adsb_pos);
            if pos_age > ES_TTL || (m.df == 17 && ran_out && pos_age > FIELD_TTL) {
                a.addr_type = AddrType::ModeS;
                a.version = -1;
            } else if m.df == 17 && ran_out {
                a.ver_t = now;
            }
        } else if a.version >= 0 && ran_out {
            a.version = -1;
        }
    }
    if m.df == 17 {
        own_version(a, m.op.version, now);
    } else if a.version < 0 && matches!(m.addr_type, Some(AddrType::AdsbOther | AddrType::Unknown)) {
        a.version = 0;
        a.ver_t = now;
    }
    if let Some(g) = m.ground {
        // "Airborne" from a capability field does not outweigh surface
        // position messages still arriving; leaving the ground shows as
        // an airborne position message.
        let surface_recent = a.pos_surface && now.saturating_sub(a.pos.t) < 30_000;
        if !lone_surface && (g || m.cpr.is_some() || !(surface_recent && a.ground.v)) {
            a.ground.accept(g, src, now);
        }
    }
    if let Some(v) = m.alt_baro {
        if src >= Source::ModeSChecked {
            a.alt_baro.accept(v, src, now);
        } else {
            surveillance_altitude(a, v, src, now);
        }
    }
    // An air-to-air reply's altitude counts like a surveillance reply's
    // (probed: one alone is not shown, the second matching one is).
    if let Some(v) = m.acas_alt {
        surveillance_altitude(a, v, src, now);
    }
    if let Some(d) = m.geom_delta {
        a.geom_delta.accept(d, src, now);
    }
    if let Some(v) = m.alt_geom {
        a.alt_geom.accept(v, src, now);
    } else if m.geom_delta.is_some() || m.alt_baro.is_some() {
        // geometric altitude follows from the barometric one and the
        // difference the velocity message carries
        // Probed on readsb: a difference arriving after the altitude
        // gives a geometric altitude whatever kind of message the
        // altitude came in (a surveillance reply's too), but only once
        // per altitude: further differences with no new altitude do not
        // keep it alive, and it lapses with the altitude it was made from.
        if let (Some(b), Some(d)) = (a.alt_baro.get(now), a.geom_delta.get(now)) {
            let fresh_alt = !a.alt_geom.valid(now) || a.alt_baro.t >= a.alt_geom.t;
            if if m.alt_baro.is_some() { a.alt_baro.src == src } else { fresh_alt } {
                a.alt_geom.accept(b + d, src, now);
            }
        }
    }
    if let Some(v) = m.gs {
        if !lone_surface {
            a.gs.accept(v, src, now);
        }
    }
    if let Some(v) = m.track {
        a.track.accept(v, src, now);
    }
    match if lone_surface { None } else { m.heading } {
        Some(Heading::Magnetic(h)) => {
            if a.version >= 1 && !a.hrd_magnetic && m.df != 20 && m.df != 21 {
                a.true_heading.accept(h, src, now);
            } else if a.mag_heading.accept(h, src, now) && a.pos.valid(now) {
                // the true heading follows from the magnetic one and
                // the declination where the aircraft is, worked out as
                // the heading arrives and only with a current position
                // (readsb: with a position 50 s old, not 70; a position
                // that comes later does not bring it; tools/wind_probe2.py)
                let d = crate::geomag::lookup(a.pos.v.0, a.pos.v.1);
                a.true_heading.accept((h + d).rem_euclid(360.0), src, now);
            }
        }
        Some(Heading::True(h)) => {
            a.true_heading.accept(h, src, now);
        }
        Some(Heading::Ground(h)) => {
            // on the ground the field is the track unless the aircraft
            // has said it sends its heading there
            if a.track_not_heading {
                a.track.accept(h, src, now);
            } else if a.hrd_magnetic {
                a.mag_heading.accept(h, src, now);
            } else {
                a.true_heading.accept(h, src, now);
            }
        }
        None => {}
    }
    if let Some(v) = m.ias {
        a.ias.accept(v, src, now);
    }
    if let Some(v) = m.tas {
        a.tas.accept(v, src, now);
    }
    if let Some(v) = m.mach {
        a.mach.accept(v, src, now);
    }
    if let Some(v) = m.baro_rate {
        a.baro_rate.accept(v, src, now);
    }
    if let Some(v) = m.geom_rate {
        a.geom_rate.accept(v, src, now);
    }
    if let Some(v) = m.roll {
        a.roll.accept(v, src, now);
    }
    if let Some(v) = m.track_rate {
        a.track_rate.accept(v, src, now);
    }
    if let Some(v) = m.squawk {
        // one reception of a code is not believed: a single corrupted
        // reply would otherwise show as a squawk change (or as 7700)
        if v == a.squawk_tentative || (a.squawk.valid(now) && a.squawk.v == v) {
            a.squawk.accept(v, src, now);
        }
        a.squawk_tentative = v;
    }
    if let Some(v) = m.callsign {
        a.callsign.accept(v, src, now);
    }
    if let Some(c) = m.category {
        a.category = c;
    }
    if let Some(v) = m.emergency {
        a.emergency.accept(v, src, now);
    }
    if let Some(v) = m.alert {
        a.alert.accept(v, src, now);
    }
    if let Some(v) = m.spi {
        a.spi.accept(v, src, now);
    }
    if let Some(v) = m.nac_v {
        a.nac_v.accept(v, src, now);
    }
    if let Some(v) = m.nav_qnh {
        a.nav_qnh.accept(v, src, now);
    }
    if let Some(v) = m.nav_alt_mcp {
        a.nav_alt_mcp.accept(v, src, now);
    }
    if let Some(v) = m.nav_alt_fms {
        a.nav_alt_fms.accept(v, src, now);
    }
    if let Some(v) = m.nav_heading {
        a.nav_heading.accept(v, src, now);
    }
    if let Some(v) = m.nav_modes {
        a.nav_modes.accept(v, src, now);
    }
    let op = &m.op;
    if let Some(v) = op.version {
        // a rebroadcast speaks for the rebroadcasting service's version
        // of the aircraft, kept apart from what the aircraft itself says
        match src {
            Source::Adsr => a.adsr_version = v as i8,
            Source::Tisb => a.tisb_version = v as i8,
            _ if m.df == 17 => {} // taken above
            _ => {
                a.version = v as i8;
                a.ver_t = now;
            }
        }
    }
    if let Some(v) = op.nic_a {
        a.nic_a = v;
    }
    if let Some(v) = op.nic_c {
        a.nic_c = v;
    }
    if let Some(v) = op.hrd_magnetic {
        a.hrd_magnetic = v;
    }
    if let Some(v) = op.track_not_heading {
        a.track_not_heading = v;
    }
    if let Some(v) = op.nac_p {
        a.nac_p.accept(v, src, now);
    }
    if let Some(v) = op.sil {
        a.sil.accept(v, src, now);
    }
    if let Some(v) = op.sil_per_sample {
        a.sil_type = if v { 2 } else { 1 };
    }
    if let Some(v) = op.gva {
        a.gva.accept(v, src, now);
    }
    if let Some(v) = op.sda {
        a.sda.accept(v, src, now);
    }
    if let Some(v) = op.nic_baro {
        a.nic_baro.accept(v, src, now);
    }

    if m.tas.is_some() || m.mach.is_some() || m.heading.is_some() {
        derive_air_data(a, src, now);
    }
    if m.addr_type == Some(AddrType::Unknown) && m.df == 18 {
        // a target of unknown kind: all it yields is the accuracy its
        // type code would stand for
        if let Some((nac_p, sil)) = modes::v0_accuracy(m.tc) {
            a.nac_p.accept(nac_p, src, now);
            a.sil.accept(sil, src, now);
            if a.sil_type == 255 {
                a.sil_type = 0;
            }
        }
    }
    let mut out = Applied { pos: Pos::None, lat: 0.0, lon: 0.0 };
    if let Some(c) = m.cpr {
        // the version that applies is the one stated through this kind of source
        let stated = match src {
            Source::Adsr => a.adsr_version,
            Source::Tisb => a.tisb_version,
            _ => a.version,
        };
        if stated <= 0 {
            if let Some((nac_p, sil)) = modes::v0_accuracy(c.tc) {
                a.nac_p.accept(nac_p, src, now);
                a.sil.accept(sil, src, now);
                if a.sil_type == 255 {
                    a.sil_type = 0;
                }
            }
        }
        out.pos = position(a, &c, src, now, rx, rx_ref, key);
        if m.df == 17 && (out.pos == Pos::New || (out.pos == Pos::Duplicate && a.pos.src == Source::Adsb)) {
            a.adsb_pos = now;
        }
        if out.pos == Pos::New {
            // what kind of source an aircraft is follows its position
            if let Some(t) = m.addr_type {
                a.addr_type = t;
                a.addr_type_t = now;
            }
        }
        if out.pos != Pos::None {
            out.lat = a.pos.v.0;
            out.lon = a.pos.v.1;
        }
    }
    out
}

/// The even and odd position messages one receiver has relayed.
#[derive(Clone, Copy, Default)]
struct RxCpr {
    rx: u64,
    slots: [CprSlot; 2],
    t: Ms,
}

/// A position that contradicts the current one, waiting to see whether
/// a second receiver heard the message it came from.
#[derive(Clone, Copy)]
struct Suspect {
    rx: u64,
    /// every receiver that relayed this same message so far
    wit: [u64; 4],
    n: u8,
    key: u64,
    t: u32,
    lat: f64,
    lon: f64,
    cpr: Cpr,
    src: Source,
    /// the position it contradicts was itself confirmed
    strong: bool,
}

/// Make (lat, lon) the aircraft's position.
fn adopt(a: &mut Aircraft, lat: f64, lon: f64, c: &Cpr, src: Source, now: Ms, rx: u64) {
    let before = (a.pos.src != Source::Invalid).then_some(a.pos.v);
    a.pos.v = (lat, lon);
    a.pos.t = now;
    a.pos.src = src;
    if let Some((plat, plon)) = before {
        let d = dist_m(plat, plon, lat, lon);
        // kept whether or not a track is reported: it is shown when none is
        if d > 40.0 && d < 50_000.0 {
            a.calc_track.accept(bearing(plat, plon, lat, lon) as f32, src, now);
        }
    }
    let (nic, rc) = modes::nic_rc(c, a.version.max(0) as u8, a.nic_a, a.nic_c);
    a.nic = nic;
    a.rc = rc;
    a.pos_rx = rx;
    a.pos_surface = c.surface;
    a.pos_bad = 0;
    a.far = None;
    a.anchor = if a.by_trusted { (now as u32).max(1) } else { 0 };
    a.pos_trusted = a.by_trusted;
    a.witness(rx, now);
}

/// How far the aircraft can have gone in `dt` seconds, in metres: its
/// ground speed with a third to spare (at least 60 kt on the surface,
/// 200 kt in the air), or 150 kt and 900 kt when no speed is known.
fn reach_m(a: &Aircraft, surface: bool, now: Ms, dt: f64) -> f64 {
    let speed_kt = match a.gs.get(now) {
        Some(g) => (g as f64 * 1.3).max(if surface { 60.0 } else { 200.0 }),
        None => {
            if surface {
                150.0
            } else {
                900.0
            }
        }
    };
    speed_kt * 0.5144 * (dt + 1.0) + 150.0
}

/// How far a place solved elsewhere (MLAT, a BaseStation feed, ADS-C)
/// may be from the truth before the aircraft's own word, against it,
/// needs saying twice. MLAT a few kilometres off is ordinary.
const SOLVED_SLACK_M: f64 = 20_000.0;
/// How long a far decode waits for another to agree with it.
const FAR_HOLD: Ms = 10_000;

fn position(a: &mut Aircraft, c: &Cpr, src: Source, now: Ms, rx: u64, rx_ref: Option<(f64, f64)>, frame_key: u64) -> Pos {
    // Both fields zero is a transponder with no fix saying so in a
    // position message. Decoded, a surface pair of them lands on the
    // equator at a multiple of 90 degrees, one alone on the corner of a
    // zone near the last place, and the aircraft's later messages are
    // read against that (seen beside readsb, which showed neither).
    if c.lat == 0 && c.lon == 0 {
        return Pos::None;
    }
    // The same transmission arrives once per receiver that heard it.
    let key = c.lat as u64 | (c.lon as u64) << 17 | (c.odd as u64) << 34 | (c.surface as u64) << 35 | (now / 16_384) << 36;
    let key_prev = key.wrapping_sub(1 << 36);
    if a.recent_cpr.iter().any(|&k| k == key || k == key_prev) {
        if !a.pos_valid(now) {
            return Pos::None;
        }
        // An aircraft standing still says the same thing again and
        // again: it is still there, and its position is as fresh as its
        // last word (readsb's seen_pos stays near zero for one; left to
        // age here, a parked aircraft's place went stale and a solved
        // position from elsewhere took it over).
        if a.pos.src == src {
            a.pos.t = now;
        }
        a.confirm_position(rx, now);
        return Pos::Duplicate;
    }

    // An even and an odd message make a position only when the same
    // receiver relayed both: halves from two sources that disagree
    // would decode to a place neither of them meant. So each receiver
    // (the last few) has its own pair.
    let row = match a.cpr.iter().position(|r| r.rx == rx && r.t != 0) {
        Some(r) => r,
        None => {
            let r = (0..a.cpr.len()).min_by_key(|&r| a.cpr[r].t).unwrap();
            a.cpr[r] = RxCpr { rx, ..Default::default() };
            r
        }
    };
    let i = c.odd as usize;
    a.cpr[row].t = now;
    a.cpr[row].slots[i] = CprSlot { c: Some(*c), t: now };
    let mut other = a.cpr[row].slots[1 - i];
    let window = if c.surface { PAIR_SURFACE } else { pair_airborne(a, now) };
    let have_pos = a.pos.src != Source::Invalid && now.saturating_sub(a.pos.t) < 600_000;
    // An aircraft that is nowhere yet may be placed from halves heard
    // by two receivers, as readsb does: at the edge of coverage each may
    // catch only one. Once it has a place, a receiver's halves pair only
    // with its own, so that no one answers for another's message.
    if !have_pos && !c.surface && !(other.c.is_some() && now.saturating_sub(other.t) <= window) {
        if let Some(o) = a.cpr.iter().map(|r| r.slots[1 - i]).filter(|o| o.c.map_or(false, |x| !x.surface) && now.saturating_sub(o.t) <= window).max_by_key(|o| o.t) {
            other = o;
        }
    }

    let mut fix: Option<(f64, f64)> = None;
    let mut global = false;
    if let Some(o) = other.c {
        if o.surface == c.surface && now.saturating_sub(other.t) <= window {
            let (e, d) = if c.odd { (o, *c) } else { (*c, o) };
            fix = if c.surface {
                // A surface pair is ambiguous by a quarter of the globe and
                // needs a rough place. The aircraft's own last position
                // serves however old it is: seen on a long live run,
                // readsb goes on placing an aircraft that has sat at an
                // airport for an hour and more, on a link whose receiver
                // has no location of its own.
                let known = if a.pos.src != Source::Invalid { Some(a.pos.v) } else { a.last_place };
                let r = if have_pos { Some(a.pos.v) } else { rx_ref.or(known) };
                r.and_then(|(rl, ro)| cpr::global_surface(e.lat, e.lon, d.lat, d.lon, c.odd, rl, ro))
            } else {
                global = true;
                cpr::global_airborne(e.lat, e.lon, d.lat, d.lon, c.odd)
            };
        }
    }
    // One message alone places the aircraft relative to where it last
    // was, as long as it cannot have left that zone since: ten minutes
    // at any airliner's speed is well inside it.
    if fix.is_none() && have_pos {
        global = false;
        fix = cpr::local(c.lat, c.lon, c.odd, c.surface, a.pos.v.0, a.pos.v.1);
    }
    let Some((lat, lon)) = fix else { return Pos::None };

    // A place solved elsewhere gives way to the aircraft's own word (as
    // below), but not to one message that puts it where it cannot have
    // got to. Taken at once, a single bad decode after a BaseStation
    // position moved an airliner 300 km, and was then the reference its
    // next messages were read against. A second message that agrees
    // settles it: then the solved place was the wrong one.
    if a.pos.src != Source::Invalid && a.pos.src < Source::Tisb && now.saturating_sub(a.pos.t) < 120_000 {
        let dt = now.saturating_sub(a.pos.t) as f64 / 1000.0;
        if dist_m(a.pos.v.0, a.pos.v.1, lat, lon) > reach_m(a, c.surface, now, dt) + SOLVED_SLACK_M {
            let agreed = a.far.map_or(false, |(flat, flon, ft, fkey)| {
                let since = now.saturating_sub(ft);
                fkey != key && since <= FAR_HOLD && dist_m(flat, flon, lat, lon) <= reach_m(a, c.surface, now, since as f64 / 1000.0)
            });
            if !agreed {
                a.far = Some((lat, lon, now, key));
                return Pos::None;
            }
        }
    }

    // A position must be reachable from the previous one.
    // (One solved elsewhere, by MLAT or from an ADS-C report, is no
    // measure of that: the aircraft's own word replaces it. Held to a
    // MLAT result two kilometres off, an aircraft that began sending
    // ADS-B was refused, lost its place, and was given the MLAT one
    // back, every few seconds; readsb takes the first ADS-B position.)
    if a.pos.src != Source::Invalid && now.saturating_sub(a.pos.t) < 120_000 && a.pos.src >= Source::Tisb {
        let dt = now.saturating_sub(a.pos.t) as f64 / 1000.0;
        if dist_m(a.pos.v.0, a.pos.v.1, lat, lon) > reach_m(a, c.surface, now, dt) {
            if a.by_trusted && !a.pos_trusted && global {
                // Our own receiver against a place only strangers gave:
                // ours is believed at once, and they answer for it.
                a.suspect = None;
                a.blame_due = ([a.pos_rx, 0, 0, 0], 1);
                adopt(a, lat, lon, c, src, now, rx);
                a.pos_confirmed = 0;
                a.pos_wit = [(0, 0); 6];
                a.witness(rx, now);
                a.recent_cpr[a.recent_i as usize % 6] = key;
                a.recent_i = a.recent_i.wrapping_add(1);
                return Pos::New;
            } else if a.by_trusted {
                // two of our own disagree: the usual rules decide
                a.anchor = 0;
            } else if a.anchored(now) {
                // and whoever says so is at fault
                a.blame_due = ([rx, 0, 0, 0], 1);
                return Pos::None;
            }
            // the same message, late, through another receiver
            if a.corroborate_suspect(frame_key, rx, now) {
                return Pos::New;
            }
            let strong = a.pos_confirmed != 0 && now.saturating_sub(a.pos_confirmed) < 30_000;
            // Whether this is a receiver in error or the aircraft really
            // being elsewhere shows in whether a second receiver heard
            // the same message. Until then nothing moves.
            let waiting = a.suspect.map_or(false, |s| s.key == frame_key);
            // The same receiver contradicting again, its previous message
            // still unheard by anyone else: that one has had its chance.
            if let Some(old) = a.suspect {
                if old.strong && old.wit[..old.n as usize].contains(&rx) && old.key != frame_key && (now as u32).wrapping_sub(old.t) >= DUP_WINDOW {
                    a.blame_due = (old.wit, old.n);
                }
            }
            if (strong || global) && !waiting {
                a.suspect = Some(Suspect { rx, wit: [rx, 0, 0, 0], n: 1, key: frame_key, t: now as u32, lat, lon, cpr: *c, src, strong });
            }
            if strong {
                return Pos::None;
            }
            a.pos_bad += 1;
            if a.pos_bad > 4 {
                // the old position was the wrong one: start over. It is
                // still a better rough place for surface messages than
                // where the aircraft last went quiet, which may be the
                // far end of the flight it has just made.
                a.pos_bad = 0;
                a.last_place = Some(a.pos.v);
                a.pos.src = Source::Invalid;
                a.pos_confirmed = 0;
                a.cpr = [RxCpr::default(); CPR_ROWS];
            }
            return Pos::None;
        }
    }
    if src < a.pos.src && now.saturating_sub(a.pos.t) < STALE {
        return Pos::None;
    }
    adopt(a, lat, lon, c, src, now, rx);
    // only a message that was believed can be confirmed by its repeats
    a.recent_cpr[a.recent_i as usize % 6] = key;
    a.recent_i = a.recent_i.wrapping_add(1);
    Pos::New
}

/// An altitude from a reply addressed by parity alone (DF4, DF20). The
/// address could be a coincidence, so the value must fit what is known
/// or be said twice. Measured on readsb: taken at once while a better
/// altitude is under half a minute old; taken if it agrees with an
/// older one, which it otherwise unseats; and with nothing to compare
/// against, taken on the second matching reply.
/// How long a trusted link's word on a position holds against others.
const ANCHOR_HOLD: u32 = 30_000;

/// How long a version stands on an aircraft that is not placed.
const VERSION_HOLD: Ms = 45_000;

/// The version of an aircraft's own ADS-B, from one of its squitters
/// (`stated` if it is a status message): what is stated, when that
/// differs from what is held (stating it again renews nothing); 0 when
/// none is held.
fn own_version(a: &mut Aircraft, stated: Option<u8>, now: Ms) {
    match stated {
        Some(v) if a.version != v as i8 => {
            a.version = v as i8;
            a.ver_t = now;
        }
        None if a.version < 0 => {
            a.version = 0;
            a.ver_t = now;
        }
        _ => {}
    }
}

fn surveillance_altitude(a: &mut Aircraft, v: i32, src: Source, now: Ms) {
    let near = |x: i32, _since: Ms| (v - x).abs() <= 500;
    if a.alt_baro.valid(now) {
        let age = now.saturating_sub(a.alt_baro.t);
        if a.alt_baro.src <= src || age < 30_000 || near(a.alt_baro.v, a.alt_baro.t) {
            a.alt_baro.v = v;
            a.alt_baro.t = now;
            a.alt_baro.src = src;
            a.alt_candidate = (v, now);
            return;
        }
        // it contradicts an altitude that is no longer fresh: neither stands
        a.alt_baro.src = Source::Invalid;
    }
    if a.alt_candidate.1 != 0 && now.saturating_sub(a.alt_candidate.1) < FIELD_TTL && near(a.alt_candidate.0, a.alt_candidate.1) {
        a.alt_baro.v = v;
        a.alt_baro.t = now;
        a.alt_baro.src = src;
    }
    a.alt_candidate = (v, now);
}

/// Wind and temperature follow from quantities that arrive in different
/// messages; they mean something only when those describe the same
/// moment, in steady flight.
fn derive_air_data(a: &mut Aircraft, src: Source, now: Ms) {
    // measured on readsb: wind appears when airspeed and heading arrive
    // within a second and a quarter of ground speed and track
    // measured again with finer steps (tools/wind_probe2.py): wind up to
    // 1.2 s apart and not at 1.25; temperature up to 2.4 s and not at 2.6
    // The airspeed may be the older of the two: wind when heading comes
    // 2.4 s after airspeed, not when airspeed comes 1.5 s after heading.
    const TOGETHER: Ms = 1240;
    const TEMP_TOGETHER: Ms = 2500;
    let fresh = |t: Ms| now.saturating_sub(t) <= TOGETHER;
    let fresh_t = |t: Ms| now.saturating_sub(t) <= TEMP_TOGETHER;
    let temps = match (a.mach.get(now), a.tas.get(now)) {
        (Some(mach), Some(tas)) if !a.on_ground(now) && fresh_t(a.mach.t) && fresh_t(a.tas.t) && mach >= 0.395 => {
            let k = (tas as f64 / (661.47 * mach as f64)).powi(2) * 288.15;
            let oat = k - 273.15;
            let tat = k * (1.0 + 0.2 * (mach as f64).powi(2)) - 273.15;
            (-100.0..=60.0).contains(&oat).then_some((oat as f32, tat as f32))
        }
        _ => None,
    };
    match temps {
        Some(t) => {
            a.temps.accept(t, src, now);
        }
        None => {}
    }
    let wind = (|| {
        let (tas, gs, trk, (hdg, hdg_t)) = (a.tas.get(now)?, a.gs.get(now)?, a.track.get(now)?, a.true_heading_at(now)?);
        if a.on_ground(now) || !(fresh_t(a.tas.t) && fresh(a.gs.t) && fresh(a.track.t) && fresh(hdg_t) && a.pos.valid(now)) {
            return None;
        }
        // in a turn the air and ground vectors are not comparable:
        // readsb gives wind at 9.8 degrees of roll and not at 10.0
        if a.roll.get(now).map_or(false, |r| r.abs() >= 9.95) {
            return None;
        }
        let (trk, hdg, tas, gs) = ((trk as f64).to_radians(), (hdg as f64).to_radians(), tas as f64, gs as f64);
        let (wx, wy) = (tas * hdg.sin() - gs * trk.sin(), tas * hdg.cos() - gs * trk.cos());
        let ws = (wx * wx + wy * wy).sqrt();
        (ws < 250.0).then(|| (wx.atan2(wy).to_degrees().rem_euclid(360.0) as f32, ws as f32))
    })();
    if let Some(w) = wind {
        if a.wind.accept(w, src, now) {
            a.wind_alt = a.alt_baro.get(now);
        }
    }
}

/// A position that arrives already solved: MLAT, ADS-C over JAERO, a
/// BaseStation feed.
#[derive(Default)]
pub struct External {
    pub source: Source,
    pub addr_type: Option<AddrType>,
    pub pos: Option<(f64, f64)>,
    pub alt_baro: Option<i32>,
    pub alt_geom: Option<i32>,
    pub gs: Option<f32>,
    pub track: Option<f32>,
    pub baro_rate: Option<i32>,
    pub squawk: Option<u16>,
    pub callsign: Option<[u8; 8]>,
    pub ground: Option<bool>,
    pub alert: Option<bool>,
    pub spi: Option<bool>,
    /// When the data was true, if the feed says.
    pub time: Option<Ms>,
}

/// `now` is the time of the data, `wall` the time it arrived.
pub fn apply_external(a: &mut Aircraft, e: &External, now: Ms, wall: Ms) -> Pos {
    a.seen = a.seen.max(if e.source == Source::Jaero { now } else { wall });
    a.checked = a.seen;
    a.messages = a.messages.wrapping_add(1);
    let src = e.source;
    if let Some(v) = e.alt_baro {
        a.alt_baro.accept(v, src, now);
    }
    if let Some(v) = e.alt_geom {
        a.alt_geom.accept(v, src, now);
    }
    if let Some(v) = e.gs {
        a.gs.accept(v, src, now);
    }
    if let Some(v) = e.track {
        a.track.accept(v, src, now);
    }
    if let Some(v) = e.baro_rate {
        a.baro_rate.accept(v, src, now);
    }
    if let Some(v) = e.squawk {
        a.squawk.accept(v, src, now);
    }
    if let Some(v) = e.callsign {
        a.callsign.accept(v, src, now);
    }
    if let Some(v) = e.ground {
        a.ground.accept(v, src, now);
    }
    if let Some(v) = e.alert {
        a.alert.accept(v, src, now);
    }
    if let Some(v) = e.spi {
        a.spi.accept(v, src, now);
    }
    let Some((lat, lon)) = e.pos else { return Pos::None };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Pos::None;
    }
    // An ADS-C report is the last resort: readsb takes one for an
    // aircraft with no place, and for one whose place came from anything
    // else not until that is long gone (tools/src_probe.py: with ADS-B or
    // MLAT silent and JAERO lines every 3 s, the first was taken 302 s
    // and 301 s after the last position; from 90 s on the aircraft was
    // shown as "adsc" with no position).
    if src == Source::Jaero && !matches!(a.pos.src, Source::Invalid | Source::Jaero) && wall.saturating_sub(a.pos.t) < JAERO_AFTER {
        // not placed by it yet, but known by it once nothing else has
        // said what the aircraft is for a minute and a half
        let quiet = wall.saturating_sub(a.addr_type_t) > JAERO_TYPE_TTL && wall.saturating_sub(a.es_seen) > ES_TTL;
        if a.addr_type == AddrType::Jaero || quiet {
            a.addr_type = AddrType::Jaero;
            a.addr_type_t = wall;
        }
        return Pos::None;
    }
    if !a.pos.accept_after((lat, lon), src, now, STALE) {
        return Pos::None;
    }
    a.nic = 0;
    a.rc = 0;
    a.pos_rx = 0;
    a.pos_surface = false;
    if let Some(t) = e.addr_type {
        a.addr_type = t;
        a.addr_type_t = wall;
    }
    Pos::New
}

/// Apply a UAT (978 MHz) ADS-B message. readsb treats these as
/// rebroadcasts (it turns them into extended squitters first), and what
/// it then shows is what is reproduced here: the kind of source by
/// address qualifier, no integrity or accuracy claimed.
pub fn apply_uat(a: &mut Aircraft, u: &modes::uat::Uat, now: Ms) -> Pos {
    let src = if matches!(u.qualifier, 2 | 3) { Source::Tisb } else { Source::Adsr };
    a.seen = now;
    a.direct = now;
    a.checked = now;
    a.messages = a.messages.wrapping_add(1);
    a.es_seen = now;
    a.addr_type = match u.qualifier {
        0 => AddrType::AdsrIcao,
        2 => AddrType::TisbIcao,
        3 => AddrType::TisbTrackfile,
        6 => AddrType::AdsbOther,
        _ => AddrType::AdsrOther,
    };
    a.addr_type_t = now;
    if let Some(db) = u.rssi {
        a.signal((255.0 * 10f32.powf(db / 20.0)).clamp(0.0, 255.0) as u8);
    }
    if u.qualifier == 6 {
        a.version = 0;
    } else {
        if let Some(c) = u.callsign {
            a.callsign.accept(c, src, now);
        }
        if let Some(c) = u.category {
            a.category = c;
        }
    }
    a.ground.accept(u.ground, src, now);
    if let Some(v) = u.alt_baro {
        a.alt_baro.accept(v, src, now);
    }
    if let Some(v) = u.alt_geom {
        a.alt_geom.accept(v, src, now);
    }
    if let Some(v) = u.gs {
        a.gs.accept(v, src, now);
    }
    if let Some(v) = u.track {
        a.track.accept(v, src, now);
    }
    if let Some(v) = u.baro_rate {
        a.baro_rate.accept(v, src, now);
    }
    if let Some(v) = u.geom_rate {
        a.geom_rate.accept(v, src, now);
    }
    if let Some(v) = u.squawk {
        a.squawk.accept(v, src, now);
    }
    if let Some(v) = u.emergency {
        a.emergency.accept(v, src, now);
    }
    a.nac_p.accept(0, src, now);
    a.sil.accept(0, src, now);
    if a.sil_type == 255 {
        a.sil_type = 0;
    }
    if !u.ground {
        a.nac_v.accept(0, src, now);
        a.alert.accept(false, src, now);
        a.spi.accept(false, src, now);
    }
    let Some((lat, lon)) = u.pos else { return Pos::None };
    if src < a.pos.src && now.saturating_sub(a.pos.t) < STALE {
        return Pos::None;
    }
    a.pos.v = (lat, lon);
    a.pos.t = now;
    a.pos.src = src;
    a.nic = 0;
    a.rc = 0;
    a.pos_rx = 0;
    a.pos_surface = u.ground;
    Pos::New
}

#[derive(Default)]
pub struct Addr(u64);
impl Hasher for Addr {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {}
    fn write_u32(&mut self, n: u32) {
        self.0 = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
pub type Map = HashMap<u32, Box<Aircraft>, BuildHasherDefault<Addr>>;

/// What is kept of an aircraft that has gone silent: its trace, packed
/// away, and where it last was. A day's traffic is mostly these, so they
/// are kept apart from the aircraft being tracked, at a fraction of the
/// size, and are not walked when the outputs are written.
pub struct Gone {
    pub seen: Ms,
    pub pos: F<(f64, f64)>,
    pub nic: u8,
    pub rc: u32,
    pub last_place: Option<(f64, f64)>,
    pub heat_tag: u64,
    pub trace: crate::trace::Trace,
}

#[derive(Default)]
pub struct Shard {
    pub map: Map,
    pub gone: HashMap<u32, Gone, BuildHasherDefault<Addr>>,
    pub messages: u64,
}

impl Shard {
    /// An aircraft heard again after being forgotten starts afresh, as
    /// readsb has it, except for its trace and where it last was (which
    /// readsb goes on showing as lastPosition on the new record, up to an
    /// hour old). Call before looking an address up for a message.
    #[inline]
    pub fn wake(&mut self, addr: u32, now: Ms) {
        if self.gone.is_empty() {
            return;
        }
        if let Some(g) = self.gone.remove(&addr) {
            let mut a = Box::new(Aircraft::new(addr, g.seen));
            a.messages = 0;
            a.pos = g.pos;
            a.nic = g.nic;
            a.rc = g.rc;
            a.last_place = g.last_place;
            a.heat_tag = g.heat_tag;
            a.trace = g.trace;
            a.fade_position(now);
            self.map.insert(addr, a);
        }
    }
}

pub const SHARDS: usize = 64;

pub struct Store {
    shards: Vec<Mutex<Shard>>,
    /// aircraft in the table, counted at the last sweep plus those added since
    pub count: std::sync::atomic::AtomicUsize,
}

/// Silence after which an aircraft is forgotten: with a position, without.
const FORGET_PLACED: Ms = 960_000;
const FORGET: Ms = 300_000;
const LAST_POSITION_KEPT: Ms = 3_600_000;

impl Store {
    pub fn new() -> Self {
        Store { shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(), count: std::sync::atomic::AtomicUsize::new(0) }
    }
    #[inline]
    pub fn shard(&self, addr: u32) -> &Mutex<Shard> {
        let h = (addr as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
        &self.shards[h as usize % SHARDS]
    }
    pub fn known(&self, addr: u32) -> bool {
        self.shard(addr).lock().map.contains_key(&addr)
    }
    pub fn shards(&self) -> &[Mutex<Shard>] {
        &self.shards
    }
    pub fn messages(&self) -> u64 {
        self.shards.iter().map(|s| s.lock().messages).sum()
    }
    /// Forget aircraft only `rx` ever reported: called when that
    /// receiver turns out to have been inventing traffic.
    pub fn purge_receiver(&self, rx: u64) -> usize {
        let mut n = 0;
        for s in &self.shards {
            let mut s = s.lock();
            let before = s.map.len();
            s.map.retain(|_, a| !(a.last_rx == rx && a.rx_recent.iter().all(|e| e.0 == rx || e.0 == 0) && a.pos_confirmed == 0));
            n += before - s.map.len();
        }
        self.count.fetch_sub(n.min(self.count.load(std::sync::atomic::Ordering::Relaxed)), std::sync::atomic::Ordering::Relaxed);
        n
    }

    /// Drop aircraft not heard for `ttl`. One with a recorded track
    /// stays for `keep_trace` so its history can still be served, with
    /// points older than that trimmed; one with a JAERO position stays
    /// for that position's longer lifetime.
    pub fn expire(&self, now: Ms, ttl: Ms, keep_trace: Ms) -> usize {
        let mut left = 0;
        for s in &self.shards {
            let mut s = s.lock();
            s.map.retain(|_, a| {
                let idle = now.saturating_sub(a.seen);
                if idle <= ttl || (a.pos.src == Source::Jaero && a.pos.valid(now)) {
                    return true;
                }
                !a.trace.is_empty() && idle <= keep_trace
            });
            // An aircraft silent for a while is forgotten, though its trace
            // stays to be served: if it returns it starts afresh. Watched
            // on readsb over 45 minutes of live traffic: records came back
            // intact after up to 17 minutes of silence when the aircraft
            // had a position and were new from 16 minutes on; without a
            // position, new from 5 to 12 minutes on.
            let s = &mut *s;
            let forget: Vec<u32> = s
                .map
                .values()
                .filter(|a| {
                    let idle = now.saturating_sub(a.seen);
                    let had_pos = a.pos.src != Source::Invalid;
                    idle > if had_pos { FORGET_PLACED } else { FORGET } && !(a.pos.src == Source::Jaero && a.pos.valid(now))
                })
                .map(|a| a.addr)
                .collect();
            for addr in forget {
                let Some(mut a) = s.map.remove(&addr) else { continue };
                if a.trace.is_empty() {
                    continue;
                }
                a.trace.compact(0);
                s.gone.insert(addr, Gone { seen: a.seen, pos: a.pos, nic: a.nic, rc: a.rc, last_place: a.last_place, heat_tag: a.heat_tag, trace: std::mem::take(&mut a.trace) });
            }
            s.gone.retain(|_, g| now.saturating_sub(g.seen) <= keep_trace && !g.trace.is_empty());
            // Points past the limit go.
            let cut = now.saturating_sub(keep_trace);
            for a in s.map.values_mut() {
                a.fade_position(now);
                a.trace.trim_before(cut);
            }
            for g in s.gone.values_mut() {
                g.trace.trim_before(cut);
            }
            s.gone.retain(|_, g| !g.trace.is_empty());
            left += s.map.len() + s.gone.len();
        }
        self.count.store(left, std::sync::atomic::Ordering::Relaxed);
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modes::encode as enc;

    const T0: Ms = 1_800_000_000_000;

    fn feed(a: &mut Aircraft, frame: &[u8], now: Ms) -> Applied {
        let m = modes::decode(frame, |_| true).expect("frame decodes");
        apply(a, &m, 100, now, 1, None, 0)
    }

    #[test]
    fn an_even_odd_pair_gives_a_position() {
        let mut a = Aircraft::new(0x4CA2D6, T0);
        let r = feed(&mut a, &enc::airborne_position(0x4CA2D6, 45.5, 9.25, 35000, false, 11), T0);
        assert_eq!(r.pos, Pos::None, "one message alone is ambiguous");
        let r = feed(&mut a, &enc::airborne_position(0x4CA2D6, 45.5, 9.25, 35000, true, 11), T0 + 500);
        assert_eq!(r.pos, Pos::New);
        assert!((r.lat - 45.5).abs() < 1e-4 && (r.lon - 9.25).abs() < 1e-4);
        assert_eq!(a.alt_baro.get(T0 + 500), Some(35000));
        assert_eq!((a.nic, a.rc), (8, 186));
        // the same message through a second receiver is not a new position
        let r = feed(&mut a, &enc::airborne_position(0x4CA2D6, 45.5, 9.25, 35000, true, 11), T0 + 600);
        assert_eq!(r.pos, Pos::Duplicate);
    }

    #[test]
    fn a_stale_pair_is_not_combined() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0);
        let r = feed(&mut a, &enc::airborne_position(1, 45.9, 9.9, 35000, true, 11), T0 + 17_000);
        assert_eq!(r.pos, Pos::None);
    }

    #[test]
    fn an_impossible_jump_is_rejected_then_believed() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::velocity(1, 450.0, 90.0, 0, None), T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400);
        // 300 km away a second later
        let mut t = T0 + 1400;
        let mut accepted = 0;
        for i in 0..8 {
            let r = feed(&mut a, &enc::airborne_position(1, 48.0 + i as f64 * 1e-4, 9.25, 35000, i % 2 == 1, 11), t);
            accepted += (r.pos == Pos::New) as u32;
            if i < 3 {
                assert_eq!(r.pos, Pos::None, "message {i} must not move the aircraft");
                assert!((a.pos.v.0 - 45.5).abs() < 1e-3);
            }
            t += 500;
        }
        // persistent evidence wins in the end
        assert!(accepted > 0 && (a.pos.v.0 - 48.0).abs() < 0.01);
    }

    #[test]
    fn mlat_does_not_override_fresh_adsb() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400);
        let mlat = External { source: Source::Mlat, pos: Some((45.6, 9.3)), addr_type: Some(modes::AddrType::Mlat), ..Default::default() };
        assert_eq!(apply_external(&mut a, &mlat, T0 + 2000, T0 + 2000), Pos::None);
        assert!((a.pos.v.0 - 45.5).abs() < 1e-3);
        // once ADS-B has been silent for a while, MLAT takes over
        assert_eq!(apply_external(&mut a, &mlat, T0 + 20_000, T0 + 20_000), Pos::New);
        assert_eq!(a.pos.src, Source::Mlat);
        assert_eq!(crate::json::type_str(&a, T0 + 20_000), "mlat");
    }

    #[test]
    fn its_own_position_replaces_a_solved_one_at_once() {
        // placed by MLAT a few kilometres from where it says it is
        let mut a = Aircraft::new(1, T0);
        let mlat = External { source: Source::Mlat, pos: Some((45.53, 9.25)), addr_type: Some(modes::AddrType::Mlat), ..Default::default() };
        assert_eq!(apply_external(&mut a, &mlat, T0, T0), Pos::New);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0 + 1000);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 1400);
        assert_eq!(a.pos.src, Source::Adsb);
        assert!((a.pos.v.0 - 45.5).abs() < 1e-3);
        assert_eq!(crate::json::type_str(&a, T0 + 1400), "adsb_icao");
    }

    #[test]
    fn one_message_cannot_carry_an_aircraft_off_from_a_solved_place() {
        // Seen beside readsb: an airliner placed by a BaseStation feed at
        // 37.03 -112.36, then put 300 km away by a single ADS-B decode,
        // which readsb did not show.
        let place = (37.03, -112.36);
        let far = (39.125, -110.145);
        let sbs = |p| External { source: Source::Sbs, pos: Some(p), ..Default::default() };
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::velocity(1, 424.0, 252.0, 0, None), T0);
        assert_eq!(apply_external(&mut a, &sbs(place), T0, T0), Pos::New);
        let m = modes::decode(&enc::airborne_position(1, far.0, far.1, 30000, false, 11), |_| true).unwrap();
        assert_eq!(apply(&mut a, &m, 100, T0 + 1000, 2, None, 0).pos, Pos::None);
        assert_eq!(a.pos.src, Source::Sbs);
        assert!((a.pos.v.0 - place.0).abs() < 1e-6);
        // its own messages from where the feed had it are taken at once
        let r = feed(&mut a, &enc::airborne_position(1, place.0, place.1 - 0.005, 30000, true, 11), T0 + 1500);
        assert_eq!(r.pos, Pos::New);
        assert_eq!(a.pos.src, Source::Adsb);
        assert!((a.pos.v.0 - place.0).abs() < 1e-3);

        // Two of its own messages that agree move it however far: then
        // the solved place was the wrong one.
        let mut b = Aircraft::new(2, T0);
        assert_eq!(apply_external(&mut b, &sbs(place), T0, T0), Pos::New);
        assert_eq!(feed(&mut b, &enc::airborne_position(2, far.0, far.1, 30000, false, 11), T0 + 1000).pos, Pos::None);
        let r = feed(&mut b, &enc::airborne_position(2, far.0, far.1 - 0.002, 30000, true, 11), T0 + 1500);
        assert_eq!(r.pos, Pos::New);
        assert!((b.pos.v.0 - far.0).abs() < 1e-3 && (b.pos.v.1 - far.1).abs() < 1e-2);
    }

    #[test]
    fn a_position_message_of_zeros_places_nothing() {
        // A transponder with no fix may still send position messages,
        // both fields zero. As a surface pair they decode to the equator
        // on a quarter meridian: beside readsb, (0, -90) for an aircraft
        // at a New Jersey airport and (0, -180) for one at Anchorage, and
        // one alone, read against the place before, to a zone corner.
        let newark = (40.692, -74.165);
        let mut a = Aircraft::new(1, T0);
        let sbs = External { source: Source::Sbs, pos: Some(newark), ..Default::default() };
        assert_eq!(apply_external(&mut a, &sbs, T0, T0), Pos::New);
        for i in 0..4u64 {
            let r = feed(&mut a, &enc::surface_position(1, 0.0, 0.0, 0.0, 0.0, i % 2 == 1), T0 + 500 * (i + 1));
            assert_eq!(r.pos, Pos::None, "message {i}");
        }
        assert_eq!(a.pos.v, newark);
        // nor with only the receiver's location to go by
        let mut b = Aircraft::new(2, T0);
        for i in 0..2u64 {
            let m = modes::decode(&enc::surface_position(2, 0.0, 0.0, 0.0, 0.0, i == 1), |_| true).unwrap();
            assert_eq!(apply(&mut b, &m, 100, T0 + 500 * i, 1, Some((61.2, -150.0)), 0).pos, Pos::None);
        }
        // airborne: a pair of zeros would be (0, 0)
        let mut c = Aircraft::new(3, T0);
        feed(&mut c, &enc::airborne_position(3, 0.0, 0.0, 1000, false, 11), T0);
        assert_eq!(feed(&mut c, &enc::airborne_position(3, 0.0, 0.0, 1000, true, 11), T0 + 500).pos, Pos::None);
        assert_eq!(c.pos.src, Source::Invalid);
        // a real place that merely has a zero field still decodes
        let mut d = Aircraft::new(4, T0);
        feed(&mut d, &enc::airborne_position(4, 0.0, 9.25, 1000, false, 11), T0);
        assert_eq!(feed(&mut d, &enc::airborne_position(4, 0.0, 9.25, 1000, true, 11), T0 + 500).pos, Pos::New);
    }

    #[test]
    fn an_adsc_report_does_not_displace_a_live_track() {
        let jaero = External { source: Source::Jaero, pos: Some((45.52, 9.25)), addr_type: Some(modes::AddrType::Jaero), ..Default::default() };
        // over ADS-B, fresh or a minute stale
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400);
        for dt in [2_000, 20_000, 100_000] {
            assert_eq!(apply_external(&mut a, &jaero, T0 + dt, T0 + dt), Pos::None);
        }
        assert_eq!(a.pos.src, Source::Adsb);
        // over MLAT
        let mut a = Aircraft::new(2, T0);
        let mlat = External { source: Source::Mlat, pos: Some((45.5, 9.25)), addr_type: Some(modes::AddrType::Mlat), ..Default::default() };
        assert_eq!(apply_external(&mut a, &mlat, T0, T0), Pos::New);
        assert_eq!(apply_external(&mut a, &jaero, T0 + 1000, T0 + 1000), Pos::None);
        assert_eq!(crate::json::type_str(&a, T0 + 1000), "mlat");
        // an aircraft with no place takes it, and MLAT takes over from it
        let mut a = Aircraft::new(3, T0);
        assert_eq!(apply_external(&mut a, &jaero, T0, T0), Pos::New);
        assert_eq!(crate::json::type_str(&a, T0 + 1000), "adsc");
        assert_eq!(apply_external(&mut a, &mlat, T0 + 16_000, T0 + 16_000), Pos::New);
        assert_eq!(crate::json::type_str(&a, T0 + 16_000), "mlat");
    }

    #[test]
    fn a_solved_aircraft_also_heard_directly_goes_back_to_mode_s() {
        let mut a = Aircraft::new(1, T0);
        let jaero = External { source: Source::Jaero, pos: Some((45.52, 9.25)), addr_type: Some(modes::AddrType::Jaero), ..Default::default() };
        assert_eq!(apply_external(&mut a, &jaero, T0, T0), Pos::New);
        // heard no other way: ADS-C for as long as the position stands
        assert_eq!(crate::json::type_str(&a, T0 + 600_000), "adsc");
        // heard directly: Mode S again a minute and a half on, the position kept
        a.direct = T0 + 100_000;
        assert_eq!(crate::json::type_str(&a, T0 + 80_000), "adsc");
        assert_eq!(crate::json::type_str(&a, T0 + 100_000), "mode_s");
        assert!(a.pos.valid(T0 + 100_000));
    }

    #[test]
    fn an_aircraft_standing_still_keeps_a_fresh_position() {
        let mut a = Aircraft::new(1, T0);
        for i in 0..30u64 {
            feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, i % 2 == 1, 11), T0 + i * 1000);
        }
        assert_eq!(a.pos.src, Source::Adsb);
        assert!(T0 + 29_000 - a.pos.t <= 1000, "position {} ms old", T0 + 29_000 - a.pos.t);
    }

    #[test]
    fn surface_messages_keep_an_aircraft_on_the_ground() {
        let mut a = Aircraft::new(1, T0);
        let m = modes::decode(&enc::surface_position(1, 45.5, 9.25, 12.0, 90.0, false), |_| true).unwrap();
        apply(&mut a, &m, 100, T0, 1, Some((45.6, 9.3)), 0);
        let m = modes::decode(&enc::surface_position(1, 45.5, 9.25, 12.0, 90.0, true), |_| true).unwrap();
        let r = apply(&mut a, &m, 100, T0 + 500, 1, Some((45.6, 9.3)), 0);
        assert_eq!(r.pos, Pos::New);
        assert_eq!(a.receiver_count(T0 + 500), 1);
        assert!((r.lat - 45.5).abs() < 1e-4 && (r.lon - 9.25).abs() < 1e-4);
        assert!(a.on_ground(T0 + 500));
        // an identification message with "airborne" capability changes nothing
        feed(&mut a, &enc::ident(1, b"TEST123 ", 0xA3), T0 + 700);
        assert!(a.on_ground(T0 + 700));
        // an airborne position does
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 500, false, 11), T0 + 1200);
        assert!(!a.on_ground(T0 + 1200));
    }

    #[test]
    fn a_landing_aircraft_is_read_against_where_it_just_was() {
        // Seen beside readsb: airliners back from long flights landed, were
        // refused on the ground and started over, and were then read
        // against where they had stood hours before at the far end, one a
        // quarter of the globe away in latitude, one in longitude. readsb
        // kept them at the airport.
        let mut a = Aircraft::new(1, T0);
        a.last_place = Some((7.18, 79.89));
        feed(&mut a, &enc::airborne_position(1, 55.95, 37.40, 1000, false, 11), T0);
        feed(&mut a, &enc::airborne_position(1, 55.95, 37.40, 1000, true, 11), T0 + 500);
        assert!(a.pos_valid(T0 + 500));
        // on the ground, out of reach of that last fix until it starts over
        let mut t = T0 + 1500;
        for i in 0..12 {
            feed(&mut a, &enc::surface_position(1, 56.10, 37.40, 10.0, 0.0, i % 2 == 1), t);
            t += 500;
        }
        assert!(a.pos_valid(t) && (a.pos.v.0 - 56.10).abs() < 1e-3 && (a.pos.v.1 - 37.40).abs() < 1e-3, "placed at {:?}", a.pos.v);
    }

    #[test]
    fn a_parked_aircraft_is_placed_by_where_it_last_was() {
        // no receiver location at all: only the aircraft's own history
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 500, false, 11), T0);
        feed(&mut a, &enc::airborne_position(1, 45.5, 9.25, 500, true, 11), T0 + 500);
        assert!(a.pos_valid(T0 + 500));
        // an hour later it moves on the ground, a little way off
        let t = T0 + 3_600_000;
        feed(&mut a, &enc::surface_position(1, 45.51, 9.26, 12.0, 90.0, false), t);
        let r = feed(&mut a, &enc::surface_position(1, 45.51, 9.26, 12.0, 90.0, true), t + 500);
        assert_eq!(r.pos, Pos::New);
        assert!((r.lat - 45.51).abs() < 1e-4 && (r.lon - 9.26).abs() < 1e-4);
        // an aircraft never placed has nothing to go by
        let mut b = Aircraft::new(2, T0);
        feed(&mut b, &enc::surface_position(2, 45.51, 9.26, 12.0, 90.0, false), t);
        let r = feed(&mut b, &enc::surface_position(2, 45.51, 9.26, 12.0, 90.0, true), t + 500);
        assert_eq!(r.pos, Pos::None);
    }

    #[test]
    fn a_silent_aircraft_is_forgotten_but_its_trace_and_place_stay() {
        let st = Store::new();
        {
            let mut sh = st.shard(1).lock();
            let a = sh.map.entry(1).or_insert_with(|| Box::new(Aircraft::new(1, T0)));
            feed(a, &enc::airborne_position(1, 45.5, 9.25, 500, false, 11), T0);
            feed(a, &enc::airborne_position(1, 45.5, 9.25, 500, true, 11), T0 + 500);
            a.trace.points.push(crate::trace::Packed::pack(&crate::trace::Point { t: T0, lat: 45_500_000, lon: 9_250_000, alt: 500, alt_geom: crate::trace::NONE, gs: 0, track: 0, rate: 0, geom_rate: 0, ias: 0, flags: 0, addr_type: 0 }));
        }
        // ten minutes: still itself
        st.expire(T0 + 600_000, 300_000, 86_400_000);
        assert!(st.shard(1).lock().map[&1].pos.src != Source::Invalid);
        // twenty: a new record, which still says where it last was
        assert_eq!(st.expire(T0 + 1_200_000, 300_000, 86_400_000), 1);
        {
            let sh = st.shard(1).lock();
            let g = &sh.gone[&1];
            assert!(sh.map.is_empty() && g.pos.src != Source::Invalid && g.trace.len() == 1);
        }
        // past the hour that is gone too, kept only as a rough place
        let t = T0 + 3_700_000;
        st.expire(t, 300_000, 86_400_000);
        let mut sh = st.shard(1).lock();
        sh.wake(1, t);
        let a = sh.map.get_mut(&1).unwrap();
        assert!(a.pos.src == Source::Invalid && a.last_place.is_some() && a.messages == 0 && a.trace.len() == 1);
        feed(a, &enc::surface_position(1, 45.51, 9.26, 12.0, 90.0, false), t);
        let r = feed(a, &enc::surface_position(1, 45.51, 9.26, 12.0, 90.0, true), t + 500);
        assert_eq!(r.pos, Pos::New);
        assert!((r.lat - 45.51).abs() < 1e-4);
    }

    #[test]
    fn one_receiver_cannot_move_a_confirmed_aircraft() {
        let mut a = Aircraft::new(1, T0);
        let feed_rx = |a: &mut Aircraft, f: &[u8], now: Ms, rx: u64, key: u64| {
            let m = modes::decode(f, |_| true).unwrap();
            apply(a, &m, 100, now, rx, None, key)
        };
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0, 1, 10);
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400, 1, 11);
        // receiver 2 hears the same transmission
        a.confirm_position(2, T0 + 450);
        // receiver 3 alone insists on a place 280 km away, again and again
        let mut t = T0 + 1000;
        for i in 0..20u64 {
            let r = feed_rx(&mut a, &enc::airborne_position(1, 48.0 + i as f64 * 1e-4, 9.25, 35000, i % 2 == 1, 11), t, 3, 100 + i);
            assert_eq!(r.pos, Pos::None);
            a.confirm_position(2, t); // the honest receivers keep agreeing
            t += 500;
            // and each time it is receiver 3 that is held responsible
            if i > 0 {
                assert_eq!(a.take_blame(t), ([3, 0, 0, 0], 1));
            }
        }
        assert!((a.pos.v.0 - 45.5).abs() < 1e-3);

        // but when a second receiver hears those same messages, the
        // aircraft itself is sending them: nobody is blamed, and after a
        // few it is believed
        let mut b = Aircraft::new(2, T0);
        feed_rx(&mut b, &enc::airborne_position(2, 45.5, 9.25, 35000, false, 11), T0, 1, 10);
        feed_rx(&mut b, &enc::airborne_position(2, 45.5, 9.25, 35000, true, 11), T0 + 400, 1, 11);
        b.confirm_position(2, T0 + 450);
        let mut t = T0 + 1000;
        for i in 0..8u64 {
            feed_rx(&mut b, &enc::airborne_position(2, 48.0 + i as f64 * 1e-4, 9.25, 35000, i % 2 == 1, 11), t, 3, 200 + i);
            b.corroborate_suspect(200 + i, 4, t);
            assert_eq!(b.take_blame(t + 3000).1, 0);
            t += 500;
        }
        assert!((b.pos.v.0 - 48.0).abs() < 0.01, "{}", b.pos.v.0);
    }

    #[test]
    fn the_many_are_believed_over_the_few() {
        let feed_rx = |a: &mut Aircraft, f: &[u8], now: Ms, rx: u64, key: u64| {
            let m = modes::decode(f, |_| true).unwrap();
            apply(a, &m, 100, now, rx, None, key)
        };
        let mut a = Aircraft::new(1, T0);
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0, 1, 10);
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400, 1, 11);
        let mut t = T0 + 1000;
        for i in 0..20u64 {
            // five receivers agree where it is
            for rx in 2..=5 {
                a.confirm_position(rx, t);
            }
            // two receivers relay the same false message, again and again
            let r = feed_rx(&mut a, &enc::airborne_position(1, 48.0 + i as f64 * 1e-4, 9.25, 35000, i % 2 == 1, 11), t, 8, 100 + i);
            assert_eq!(r.pos, Pos::None);
            assert!(!a.corroborate_suspect(100 + i, 9, t + 50), "two against five do not move it");
            t += 500;
        }
        assert!((a.pos.v.0 - 45.5).abs() < 1e-3);
        // and both are held responsible
        let (who, n) = a.take_blame(t + 5000);
        assert_eq!((n, who[0], who[1]), (2, 8, 9));
    }

    #[test]
    fn a_trusted_link_holds_a_track_against_any_number() {
        let feed_rx = |a: &mut Aircraft, f: &[u8], now: Ms, rx: u64, key: u64| {
            let m = modes::decode(f, |_| true).unwrap();
            apply(a, &m, 100, now, rx, None, key)
        };
        let mut a = Aircraft::new(1, T0);
        a.by_trusted = true;
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, false, 11), T0, 1, 1);
        feed_rx(&mut a, &enc::airborne_position(1, 45.5, 9.25, 35000, true, 11), T0 + 400, 1, 2);
        assert!(a.anchored(T0 + 400));
        // one trusted receiver, then five strangers telling the same lie
        // in the same messages, for twenty seconds
        a.by_trusted = false;
        let mut t = T0 + 1000;
        for i in 0..40u64 {
            let lie = enc::airborne_position(1, 48.0 + i as f64 * 1e-4, 9.25, 35000, i % 2 == 1, 11);
            assert_eq!(feed_rx(&mut a, &lie, t, 10, 100 + i).pos, Pos::None);
            for rx in 11..15 {
                assert!(!a.corroborate_suspect(100 + i, rx, t + 20));
            }
            t += 500;
        }
        assert!((a.pos.v.0 - 45.5).abs() < 1e-3, "it has not moved");
        // without the trusted link's word for half a minute, the many decide again
        assert!(!a.anchored(T0 + 400 + 31_000));
    }

    #[test]
    fn frames_repeat_only_within_the_window() {
        let mut a = Aircraft::new(1, T0);
        a.remember(42, T0, true);
        assert_eq!(a.duplicate(42, 10, T0 + 100, 7, true), Some(true));
        assert_eq!(a.duplicate(43, 10, T0 + 100, 7, true), None);
        assert_eq!(a.duplicate(42, 10, T0 + 500, 7, true), None, "a later repeat is a new transmission");
        assert_eq!(a.messages, 1);
    }

    #[test]
    fn fields_expire() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::ident(1, b"TEST123 ", 0xA3), T0);
        feed(&mut a, &enc::velocity(1, 450.0, 90.0, 0, None), T0);
        assert!(a.gs.get(T0 + 59_000).is_some());
        assert!(a.gs.get(T0 + 63_000).is_none());
        // the identification comes rarely and is kept three minutes
        assert!(a.callsign.get(T0 + 179_000).is_some());
        assert!(a.callsign.get(T0 + 187_000).is_none());
    }

    #[test]
    fn a_lesser_source_waits_a_quarter_minute() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::target_state(1, 38_016, 1013.6, 90.0, 0), T0);
        // version unknown: still, the selected altitude is taken
        assert_eq!(a.nav_alt_mcp.get(T0), Some(38_016));
        let commb = enc::long_ap(20, modes::alt::ac13_encode(38_000), 1, enc::bds40(38_000, 38_000, 1013.2));
        feed(&mut a, &commb, T0 + 14_000);
        assert_eq!(a.nav_alt_mcp.get(T0 + 14_000), Some(38_016), "ADS-B said so 14 s ago");
        feed(&mut a, &commb, T0 + 35_000);
        assert_eq!(a.nav_alt_mcp.get(T0 + 35_000), Some(38_000), "ADS-B silent for 21 s: Comm-B counts");
    }

    #[test]
    fn surveillance_altitudes_need_backing() {
        // each case as measured on readsb
        let df4 = |alt: i32| enc::df4(1, alt, false);
        let adsb = |a: &mut Aircraft, t: Ms| {
            feed(a, &enc::airborne_position(1, 45.5, 9.25, 36000, false, 11), t);
            feed(a, &enc::airborne_position(1, 45.5, 9.25, 36000, true, 11), t + 100);
        };
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &df4(34000), T0);
        assert_eq!(a.alt_baro.get(T0), None, "one reply, nothing known");
        feed(&mut a, &df4(34000), T0 + 300);
        assert_eq!(a.alt_baro.get(T0 + 300), Some(34000), "said twice");

        let mut a = Aircraft::new(1, T0);
        adsb(&mut a, T0);
        feed(&mut a, &df4(34000), T0 + 20_000);
        assert_eq!(a.alt_baro.get(T0 + 20_000), Some(34000), "ADS-B 20 s ago: taken");

        let mut a = Aircraft::new(1, T0);
        adsb(&mut a, T0);
        feed(&mut a, &df4(36000), T0 + 45_000);
        assert_eq!(a.alt_baro.get(T0 + 45_000), Some(36000), "agrees with ADS-B 45 s ago");

        let mut a = Aircraft::new(1, T0);
        adsb(&mut a, T0);
        feed(&mut a, &df4(34000), T0 + 45_000);
        assert_eq!(a.alt_baro.get(T0 + 45_000), None, "contradicts ADS-B 45 s ago: neither stands");

        let mut a = Aircraft::new(1, T0);
        adsb(&mut a, T0);
        feed(&mut a, &df4(36000), T0 + 80_000);
        assert_eq!(a.alt_baro.get(T0 + 80_000), None, "ADS-B altitude expired: one reply is not enough");
        feed(&mut a, &df4(36000), T0 + 80_300);
        assert_eq!(a.alt_baro.get(T0 + 80_300), Some(36000));
    }

    #[test]
    fn version_and_kind_follow_the_probed_rules() {
        let shown = |a: &Aircraft, t: Ms| (a.version_shown(t), crate::json::type_str(a, t));
        // never located: a Mode S target whose stated version holds 45 s,
        // stated again or not, until a message finds it that old
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::op_status(1, 10, 3, 2, 2), T0);
        feed(&mut a, &enc::velocity(1, 450.0, 90.0, 0, None), T0 + 20_000);
        feed(&mut a, &enc::op_status(1, 10, 3, 2, 2), T0 + 30_000);
        assert_eq!(shown(&a, T0 + 50_000), (Some(2), "mode_s"));
        feed(&mut a, &enc::df11(1, false), T0 + 50_000);
        assert_eq!(shown(&a, T0 + 50_000), (None, "mode_s"));
        // a squitter that states none makes it 0
        feed(&mut a, &enc::velocity(1, 450.0, 90.0, 0, None), T0 + 51_000);
        assert_eq!(shown(&a, T0 + 51_000), (Some(0), "mode_s"));
        feed(&mut a, &enc::op_status(1, 10, 3, 2, 2), T0 + 52_000);
        assert_eq!(shown(&a, T0 + 52_000), (Some(2), "mode_s"));

        // located: an ADS-B aircraft, its version kept, until a message
        // comes 90 s after its last position
        let mut c = Aircraft::new(3, T0);
        feed(&mut c, &enc::airborne_position(3, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut c, &enc::airborne_position(3, 45.5, 9.25, 35000, true, 11), T0 + 400);
        feed(&mut c, &enc::op_status(3, 10, 3, 2, 2), T0 + 500);
        feed(&mut c, &enc::velocity(3, 450.0, 90.0, 0, None), T0 + 50_000);
        assert_eq!(shown(&c, T0 + 94_000), (Some(2), "adsb_icao"));
        feed(&mut c, &enc::velocity(3, 450.0, 90.0, 0, None), T0 + 95_000);
        assert_eq!(shown(&c, T0 + 95_000), (Some(0), "mode_s"));
        // an aircraft whose squitters kept coming while it was placed
        // is given until the 90 s; one that fell quiet is not
        let mut d = Aircraft::new(4, T0);
        feed(&mut d, &enc::airborne_position(4, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut d, &enc::airborne_position(4, 45.5, 9.25, 35000, true, 11), T0 + 400);
        feed(&mut d, &enc::op_status(4, 10, 3, 2, 2), T0 + 500);
        feed(&mut d, &enc::velocity(4, 450.0, 90.0, 0, None), T0 + 50_000);
        feed(&mut d, &enc::velocity(4, 450.0, 90.0, 0, None), T0 + 85_000);
        assert_eq!(shown(&d, T0 + 85_000), (Some(2), "adsb_icao"));
        let mut e = Aircraft::new(5, T0);
        feed(&mut e, &enc::airborne_position(5, 45.5, 9.25, 35000, false, 11), T0);
        feed(&mut e, &enc::airborne_position(5, 45.5, 9.25, 35000, true, 11), T0 + 400);
        feed(&mut e, &enc::op_status(5, 10, 3, 2, 2), T0 + 500);
        feed(&mut e, &enc::velocity(5, 450.0, 90.0, 0, None), T0 + 45_000);
        feed(&mut e, &enc::df11(5, false), T0 + 69_000);
        assert_eq!(shown(&e, T0 + 69_000), (Some(2), "adsb_icao"));
        feed(&mut e, &enc::velocity(5, 450.0, 90.0, 0, None), T0 + 70_000);
        assert_eq!(shown(&e, T0 + 70_000), (Some(0), "mode_s"));
        // a position without a status message: ADS-B again, version 0
        feed(&mut c, &enc::airborne_position(3, 45.5, 9.25, 35000, false, 11), T0 + 110_000);
        feed(&mut c, &enc::airborne_position(3, 45.5, 9.25, 35000, true, 11), T0 + 110_400);
        assert_eq!(shown(&c, T0 + 110_400), (Some(0), "adsb_icao"));
    }

    #[test]
    fn an_air_to_air_altitude_needs_saying_twice() {
        let mut a = Aircraft::new(1, T0);
        feed(&mut a, &enc::df0(1, 36000), T0);
        assert_eq!(a.alt_baro.get(T0), None);
        feed(&mut a, &enc::df0(1, 36000), T0 + 1000);
        assert_eq!(a.alt_baro.get(T0 + 1000), Some(36000));
    }
}
