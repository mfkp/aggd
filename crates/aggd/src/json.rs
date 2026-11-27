//! readsb's JSON shapes: one aircraft object, the aircraft.json
//! envelope, receiver.json.

use crate::track::{bearing, dist_m, Aircraft, Ms, Store};
use modes::{AddrType, Source};
use std::fmt::Write;

pub fn hex(addr: u32) -> String {
    if addr & modes::NON_ICAO != 0 {
        format!("~{:06x}", addr & 0xFF_FFFF)
    } else {
        format!("{:06x}", addr)
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

pub const EMERGENCY: [&str; 8] = ["none", "general", "lifeguard", "minfuel", "nordo", "unlawful", "downed", "reserved"];

pub fn nav_modes(o: &mut String, m: u8) {
    o.push('[');
    let mut first = true;
    for (bit, name) in [
        (modes::NAV_AUTOPILOT, "autopilot"),
        (modes::NAV_VNAV, "vnav"),
        (modes::NAV_ALTHOLD, "althold"),
        (modes::NAV_APPROACH, "approach"),
        (modes::NAV_LNAV, "lnav"),
        (modes::NAV_TCAS, "tcas"),
    ] {
        if m & bit != 0 {
            if !first {
                o.push(',');
            }
            first = false;
            let _ = write!(o, "\"{name}\"");
        }
    }
    o.push(']');
}

/// The value readsb puts in "type": where the best current data came from.
pub fn type_str(a: &Aircraft, now: Ms) -> &'static str {
    // still that kind: placed that way lately, or heard no other way since
    let still = |ttl: Ms| now.saturating_sub(a.addr_type_t) <= ttl || a.direct <= a.addr_type_t;
    if a.pos.valid(now) {
        match a.pos.src {
            Source::Mlat if a.addr_type == AddrType::Mlat && still(crate::track::MLAT_TYPE_TTL) => return "mlat",
            Source::Jaero if a.addr_type == AddrType::Jaero && still(crate::track::JAERO_TYPE_TTL) => return "adsc",
            Source::Sbs | Source::Prio => return "other",
            _ => {}
        }
    }
    // The kind of ADS-B source its last position came from, for as long
    // as extended squitters keep coming (and a minute and a half more).
    match a.addr_type {
        // set by a message: an extended squitter that is nobody's own
        AddrType::Unknown if a.addr_type_t != 0 && now.saturating_sub(a.es_seen) <= crate::track::ES_TTL => "unknown",
        AddrType::Unknown | AddrType::ModeS => "mode_s",
        // known from ADS-C reports that do not place it yet
        AddrType::Jaero if !a.pos.valid(now) && still(crate::track::JAERO_TYPE_TTL) => "adsc",
        t if t <= AddrType::TisbOther && t != AddrType::Jaero && t != AddrType::Mlat && t != AddrType::Other => {
            let own = matches!(t, AddrType::AdsbIcao);
            if if own { a.adsb_pos != 0 } else { now.saturating_sub(a.es_seen) <= crate::track::ES_TTL } {
                t.as_str()
            } else {
                "mode_s"
            }
        }
        _ => "mode_s",
    }
}

pub struct View<'a> {
    pub now: Ms,
    /// Where receivers roughly are, for aircraft without a position.
    pub reg: Option<&'a crate::receivers::Registry>,
    /// Add rcvrs / receiverCount.
    pub receiver_count: bool,
    /// The site reference for r_dst / r_dir.
    pub site: Option<(f64, f64)>,
    /// Add desc / ownOp / year from the database.
    pub long_db: bool,
}

