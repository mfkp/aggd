//! Scripted flights: aircraft that each do one thing (climb at a fixed
//! rate, turn at a fixed rate, accelerate, sit on the ground, fall
//! silent for a while), sent as Beast frames to several inputs at once.
//! What an aggregator's trace keeps of such a flight shows its rule for
//! when a point is added, one cause at a time.

use crate::sky::beast;
use modes::encode as enc;
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

struct Flight {
    addr: u32,
    lat: f64,
    lon: f64,
    alt: f64,
    gs: f64,
    track: f64,
    /// ft/min, degrees/s, knots/s: applied between `from` and `to` seconds
    vrate: f64,
    turn: f64,
    accel: f64,
    from: f64,
    to: f64,
    ground: bool,
    /// becomes airborne (or lands, if it started in the air) at this time
    flip: f64,
    /// first transmission, and a silence
    start: f64,
    gap: (f64, f64),
    /// milliseconds between positions
    pos_ms: u64,
    /// no velocity messages: speed and track unknown in the air
    no_velocity: bool,
    /// in level flight, report this vertical rate and zero in turn,
    /// changing every `rate_period` seconds (the altitude stays)
    rate_jitter: f64,
    rate_period: f64,
    /// for a second from this time, positions half a degree north of the truth
    glitch: f64,
    /// at this time, one DF11 that says "on the ground" from an aircraft in the air
    df11_ground: f64,
    /// from this time, another callsign
    ident2: f64,
    /// between these times only DF11, no position, velocity or callsign
    quiet: (f64, f64),
}

/// `hex=4D0001,alt=5000,gs=250,track=90,vrate=1000,turn=0,accel=0,from=120,to=600,ground=0,flip=300,start=0,gap=200-400,pos=500,novel=1,rj=256,rjp=8,lat=42.1,lon=12.5`
fn parse(spec: &str, i: usize) -> anyhow::Result<Flight> {
    let mut f = Flight {
        addr: 0x4D0000 + i as u32,
        lat: 42.0 + (i % 40) as f64 * 0.05,
        lon: 12.5 + (i / 40) as f64 * 0.5,
        alt: 30000.0,
        gs: 300.0,
        track: 90.0,
        vrate: 0.0,
        turn: 0.0,
        accel: 0.0,
        from: 0.0,
        to: f64::MAX,
        ground: false,
        flip: f64::MAX,
        start: 0.0,
        gap: (f64::MAX, f64::MAX),
        pos_ms: 500,
        no_velocity: false,
        rate_jitter: 0.0,
        rate_period: 8.0,
        glitch: f64::MAX,
        df11_ground: f64::MAX,
        ident2: f64::MAX,
        quiet: (f64::MAX, f64::MAX),
    };
    for kv in spec.split(',').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("{kv}: want key=value"))?;
        let n = || v.parse::<f64>().map_err(|_| anyhow::anyhow!("{kv}: not a number"));
        match k {
            "hex" => f.addr = u32::from_str_radix(v, 16)?,
            "lat" => f.lat = n()?,
            "lon" => f.lon = n()?,
            "alt" => f.alt = n()?,
            "gs" => f.gs = n()?,
            "track" => f.track = n()?,
            "vrate" => f.vrate = n()?,
            "turn" => f.turn = n()?,
            "accel" => f.accel = n()?,
            "from" => f.from = n()?,
            "to" => f.to = n()?,
            "ground" => f.ground = n()? != 0.0,
            "flip" => f.flip = n()?,
            "start" => f.start = n()?,
            "pos" => f.pos_ms = n()? as u64,
            "novel" => f.no_velocity = n()? != 0.0,
            "rj" => f.rate_jitter = n()?,
            "rjp" => f.rate_period = n()?,
            "glitch" => f.glitch = n()?,
            "df11g" => f.df11_ground = n()?,
            "ident2" => f.ident2 = n()?,
            "quiet" => {
                let (a, b) = v.split_once('-').ok_or_else(|| anyhow::anyhow!("{kv}: want quiet=A-B"))?;
                f.quiet = (a.parse()?, b.parse()?);
            }
            "gap" => {
                let (a, b) = v.split_once('-').ok_or_else(|| anyhow::anyhow!("{kv}: want gap=A-B"))?;
                f.gap = (a.parse()?, b.parse()?);
            }
            _ => anyhow::bail!("{kv}: unknown key"),
        }
    }
    Ok(f)
}

