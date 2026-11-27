mod flight;
mod sky;
mod tape;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Traffic generator and comparison harness for the aggregator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Feed a synthetic sky to an aggregator's Beast ingest port.
    Sky {
        /// host:port of each Beast input to feed (all get the same bytes)
        #[arg(required = true)]
        targets: Vec<String>,
        #[arg(long, default_value_t = 500)]
        planes: usize,
        #[arg(long, default_value_t = 20)]
        receivers: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// lat0,lat1,lon0,lon1
        #[arg(long, default_value = "36,47,6,19", allow_hyphen_values = true)]
        r#box: String,
        #[arg(long, default_value_t = 200.0)]
        range_nm: f64,
        /// stop after this many seconds (0: run until killed)
        #[arg(long, default_value_t = 0)]
        seconds: u64,
        /// receivers sending frames that fail their checksum (first target only)
        #[arg(long, default_value_t = 0)]
        garbage: usize,
        /// receivers reporting real aircraft 2.5 degrees off (first target only)
        #[arg(long, default_value_t = 0)]
        liars: usize,
        /// receivers inventing aircraft at 20,000 messages a second (first target only)
        #[arg(long, default_value_t = 0)]
        flood: usize,
        /// the lying receivers all report the same false positions
        #[arg(long)]
        collude: bool,
        /// local address the misbehaving receivers connect from (e.g.
        /// 127.0.0.2), so the server sees them come from elsewhere than
        /// the honest ones
        #[arg(long)]
        hostile_from: Option<String>,
        /// receivers' frames arrive up to this many milliseconds late
        #[arg(long, default_value_t = 0)]
        jitter_ms: u64,
        /// fraction of frames each receiver misses (0..1)
        #[arg(long, default_value_t = 0.0)]
        loss: f64,
    },
    /// Record a Beast stream (HOST:PORT of a Beast output) to a file.
    Record {
        source: String,
        file: String,
        #[arg(long, default_value_t = 300)]
        seconds: u64,
    },
    /// Play a recording to Beast inputs, as it arrived.
    Replay {
        file: String,
        #[arg(required = true)]
        targets: Vec<String>,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// only frames of this ICAO address (hex)
        #[arg(long)]
        only: Option<String>,
    },
    /// Play scripted flights (one --plane each) to Beast inputs: see
    /// flight.rs for the keys. Used to measure a trace's rules.
    Flight {
        #[arg(required = true)]
        targets: Vec<String>,
        /// key=value,... (alt, gs, track, vrate, turn, accel, from, to, ground, flip, start, gap=A-B, pos, novel, hex, lat, lon)
        #[arg(long = "plane", required = true)]
        planes: Vec<String>,
        #[arg(long, default_value_t = 600)]
        seconds: u64,
    },
    /// Print one aircraft's frames from a recording, decoded.
    Dump {
        file: String,
        /// ICAO address (hex)
        hex: String,
    },
    /// Print one frame of every kind for an address, as "name hex" lines.
    Frames {
        /// ICAO address, hex
        #[arg(long, default_value = "4CA2D6")]
        hex: String,
        /// altitude in the DF4 reply, feet
        #[arg(long, default_value_t = 36000)]
        df4_alt: i32,
        /// roll angle in the BDS 5,0 reply, degrees
        #[arg(long, default_value_t = -12.0, allow_hyphen_values = true)]
        roll: f64,
        /// track rate in the BDS 5,0 reply, degrees a second
        #[arg(long, default_value_t = 0.5, allow_hyphen_values = true)]
        track_rate: f64,
        /// ground speed in the velocity message, knots
        #[arg(long, default_value_t = 440.0)]
        gs: f64,
        /// where the position messages put the aircraft
        #[arg(long, default_value_t = 45.5, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, default_value_t = 9.25, allow_hyphen_values = true)]
        lon: f64,
        /// altitude in the position messages, feet
        #[arg(long, default_value_t = 36000)]
        alt: i32,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Flight { targets, planes, seconds } => flight::run(&targets, &planes, seconds),
        Cmd::Dump { file, hex } => tape::dump(&file, u32::from_str_radix(&hex, 16)?),
        Cmd::Record { source, file, seconds } => tape::record(&source, &file, seconds),
        Cmd::Replay { file, targets, speed, only } => {
            let only = match only {
                Some(h) => Some(u32::from_str_radix(&h, 16)?),
                None => None,
            };
            tape::replay(&file, &targets, speed, only)
        }
        Cmd::Frames { hex, df4_alt, roll, track_rate, gs, lat, lon, alt: pos_alt } => {
            use modes::{alt, encode as e};
            let a = u32::from_str_radix(&hex, 16)?;
            let h = |f: &[u8]| f.iter().map(|b| format!("{b:02X}")).collect::<String>();
            let ac = alt::ac13_encode(36000);
            let out: Vec<(&str, Vec<u8>)> = vec![
                ("df11", e::df11(a, false).to_vec()),
                ("ident", e::ident(a, b"PROBE1  ", 0xA3).to_vec()),
                ("pos_even", e::airborne_position(a, lat, lon, pos_alt, false, 11).to_vec()),
                ("pos_odd", e::airborne_position(a, lat, lon, pos_alt, true, 11).to_vec()),
                ("velocity", e::velocity(a, gs as _, 250.0, -640, Some(225)).to_vec()),
                ("velocity_air", e::velocity_airspeed(a, 252.0, 450, true, -640).to_vec()),
                ("emergency", e::emergency(a, 1, 0x7700).to_vec()),
                ("target_state", e::target_state(a, 36000, 1013.6, 250.3125, 0x13).to_vec()),
                ("op_status", e::op_status(a, 10, 3, 2, 2).to_vec()),
                ("df4", e::df4(a, df4_alt, false).to_vec()),
                ("df5", e::df5(a, 0x2345).to_vec()),
                ("df0", e::df0(a, 36000).to_vec()),
                ("df20_bds50", e::long_ap(20, ac, a, e::bds50(roll as _, 250.0, 440, track_rate as _, 450)).to_vec()),
                ("df20_bds60", e::long_ap(20, ac, a, e::bds60(252.0, 280, 0.78, -640, -608)).to_vec()),
                ("df21_bds40", e::long_ap(21, alt::id13_encode(0x2345), a, e::bds40(36000, 36000, 1013.2)).to_vec()),
                ("df20_bds20", e::long_ap(20, ac, a, e::bds20(b"PROBE2  ")).to_vec()),
                ("df16", e::long_ap(16, ac, a, 0).to_vec()),
                ("pos_even_2", e::airborne_position(a, lat + 0.002, lon, 36000, false, 11).to_vec()),
            ];
            for (n, f) in out {
                println!("{n} {}", h(&f));
            }
            // one after the other without a pause: everything fresh at once
            println!("velocity_b {}", h(&e::velocity(a, 440.0, 250.0, -640, Some(225))));
            println!("+velocity_air_b {}", h(&e::velocity_airspeed(a, 252.0, 450, true, -640)));
            println!("+bds50_b {}", h(&e::long_ap(20, ac, a, e::bds50(-1.0, 250.0, 440, 0.0, 450))));
            println!("+bds60_b {}", h(&e::long_ap(20, ac, a, e::bds60(252.0, 280, 0.78, -640, -608))));
            println!("+pos_b_even {}", h(&e::airborne_position(a, lat + 0.004, lon, 36000, false, 11)));
            println!("+pos_b_odd {}", h(&e::airborne_position(a, lat + 0.0041, lon, 36000, true, 11)));
            println!("surface_even {}", h(&e::surface_position(a, lat, lon, 12.0, 90.0, false)));
            println!("surface_odd {}", h(&e::surface_position(a, lat, lon, 12.0, 90.0, true)));
            // rebroadcasts and non-transponder devices, each under its own address
            let me_pos = |odd: bool| u64::from_be_bytes({
                let f = e::airborne_position(0, lat, lon, 5000, odd, 11);
                let mut b = [0u8; 8];
                b[1..].copy_from_slice(&f[4..11]);
                b
            });
            for (n, cf, add) in [("df18_cf0", 0u8, 1u32), ("df18_cf1", 1, 2), ("df18_tisb", 2, 3), ("df18_cf5", 5, 4), ("df18_adsr", 6, 5)] {
                println!("{n}_even {}", h(&e::raw_es(18, cf, a + add, me_pos(false))));
                println!("{n}_odd {}", h(&e::raw_es(18, cf, a + add, me_pos(true))));
            }
            Ok(())
        }
        Cmd::Sky { targets, planes, receivers, seed, r#box, range_nm, seconds, garbage, liars, flood, collude, hostile_from, jitter_ms, loss } => {
            let b: Vec<f64> = r#box.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>()?;
            anyhow::ensure!(b.len() == 4, "--box wants lat0,lat1,lon0,lon1");
            sky::run(sky::Opts { targets, planes, receivers, seed, box_: (b[0], b[1], b[2], b[3]), range_nm, seconds, garbage, liars, flood, collude, hostile_from, jitter_ms, loss })
        }
    }
}