/// One aircraft object. `extra` are the API's distance and bearing from
/// the query point (nautical miles, degrees).
pub fn aircraft(o: &mut String, a: &Aircraft, v: &View<'_>, extra: Option<(f64, f64)>) {
    let now = v.now;
    let _ = write!(o, "{{\"hex\":\"{}\",\"type\":\"{}\"", hex(a.addr), type_str(a, now));
    if let Some(cs) = a.callsign.get(now) {
        let _ = write!(o, ",\"flight\":\"{}\"", esc(std::str::from_utf8(&cs).unwrap_or("")));
    }
    if let Some(d) = &a.db {
        if !d.reg.is_empty() {
            let _ = write!(o, ",\"r\":\"{}\"", esc(&d.reg));
        }
        if !d.typ.is_empty() {
            let _ = write!(o, ",\"t\":\"{}\"", esc(&d.typ));
        }
        if v.long_db {
            if !d.desc.is_empty() {
                let _ = write!(o, ",\"desc\":\"{}\"", esc(&d.desc));
            }
            if !d.own_op.is_empty() {
                let _ = write!(o, ",\"ownOp\":\"{}\"", esc(&d.own_op));
            }
            if !d.year.is_empty() {
                let _ = write!(o, ",\"year\":\"{}\"", esc(&d.year));
            }
        }
        if d.flags != 0 {
            let _ = write!(o, ",\"dbFlags\":{}", d.flags);
        }
    }
    if a.on_ground(now) {
        o.push_str(",\"alt_baro\":\"ground\"");
    } else if let Some(x) = a.alt_baro.get(now) {
        let _ = write!(o, ",\"alt_baro\":{x}");
    }
    if let Some(x) = a.alt_geom.get(now) {
        let _ = write!(o, ",\"alt_geom\":{x}");
    }
    if let Some(x) = a.gs.get(now) {
        let _ = write!(o, ",\"gs\":{x:.1}");
    }
    if let Some(x) = a.ias.get(now) {
        let _ = write!(o, ",\"ias\":{x}");
    }
    if let Some(x) = a.tas.get(now) {
        let _ = write!(o, ",\"tas\":{x}");
    }
    if let Some(x) = a.mach.get(now) {
        let _ = write!(o, ",\"mach\":{x:.3}");
    }
    if let Some(w) = wind(a, now) {
        let _ = write!(o, ",\"wd\":{:.0},\"ws\":{:.0}", w.0, w.1);
    }
    if *WIND_DEBUG {
        let age = |t: Ms| if t == 0 { -1 } else { now.saturating_sub(t) as i64 };
        let _ = write!(
            o,
            ",\"dbg\":{{\"tas\":{},\"ias\":{},\"mach\":{},\"mh\":{},\"th\":{},\"gs\":{},\"trk\":{},\"roll\":{},\"pos\":{},\"wind\":{},\"gs_src\":{},\"tas_src\":{},\"rollv\":{}}}",
            age(a.tas.t), age(a.ias.t), age(a.mach.t), age(a.mag_heading.t), age(a.true_heading.t), age(a.gs.t), age(a.track.t), age(a.roll.t), age(a.pos.t), age(a.wind.t),
            a.gs.src as u8, a.tas.src as u8, a.roll.v
        );
    }
    if let Some(t) = temps(a, now) {
        let _ = write!(o, ",\"oat\":{:.0},\"tat\":{:.0}", t.0, t.1);
    }
    if let Some(x) = a.track.get(now) {
        let _ = write!(o, ",\"track\":{x:.2}");
    } else if a.pos.valid(now) && !(a.on_ground(now) && a.true_heading_at(now).is_some()) {
        // readsb shows one for every placed aircraft that reports no
        // track (not for one on the ground that gives its heading):
        // north until two positions 100 m apart have given it
        let x = if a.calc_track.t != 0 { a.calc_track.v } else { 0.0 };
        let _ = write!(o, ",\"calc_track\":{x:.0}");
    }
    if let Some(x) = a.track_rate.get(now) {
        let _ = write!(o, ",\"track_rate\":{x:.2}");
    }
    if let Some(x) = a.roll.get(now) {
        let _ = write!(o, ",\"roll\":{x:.2}");
    }
    if let Some(x) = a.mag_heading.get(now) {
        let _ = write!(o, ",\"mag_heading\":{x:.2}");
    }
    if let Some((x, _)) = a.true_heading_at(now) {
        let _ = write!(o, ",\"true_heading\":{x:.2}");
    }
    if let Some(x) = a.baro_rate.get(now) {
        let _ = write!(o, ",\"baro_rate\":{x}");
    }
    if let Some(x) = a.geom_rate.get(now) {
        let _ = write!(o, ",\"geom_rate\":{x}");
    }
    if let Some(x) = a.squawk.get(now) {
        let _ = write!(o, ",\"squawk\":\"{x:04x}\"");
    }
    if let Some(x) = a.emergency.get(now) {
        let _ = write!(o, ",\"emergency\":\"{}\"", EMERGENCY[x as usize & 7]);
    }
    if a.category != 0 {
        let _ = write!(o, ",\"category\":\"{:02X}\"", a.category);
    }
    if let Some(x) = a.nav_qnh.get(now) {
        let _ = write!(o, ",\"nav_qnh\":{x:.1}");
    }
    if let Some(x) = a.nav_alt_mcp.get(now) {
        let _ = write!(o, ",\"nav_altitude_mcp\":{x}");
    }
    if let Some(x) = a.nav_alt_fms.get(now) {
        let _ = write!(o, ",\"nav_altitude_fms\":{x}");
    }
    if let Some(x) = a.nav_heading.get(now) {
        let _ = write!(o, ",\"nav_heading\":{x:.2}");
    }
    if let Some(x) = a.nav_modes.get(now) {
        o.push_str(",\"nav_modes\":");
        nav_modes(o, x);
    }
    if a.pos.valid(now) {
        let (lat, lon) = a.pos.v;
        let _ = write!(
            o,
            ",\"lat\":{:.6},\"lon\":{:.6},\"nic\":{},\"rc\":{},\"seen_pos\":{:.3}",
            lat,
            lon,
            a.nic,
            a.rc,
            a.pos.age_s(now)
        );
        if let Some((slat, slon)) = v.site {
            let _ = write!(
                o,
                ",\"r_dst\":{:.3},\"r_dir\":{:.1}",
                dist_m(slat, slon, lat, lon) / 1852.0,
                bearing(slat, slon, lat, lon)
            );
        }
    } else {
        // no position of its own: at least say roughly where it was heard
        if let Some((lat, lon)) = v.reg.and_then(|r| r.reference(a.last_rx)) {
            let _ = write!(o, ",\"rr_lat\":{lat:.1},\"rr_lon\":{lon:.1}");
        }
        if a.pos.src != Source::Invalid {
            let _ = write!(
                o,
                ",\"lastPosition\":{{\"lat\":{:.6},\"lon\":{:.6},\"nic\":{},\"rc\":{},\"seen_pos\":{:.3}}}",
                a.pos.v.0,
                a.pos.v.1,
                a.nic,
                a.rc,
                a.pos.age_s(now)
            );
            // an aircraft still transmitting whose ADS-B position stopped:
            // where and when its GPS was last good
            if a.pos.src == Source::Adsb && a.gps_lost > a.pos.t && a.pos.age_s(now) < 900.0 {
                let _ = write!(
                    o,
                    ",\"gpsOkBefore\":{:.1},\"gpsOkLat\":{:.6},\"gpsOkLon\":{:.6}",
                    a.pos.t as f64 / 1000.0,
                    a.pos.v.0,
                    a.pos.v.1
                );
            }
        }
    }
    if let Some(v) = a.version_shown(now) {
        let _ = write!(o, ",\"version\":{v}");
    }
    if let Some(x) = a.nic_baro.get(now) {
        let _ = write!(o, ",\"nic_baro\":{}", x as u8);
    }
    if let Some(x) = a.nac_p.get(now) {
        let _ = write!(o, ",\"nac_p\":{x}");
    }
    if let Some(x) = a.nac_v.get(now) {
        let _ = write!(o, ",\"nac_v\":{x}");
    }
    if let Some(x) = a.sil.get(now) {
        let _ = write!(o, ",\"sil\":{x}");
    }
    if a.sil_type < 3 {
        let _ = write!(o, ",\"sil_type\":\"{}\"", ["unknown", "perhour", "persample"][a.sil_type as usize]);
    }
    if let Some(x) = a.gva.get(now) {
        let _ = write!(o, ",\"gva\":{x}");
    }
    if let Some(x) = a.sda.get(now) {
        let _ = write!(o, ",\"sda\":{x}");
    }
    if let Some(x) = a.alert.get(now) {
        let _ = write!(o, ",\"alert\":{}", x as u8);
    }
    if let Some(x) = a.spi.get(now) {
        let _ = write!(o, ",\"spi\":{}", x as u8);
    }
    by_source(o, a, now, Source::Mlat, "mlat");
    by_source(o, a, now, Source::Tisb, "tisb");
    let _ = write!(
        o,
        ",\"messages\":{},\"seen\":{:.1},\"rssi\":{:.1}",
        a.messages,
        now.saturating_sub(a.seen) as f64 / 1000.0,
        a.rssi()
    );
    if v.receiver_count {
        let n = a.receiver_count(now);
        let _ = write!(o, ",\"rcvrs\":{n},\"receiverCount\":{n}");
    }
    if let Some((t, b)) = a.acas_ra {
        if now.saturating_sub(t) < 15_000 {
            acas_ra(o, t, &b);
        }
    }
    if let Some((dst, dir)) = extra {
        let _ = write!(o, ",\"dst\":{dst:.3},\"dir\":{dir:.1}");
    }
    o.push('}');
}