pub fn run(targets: &[String], specs: &[String], seconds: u64) -> anyhow::Result<()> {
    let mut flights = specs.iter().enumerate().map(|(i, s)| parse(s, i)).collect::<anyhow::Result<Vec<_>>>()?;
    let mut socks = targets.iter().map(|t| TcpStream::connect(t).map_err(|e| anyhow::anyhow!("{t}: {e}"))).collect::<anyhow::Result<Vec<_>>>()?;
    for s in &socks {
        s.set_nodelay(true)?;
    }
    let start = Instant::now();
    let tick_ms = 50u64;
    let mut now_ms = 0u64;
    let mut out = Vec::new();
    while now_ms < seconds * 1000 {
        now_ms += tick_ms;
        let t = now_ms as f64 / 1000.0;
        let dt = tick_ms as f64 / 1000.0;
        out.clear();
        for (i, f) in flights.iter_mut().enumerate() {
            if t >= f.flip {
                f.ground = !f.ground;
                f.flip = f64::MAX;
            }
            if t >= f.from && t < f.to {
                f.alt += f.vrate * dt / 60.0;
                f.track = (f.track + f.turn * dt).rem_euclid(360.0);
                f.gs = (f.gs + f.accel * dt).max(0.0);
            }
            let d = f.gs * dt / 3600.0;
            f.lat += d * f.track.to_radians().cos() / 60.0;
            f.lon += d * f.track.to_radians().sin() / (60.0 * f.lat.to_radians().cos());
            if t < f.start || (t >= f.gap.0 && t < f.gap.1) {
                continue;
            }
            // each aircraft at its own moment within the period
            let due = |period: u64, offset: u64| {
                let x = now_ms + i as u64 * 37 + offset;
                x / period != (x - tick_ms) / period
            };
            let mut put = |frame: &[u8]| beast(frame, now_ms * 12_000, 120, &mut out);
            let alt = (f.alt / 25.0).round() as i32 * 25;
            let climbing = t >= f.from && t < f.to;
            if t >= f.quiet.0 && t < f.quiet.1 {
                if due(1000, 50) {
                    put(&enc::df11(f.addr, f.ground));
                }
                continue;
            }
            if t >= f.df11_ground {
                put(&enc::df11(f.addr, true));
                f.df11_ground = f64::MAX;
            }
            if due(f.pos_ms, 0) {
                let odd = ((now_ms + i as u64 * 37) / f.pos_ms) % 2 == 1;
                if f.ground {
                    put(&enc::surface_position(f.addr, f.lat, f.lon, f.gs as f32, f.track as f32, odd));
                } else {
                    let off = if t >= f.glitch && t < f.glitch + 1.2 { 0.5 } else { 0.0 };
                    put(&enc::airborne_position(f.addr, f.lat + off, f.lon, alt, odd, 11));
                }
            }
            if !f.ground && !f.no_velocity && due(500, 250) {
                let level = if (t / f.rate_period) as u64 % 2 == 1 { f.rate_jitter as i32 } else { 0 };
                put(&enc::velocity(f.addr, f.gs as f32, f.track as f32, if f.rate_jitter != 0.0 { level } else if climbing { f.vrate as i32 } else { 0 }, Some(175)));
            }
            if due(5000, 100) {
                put(&enc::ident(f.addr, if t >= f.ident2 { b"OTHER   " } else { b"FLIGHT  " }, 0xA3));
            }
            if due(4000, 50) {
                put(&enc::df11(f.addr, f.ground));
            }
        }
        if !out.is_empty() {
            for s in &mut socks {
                s.write_all(&out)?;
            }
        }
        if let Some(wait) = (start + Duration::from_millis(now_ms)).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    Ok(())
}