/// An ACAS resolution advisory report (ADS-B type 28 subtype 2, or
/// Comm-B register 3,0): the advisory bits, the complement, and the threat.
fn acas_ra(o: &mut String, t: Ms, b: &[u8; 7]) {
    let v = b.iter().fold(0u64, |a, &x| a << 8 | x as u64);
    let bits = |first: u32, last: u32| (v >> (56 - last)) & ((1 << (last - first + 1)) - 1);
    let bit = |n: u32| bits(n, n) != 0;
    let (date, time) = {
        let s = crate::persist::civil(t);
        let d = t % 86_400_000;
        (format!("{:04}-{:02}-{:02}", s.0, s.1, s.2), format!("{:02}:{:02}:{:02}.{}", d / 3_600_000, d / 60_000 % 60, d / 1000 % 60, d % 1000 / 100))
    };
    let hexbytes: String = b.iter().map(|x| format!("{x:02X}")).collect();
    let (rat, mte) = (bit(27), bit(28));
    let advisory = if rat {
        "Clear of Conflict"
    } else if !bit(9) && !mte {
        ""
    } else if bit(10) {
        // corrective
        match (bit(15), bit(11), bit(12), bit(13), bit(14)) {
            (true, false, true, _, _) => "Increase Climb",
            (true, true, true, _, _) => "Increase Descent",
            (true, false, _, true, _) => "Climb; Climb NOW",
            (true, true, _, true, _) => "Descend; Descend NOW",
            (true, false, _, _, true) => "Climb; Crossing Climb",
            (true, true, _, _, true) => "Descend; Crossing Descend",
            (true, false, ..) => "Climb",
            (true, true, ..) => "Descend",
            (false, ..) => "Level Off",
        }
    } else {
        "Monitor Vertical Speed"
    };
    let mut comp: Vec<&str> = Vec::new();
    for (n, text) in [(23, "Do not pass below"), (24, "Do not pass above"), (25, "Do not turn left"), (26, "Do not turn right")] {
        if bit(n) {
            comp.push(text);
        }
    }
    let _ = write!(
        o,
        ",\"acas_ra\":{{\"utc\":\"{date} {time}\",\"unix_timestamp\":{:.2},\"bytes\":\"{hexbytes}\",\"ARA\":\"{:07b}\",\"RAT\":\"{}\",\"MTE\":\"{}\",\"RAC\":\"{:04b}\",\"advisory_complement\":\"{}\",\"advisory\":\"{advisory}\",\"TTI\":\"{:02b}\"",
        t as f64 / 1000.0,
        bits(9, 15),
        rat as u8,
        mte as u8,
        bits(23, 26),
        comp.join("; "),
        bits(29, 30)
    );
    if bits(29, 30) == 1 {
        let _ = write!(o, ",\"threat_id_hex\":\"{:06x}\"", bits(31, 54));
    }
    o.push('}');
}

/// The names of the fields a given source supplied.
fn by_source(o: &mut String, a: &Aircraft, now: Ms, s: Source, name: &str) {
    let _ = write!(o, ",\"{name}\":[");
    let mut first = true;
    let mut put = |o: &mut String, on: bool, n: &str| {
        if on {
            if !first {
                o.push(',');
            }
            first = false;
            let _ = write!(o, "\"{n}\"");
        }
    };
    macro_rules! f {
        ($field:ident, $n:literal) => {
            put(o, a.$field.src == s && a.$field.valid(now), $n)
        };
    }
    f!(callsign, "callsign");
    f!(alt_baro, "altitude");
    f!(alt_baro, "alt_baro");
    f!(alt_geom, "alt_geom");
    f!(gs, "gs");
    f!(ias, "ias");
    f!(tas, "tas");
    f!(mach, "mach");
    f!(track, "track");
    f!(track_rate, "track_rate");
    f!(roll, "roll");
    f!(mag_heading, "mag_heading");
    f!(true_heading, "true_heading");
    f!(baro_rate, "baro_rate");
    f!(geom_rate, "geom_rate");
    f!(squawk, "squawk");
    f!(emergency, "emergency");
    f!(nav_qnh, "nav_qnh");
    f!(nav_alt_mcp, "nav_altitude_mcp");
    f!(nav_alt_fms, "nav_altitude_fms");
    f!(nav_heading, "nav_heading");
    f!(nav_modes, "nav_modes");
    let p = a.pos.src == s && a.pos.valid(now);
    put(o, p, "lat");
    put(o, p, "lon");
    put(o, p, "nic");
    put(o, p, "rc");
    f!(nic_baro, "nic_baro");
    f!(nac_p, "nac_p");
    f!(nac_v, "nac_v");
    f!(sil, "sil");
    f!(sil, "sil_type");
    f!(gva, "gva");
    f!(sda, "sda");
    o.push(']');
}

/// Wind direction (from, degrees) and speed (knots), as last derived.
pub fn wind(a: &Aircraft, now: Ms) -> Option<(f64, f64)> {
    a.wind_now(now).map(|w| (w.0 as f64, w.1 as f64))
}

/// Outside and total air temperature (°C), as last derived.
pub fn temps(a: &Aircraft, now: Ms) -> Option<(f64, f64)> {
    a.temps.get(now).map(|t| (t.0 as f64, t.1 as f64))
}

/// aircraft.json: every aircraft heard in the last `window`.
pub fn aircraft_json(store: &Store, v: &View<'_>, window: Ms) -> String {
    let mut o = String::with_capacity(1 << 20);
    let _ = write!(
        o,
        "{{ \"now\" : {:.3},\n  \"messages\" : {},\n  \"aircraft\" : [\n",
        v.now as f64 / 1000.0,
        store.messages()
    );
    let mut first = true;
    for s in store.shards() {
        let s = s.lock();
        for a in s.map.values() {
            if !listed(a, v.now, window) {
                continue;
            }
            if !first {
                o.push_str(",\n");
            }
            first = false;
            aircraft(&mut o, a, v, None);
        }
    }
    o.push_str("\n  ]\n}\n");
    o
}

/// Whether an aircraft belongs in the live outputs.
#[inline]
pub fn listed(a: &Aircraft, now: Ms, window: Ms) -> bool {
    // one message alone is as likely noise as an aircraft
    // (a position solved elsewhere is not noise: readsb lists an aircraft
    // on its first MLAT result or ADS-C report)
    (a.messages >= 2 && now.saturating_sub(a.checked) <= window) || (matches!(a.pos.src, Source::Jaero | Source::Mlat) && a.pos.valid(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acas_report_as_seen_on_a_live_network() {
        // a real type 28 subtype 2 report: level off, one threat by address
        let b = [0xE2, 0xE0, 0x00, 0x06, 0x8A, 0x69, 0x34];
        let mut o = String::new();
        acas_ra(&mut o, 1_794_835_559_680, &b);
        assert_eq!(
            o,
            ",\"acas_ra\":{\"utc\":\"2026-11-16 13:25:59.6\",\"unix_timestamp\":1794835559.68,\"bytes\":\"E2E000068A6934\",\"ARA\":\"1110000\",\"RAT\":\"0\",\"MTE\":\"0\",\"RAC\":\"0000\",\"advisory_complement\":\"\",\"advisory\":\"Level Off\",\"TTI\":\"01\",\"threat_id_hex\":\"a29a4d\"}"
        );
        let f = modes::encode::raw_es(17, 5, 0xA2235B, 0xE2E000068A6934);
        assert_eq!(modes::decode(&f, |_| false).unwrap().acas_ra, Some(b));
    }
}

/// AGGD_DEBUG_WIND=1 adds the ages of wind's inputs to each aircraft, for
/// working out a reference's rules.
static WIND_DEBUG: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("AGGD_DEBUG_WIND").is_some());
