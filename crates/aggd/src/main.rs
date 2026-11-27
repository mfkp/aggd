//! aggd: an ADS-B aggregator. Receivers connect and send Beast; aggd
//! keeps one table of aircraft and serves it the way readsb does, so
//! tar1090, API clients and downstream feeds work unchanged.

mod api;
mod bincraft;
mod tiles_data;
mod db;
mod geomag;
mod guard;
mod heatmap;
mod wmm_data;
mod http;
mod json;
mod mil_ranges;
mod net;
mod persist;
mod receivers;
mod trace;
mod track;

use clap::Parser;
use net::Proto;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use track::{Ms, Store};

pub fn now_ms() -> Ms {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as Ms)
}

/// Command line. Option names follow readsb where the meaning is the
/// same, so an existing aggregator's arguments carry over.
#[derive(Parser, Debug)]
#[command(version, about = "ADS-B aggregator: many receivers in, one sky out")]
struct Cli {
    /// Reference latitude for r_dst / r_dir (optional)
    #[arg(long, allow_hyphen_values = true)]
    lat: Option<f64>,
    #[arg(long, allow_hyphen_values = true)]
    lon: Option<f64>,
    /// Address to bind listeners to
    #[arg(long, default_value = "0.0.0.0")]
    net_bind_address: String,
    /// Beast input ports (comma separated): receivers connect here
    #[arg(long, value_delimiter = ',')]
    net_bi_port: Vec<u16>,
    /// Beast output ports: every accepted message
    #[arg(long, value_delimiter = ',')]
    net_bo_port: Vec<u16>,
    /// Beast output ports, rate limited per aircraft
    #[arg(long, value_delimiter = ',')]
    net_beast_reduce_out_port: Vec<u16>,
    /// Seconds between messages of a kind per aircraft on reduced outputs
    #[arg(long, default_value_t = 0.25)]
    net_beast_reduce_interval: f64,
    /// BaseStation output ports
    #[arg(long, value_delimiter = ',')]
    net_sbs_port: Vec<u16>,
    /// BaseStation input ports
    #[arg(long, value_delimiter = ',')]
    net_sbs_in_port: Vec<u16>,
    /// BaseStation input ports for JAERO (ADS-C) positions
    #[arg(long, value_delimiter = ',')]
    net_sbs_jaero_in_port: Vec<u16>,
    /// BaseStation output ports carrying only JAERO (ADS-C) positions
    #[arg(long, value_delimiter = ',')]
    net_sbs_jaero_port: Vec<u16>,
    /// Send MLAT positions on the Beast outputs too, as extended
    /// squitters with the MLAT timestamp
    #[arg(long)]
    forward_mlat: bool,
    /// Send MLAT positions on the BaseStation outputs too
    #[arg(long)]
    forward_mlat_sbs: bool,
    /// Rate-limit the BaseStation outputs per aircraft like the reduced Beast output
    #[arg(long)]
    net_sbs_reduce: bool,
    /// UAT input ports: dump978 raw lines (978 MHz ADS-B)
    #[arg(long, value_delimiter = ',')]
    net_uat_in_port: Vec<u16>,
    /// JSON position output ports: one line per position
    #[arg(long, value_delimiter = ',')]
    net_json_port: Vec<u16>,
    /// Minimum seconds between JSON port lines per aircraft
    #[arg(long, default_value_t = 0.0)]
    net_json_port_interval: f64,
    /// JSON port: also send aircraft that have no position
    #[arg(long)]
    net_json_port_include_noposition: bool,
    /// HTTP port for the query API (the same server as --http-port)
    #[arg(long)]
    net_api_port: Option<u16>,
    /// HTTP ports serving /data/*.json, traces and /re-api/
    #[arg(long, value_delimiter = ',')]
    http_port: Vec<u16>,
    /// host,port,protocol: an outbound connection. Protocols: beast_in,
    /// beast_out, beast_reduce_out, beast_reduce_plus_out, sbs_in,
    /// sbs_in_mlat, sbs_in_jaero, sbs_in_prio, uat_in, sbs_out, json_out;
    /// an optional fourth field uuid=UUID announces that id on this
    /// beast_reduce_plus_out connection
    #[arg(long)]
    net_connector: Vec<String>,
    /// Forward each message's receiver id on beast_reduce_plus_out
    #[arg(long)]
    net_receiver_id: bool,
    /// UUID announced on beast_reduce_plus_out connections
    #[arg(long)]
    uuid: Option<String>,
    #[arg(long)]
    uuid_file: Option<String>,
    /// tar1090-db aircraft.csv.gz for registration, type and flags
    #[arg(long)]
    db_file: Option<String>,
    /// Add desc, ownOp and year from the database to aircraft objects
    #[arg(long)]
    db_file_lt: bool,
    /// Minutes a JAERO position stays valid
    #[arg(long, default_value_t = 33.0)]
    jaero_timeout: f64,
    /// Seconds between trace points in steady flight
    #[arg(long, default_value_t = 30.0)]
    json_trace_interval: f64,
    /// Seconds between refreshes of aircraft.json
    #[arg(long, default_value_t = 1.0)]
    write_json_every: f64,
    /// Also write the JSON files to this directory every interval
    #[arg(long)]
    write_json: Option<String>,
    /// With --write-json, also keep each aircraft's trace there as readsb
    /// does: traces/<last two hex digits>/trace_recent_<hex>.json and
    /// trace_full_<hex>.json, gzipped, rewritten as the trace grows
    #[arg(long)]
    write_traces: bool,
    /// Also write what /metrics serves to this file every interval, for
    /// a node exporter's textfile collector
    #[arg(long)]
    write_prom: Option<String>,
    /// Write per-day trace files here (tar1090's globe_history layout)
    #[arg(long)]
    write_globe_history: Option<String>,
    /// Write heat map files (tar1090's ?heatmap) sampling every N
    /// seconds; needs --write-globe-history or --heatmap-dir
    #[arg(long)]
    heatmap: Option<u32>,
    /// Directory for heat map files (default: the globe history directory)
    #[arg(long)]
    heatmap_dir: Option<String>,
    /// Save traces here (all of them hourly, what they gained since on
    /// exit) and restore them at start
    #[arg(long)]
    write_state: Option<String>,
    /// Serve this directory over HTTP beside the data: tar1090's html
    /// directory makes aggd a complete map server
    #[arg(long)]
    web_root: Option<String>,
    /// Access-Control-Allow-Origin on HTTP answers: * lets any page read
    /// the data; none leaves the header out, for a proxy in front that sets
    /// its own; or origins separated by commas (https://map.example.org),
    /// each answered with its own name and the rest with no header
    #[arg(long, default_value = "*")]
    cors_origin: String,
    /// Seconds an aircraft stays listed after it was last heard
    #[arg(long, default_value_t = 60.0)]
    interactive_ttl: f64,
    /// Add rcvrs / receiverCount (receivers hearing the aircraft) to aircraft objects
    #[arg(long)]
    json_receiver_count: bool,
    /// Feeder connections accepted at once
    #[arg(long, default_value_t = 20000)]
    max_connections: usize,
    /// Feeder connections accepted from one address
    #[arg(long, default_value_t = 64)]
    max_connections_per_ip: u32,
    /// Messages per second one feeder connection may send (ten-second
    /// average); the excess is dropped, a link far above it is refused
    #[arg(long, default_value_t = 6000.0)]
    max_receiver_rate: f64,
    /// Aircraft one feeder connection may introduce per minute, beyond
    /// the thousand it may bring when it connects
    #[arg(long, default_value_t = 300)]
    max_new_aircraft: u32,
    /// Aircraft kept in the table at most
    #[arg(long, default_value_t = 500000)]
    max_aircraft: usize,
    /// Addresses or networks (CIDR, comma separated) exempt from the
    /// feeder limits and allowed to forward receiver ids: our own nodes
    #[arg(long, value_delimiter = ',')]
    net_trusted: Vec<String>,
    /// Hours of trace kept in memory per aircraft
    #[arg(long, default_value_t = 24.0)]
    trace_hours: f64,
    /// Worker threads for network I/O and decoding (default 2). One
    /// thread carries several hundred thousand messages a second; more
    /// threads only help beyond that and cost CPU below it.
    #[arg(long)]
    threads: Option<usize>,

    // Accepted for compatibility with readsb command lines; these are
    // either always on here or have no meaning without a radio.
    #[arg(long, hide = true)]
    modeac: bool,
    #[arg(long, hide = true)]
    modeac_auto: bool,
    #[arg(long, hide = true)]
    net: bool,
    #[arg(long, hide = true)]
    net_only: bool,
    #[arg(long, hide = true)]
    quiet: bool,
    #[arg(long, hide = true)]
    net_ingest: bool,
    #[arg(long, hide = true)]
    write_receiver_id_json: bool,
    #[arg(long, hide = true)]
    write_json_globe_index: bool,
    #[arg(long, hide = true)]
    max_range: Option<f64>,
    #[arg(long, hide = true)]
    json_reliable: Option<i32>,
    #[arg(long, hide = true)]
    net_heartbeat: Option<f64>,
    #[arg(long, hide = true)]
    net_ro_interval: Option<f64>,
    #[arg(long, hide = true)]
    net_ro_size: Option<u32>,
    #[arg(long, hide = true)]
    net_buffer: Option<u32>,
    #[arg(long, hide = true)]
    net_connector_delay: Option<f64>,
    #[arg(long, hide = true)]
    net_ro_interval_beast_reduce: Option<f64>,
    #[arg(long, hide = true)]
    stats_every: Option<u32>,
    #[arg(long, hide = true)]
    decode_threads: Option<u32>,
    #[arg(long, hide = true)]
    tar1090_use_api: bool,
    #[arg(long, hide = true)]
    range_outline_hours: Option<f64>,
    // readsb's debug output selection; aggd logs what it logs
    #[arg(long, hide = true)]
    debug: Option<String>,
}

pub struct Config {
    pub site: Option<(f64, f64)>,
    pub reduce_interval_ms: Ms,
    pub json_port_interval_ms: Ms,
    pub json_port_noposition: bool,
    pub json_every_ms: Ms,
    pub trace_interval_ms: Ms,
    pub trace_recent_points: usize,
    pub trace_keep_ms: Ms,
    pub list_window_ms: Ms,
    pub forward_receiver_id: bool,
    pub uuid: Option<String>,
    pub long_db: bool,
    pub jaero_timeout_min: f64,
    pub receiver_count: bool,
    pub forward_mlat: bool,
    pub forward_mlat_sbs: bool,
    pub sbs_reduce: bool,
    pub web_root: Option<String>,
    pub history_dir: Option<String>,
    pub cors: http::Cors,
}

/// Counters kept for stats.json: totals since start, sampled every ten
/// seconds so that windows can be told apart.
const C_MESSAGES: usize = 0;
const C_POSITIONS: usize = 1;
const C_BYTES_IN: usize = 2;
const C_BAD: usize = 3;
const C_SBS: usize = 4;
const C_BYTES_OUT: usize = 5;
const C_TYPE0: usize = 6;
const COUNTERS: usize = C_TYPE0 + 14;

#[derive(Clone, Copy)]
struct Sample {
    t: Ms,
    c: [u64; COUNTERS],
    /// farthest position from the site since the previous sample, metres
    max_dist: u64,
}

#[derive(Default)]
pub struct Stats {
    pub positions: AtomicU64,
    pub bad: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// batches of output a consumer was too slow for and did not get
    pub out_skipped: AtomicU64,
    pub sbs: AtomicU64,
    pub pos_by_type: [AtomicU64; 14],
    pub max_dist: AtomicU64,
    samples: Mutex<VecDeque<Sample>>,
}

pub struct Ctx {
    pub cfg: Config,
    pub store: Store,
    pub reg: receivers::Registry,
    pub hub: net::Hub,
    pub db: Option<db::Db>,
    pub stats: Stats,
    pub cache: http::Cache,
    pub guard: guard::Guard,
    pub heat: heatmap::Heatmap,
    pub state: persist::StateFiles,
    pub started: Ms,
}

impl Ctx {
    pub fn view(&self, now: Ms) -> json::View<'_> {
        json::View { now, reg: Some(&self.reg), receiver_count: self.cfg.receiver_count, site: self.cfg.site, long_db: self.cfg.long_db }
    }

    pub fn receiver_json(&self) -> String {
        let site = match self.cfg.site {
            Some((lat, lon)) => format!(" \"lat\": {lat:.6}, \"lon\": {lon:.6},"),
            None => String::new(),
        };
        format!(
            "{{ \"refresh\": {}, \"history\": 0,{} \"jaeroTimeout\": {:.1}, \"readsb\": true, \"dbServer\": {}, \"haveTraces\": true, \"json_trace_interval\": {:.1}, \"reapi\": true, \"globeIndexGrid\": {}, \"globeIndexSpecialTiles\": {}, \"binCraft\": true, \"zstd\": true, \"version\": \"aggd {}\" }}\n",
            self.cfg.json_every_ms,
            site,
            self.cfg.jaero_timeout_min,
            self.db.is_some(),
            self.cfg.trace_interval_ms as f64 / 1000.0,
            bincraft::GRID,
            bincraft::special_tiles_json(),
            env!("CARGO_PKG_VERSION")
        )
    }

    fn counts(&self, now: Ms) -> (usize, usize, [usize; 14]) {
        let (mut with, mut without, mut by) = (0, 0, [0usize; 14]);
        for s in self.store.shards() {
            for a in s.lock().map.values() {
                if !json::listed(a, now, self.cfg.list_window_ms) {
                    continue;
                }
                if a.pos.valid(now) {
                    with += 1;
                } else {
                    without += 1;
                }
                by[(a.addr_type as usize).min(13)] += 1;
            }
        }
        (with, without, by)
    }

    fn counts_json(&self, now: Ms) -> String {
        let (with, without, by) = self.counts(now);
        let names = ["adsb_icao", "adsb_icao_nt", "adsr_icao", "tisb_icao", "adsc", "mlat", "other", "mode_s", "adsb_other", "adsr_other", "tisb_trackfile", "tisb_other", "mode_ac", "unknown"];
        let by: Vec<String> = names.iter().zip(by).map(|(n, c)| format!("\"{n}\": {c}")).collect();
        format!(
            "\"aircraft_with_pos\": {with}, \"aircraft_without_pos\": {without}, \"aircraft_count_by_type\": {{ {} }}",
            by.join(", ")
        )
    }

    pub fn status_json(&self, now: Ms) -> String {
        format!(
            "{{ \"now\": {:.1}, \"uptime\": {:.1},\n{},\n\"protection\": {} }}\n",
            now as f64 / 1000.0,
            now.saturating_sub(self.started) as f64 / 1000.0,
            self.counts_json(now),
            self.guard.json(now)
        )
    }

    fn sample(&self, now: Ms, take_max: bool) -> Sample {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut c = [0u64; COUNTERS];
        c[C_MESSAGES] = self.store.messages();
        c[C_POSITIONS] = g(&self.stats.positions);
        c[C_BYTES_IN] = g(&self.stats.bytes_in);
        c[C_BAD] = g(&self.stats.bad);
        c[C_SBS] = g(&self.stats.sbs);
        c[C_BYTES_OUT] = g(&self.stats.bytes_out);
        for (i, a) in self.stats.pos_by_type.iter().enumerate() {
            c[C_TYPE0 + i] = g(a);
        }
        let max_dist = if take_max { self.stats.max_dist.swap(0, Ordering::Relaxed) } else { g(&self.stats.max_dist) };
        Sample { t: now, c, max_dist }
    }

    /// Prometheus text exposition of the counters.
    pub fn metrics(&self, now: Ms) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (with, without, _) = self.counts(now);
        let c = &self.guard.counters;
        let mut o = String::new();
        let mut put = |name: &str, kind: &str, help: &str, v: f64| {
            o.push_str(&format!("# HELP aggd_{name} {help}\n# TYPE aggd_{name} {kind}\naggd_{name} {v}\n"));
        };
        put("uptime_seconds", "gauge", "Seconds since start", now.saturating_sub(self.started) as f64 / 1000.0);
        put("messages_total", "counter", "Messages accepted", self.store.messages() as f64);
        put("positions_total", "counter", "Positions accepted", g(&self.stats.positions) as f64);
        put("bad_frames_total", "counter", "Frames that did not decode", g(&self.stats.bad) as f64);
        put("bytes_in_total", "counter", "Bytes received from feeders", g(&self.stats.bytes_in) as f64);
        put("bytes_out_total", "counter", "Bytes sent on output streams", g(&self.stats.bytes_out) as f64);
        put("output_skipped_total", "counter", "Batches of output that slow consumers did not get", g(&self.stats.out_skipped) as f64);
        put("aircraft_with_position", "gauge", "Aircraft listed with a current position", with as f64);
        put("aircraft_without_position", "gauge", "Aircraft listed without a current position", without as f64);
        put("aircraft_tracked", "gauge", "Aircraft in the table, including those kept for their trace", self.store.count.load(Ordering::Relaxed) as f64);
        let (mut pts, mut bytes, mut gone) = (0usize, 0usize, 0usize);
        for s in self.store.shards() {
            let s = s.lock();
            bytes += s.map.len() * std::mem::size_of::<crate::track::Aircraft>() + s.gone.len() * (std::mem::size_of::<crate::track::Gone>() + 8);
            gone += s.gone.len();
            for t in s.map.values().map(|a| &a.trace).chain(s.gone.values().map(|g| &g.trace)) {
                pts += t.len();
                bytes += t.bytes();
            }
        }
        put("aircraft_gone", "gauge", "Aircraft no longer heard, kept for their trace", gone as f64);
        if let Some((used, free)) = heap() {
            put("heap_used_bytes", "gauge", "Memory in use, as the allocator counts it", used as f64);
            put("heap_free_bytes", "gauge", "Memory the allocator holds but has not handed out", free as f64);
        }
        put("trace_points", "gauge", "Trace points held in memory", pts as f64);
        put("table_bytes", "gauge", "Memory held by the aircraft table and its traces", bytes as f64);
        put("feeder_connections", "gauge", "Input connections open", self.guard.connections() as f64);
        put("receivers", "gauge", "Receivers heard in the last hour", self.reg.receivers.lock().len() as f64);
        put("receivers_distrusted", "gauge", "Receivers whose positions are currently ignored", self.reg.distrusted(now).len() as f64);
        put("connections_refused_total", "counter", "Connections refused: banned address or limit", (g(&c.refused_banned) + g(&c.refused_limit)) as f64);
        put("bans_total", "counter", "Addresses banned", g(&c.bans) as f64);
        put("messages_over_rate_total", "counter", "Messages dropped for exceeding the per-link rate", g(&c.rate_dropped) as f64);
        put("garbage_frames_total", "counter", "Frames failing a checksum a receiver should have checked", g(&c.garbage) as f64);
        put("aircraft_refused_total", "counter", "New aircraft refused: per-link or table limit", g(&c.aircraft_refused) as f64);
        put("distrusted_positions_total", "counter", "Position messages ignored from distrusted receivers", g(&c.distrusted_positions) as f64);
        o
    }

    /// Record a sample for the statistics windows; every ten seconds.
    pub fn take_sample(&self, now: Ms) {
        let s = self.sample(now, true);
        let mut q = self.stats.samples.lock();
        q.push_back(s);
        while q.len() > 91 {
            q.pop_front();
        }
    }

    /// Messages per second since the last statistics sample.
    pub fn message_rate(&self) -> f64 {
        let now = now_ms();
        let last = self.stats.samples.lock().iter().rev().find(|s| now.saturating_sub(s.t) >= 5000).map(|s| (s.t, s.c[C_MESSAGES])).unwrap_or((self.started, 0));
        let dt = now.saturating_sub(last.0).max(1) as f64 / 1000.0;
        self.store.messages().saturating_sub(last.1) as f64 / dt
    }

    pub fn stats_json(&self, now: Ms) -> String {
        const NAMES: [&str; 14] = ["adsb_icao", "adsb_icao_nt", "adsr_icao", "tisb_icao", "adsc", "mlat", "other", "mode_s", "adsb_other", "adsr_other", "tisb_trackfile", "tisb_other", "mode_ac", "unknown"];
        let cur = self.sample(now, false);
        let samples = self.stats.samples.lock();
        let zero = Sample { t: self.started, c: [0; COUNTERS], max_dist: 0 };
        let window = |name: &str, span: Ms| {
            // the oldest sample inside the window, or the start of time
            let base = match samples.front() {
                // history shorter than the window: it starts at the start
                Some(first) if span != Ms::MAX && now.saturating_sub(first.t) > span => {
                    samples.iter().find(|s| now.saturating_sub(s.t) <= span).copied().unwrap_or(zero)
                }
                _ => zero,
            };
            let d = |i: usize| cur.c[i] - base.c[i];
            let far = samples.iter().filter(|s| s.t > base.t).map(|s| s.max_dist).max().unwrap_or(0).max(cur.max_dist);
            let by: Vec<String> = NAMES.iter().enumerate().map(|(i, n)| format!("\"{n}\": {}", d(C_TYPE0 + i))).collect();
            let modes = d(C_MESSAGES) - d(C_SBS).min(d(C_MESSAGES));
            format!(
                "\"{name}\":{{\"start\":{:.1},\"end\":{:.1},\"messages_valid\": {m},\"position_count_total\": {p},\"position_count_by_type\": {{{}}},\"remote\":{{\"modeac\":0,\"modes\":{modes},\"basestation\": {},\"bad\":{},\"unknown_icao\":0,\"accepted\":[{modes},0],\"bytes_in\": {},\"bytes_out\": {}}},\"cpr\":{{\"surface\":0,\"airborne\":{p},\"global_ok\":{p},\"global_bad\":0,\"global_range\":0,\"global_speed\":0,\"global_skipped\":0,\"local_ok\":0,\"local_aircraft_relative\":0,\"local_receiver_relative\":0,\"local_skipped\":0,\"local_range\":0,\"local_speed\":0,\"filtered\":0}},\"altitude_suppressed\":0,\"cpu\":{{\"demod\":0,\"reader\":0,\"background\":0,\"aircraft_json\":0,\"globe_json\":0,\"binCraft\":0,\"trace_json\":0,\"heatmap_and_state\":0,\"api_workers\":0,\"api_update\":0,\"remove_stale\":0}},\"tracks\":{{\"all\":0,\"single_message\":0}},\"messages\":{m},\"max_distance\":{far}}}",
                base.t as f64 / 1000.0,
                now as f64 / 1000.0,
                by.join(","),
                d(C_SBS),
                d(C_BAD),
                d(C_BYTES_IN),
                d(C_BYTES_OUT),
                m = d(C_MESSAGES),
                p = d(C_POSITIONS),
            )
        };
        format!(
            "{{ \"now\" : {:.1},\n{},\n{},\n{},\n{},\n{}\n}}\n",
            now as f64 / 1000.0,
            self.counts_json(now),
            window("last1min", 60_000),
            window("last5min", 300_000),
            window("last15min", 900_000),
            window("total", Ms::MAX)
        )
    }
}

fn write_atomic(dir: &str, name: &str, body: &str) -> std::io::Result<()> {
    let tmp = format!("{dir}/.{name}.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, format!("{dir}/{name}"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let threads = cli.threads.unwrap_or(2).max(1);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).max_blocking_threads(8).enable_all().build()?;
    rt.block_on(run(cli))?;
    // Everything that had to reach the disk has. Leaving through the
    // runtime's own shutdown would wait for whatever housekeeping is in
    // flight and then free the table a buffer at a time.
    std::process::exit(0)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    anyhow::ensure!(!cli.write_traces || cli.write_json.is_some(), "--write-traces writes beside --write-json, which is not set");
    let uuid = match (&cli.uuid, &cli.uuid_file) {
        (Some(u), _) => Some(u.clone()),
        (None, Some(f)) => std::fs::read_to_string(f).ok().map(|s| s.trim().to_string()),
        _ => None,
    };
    let db = match cli.db_file.as_deref() {
        None | Some("none") | Some("") => None,
        Some(path) => match db::Db::load(path) {
            Ok(d) => {
                eprintln!("aggd: database {path}: {} airframes", d.len());
                Some(d)
            }
            Err(e) => {
                eprintln!("aggd: database {path} not loaded: {e}");
                None
            }
        },
    };
    track::JAERO_TTL.store((cli.jaero_timeout * 60_000.0) as u64, Ordering::Relaxed);
    let cfg = Config {
        site: cli.lat.zip(cli.lon),
        reduce_interval_ms: (cli.net_beast_reduce_interval * 1000.0) as Ms,
        json_port_interval_ms: (cli.net_json_port_interval * 1000.0) as Ms,
        json_port_noposition: cli.net_json_port_include_noposition,
        json_every_ms: ((cli.write_json_every * 1000.0) as Ms).max(100),
        trace_interval_ms: ((cli.json_trace_interval * 1000.0) as Ms).max(1000),
        trace_recent_points: 142,
        trace_keep_ms: (cli.trace_hours * 3_600_000.0) as Ms,
        list_window_ms: ((cli.interactive_ttl * 1000.0) as Ms).max(1000),
        forward_receiver_id: cli.net_receiver_id,
        uuid,
        long_db: cli.db_file_lt,
        jaero_timeout_min: cli.jaero_timeout,
        receiver_count: cli.json_receiver_count,
        forward_mlat: cli.forward_mlat,
        forward_mlat_sbs: cli.forward_mlat_sbs,
        sbs_reduce: cli.net_sbs_reduce,
        web_root: cli.web_root.clone().map(|d| d.trim_end_matches('/').to_string()),
        history_dir: cli.write_globe_history.clone().map(|d| d.trim_end_matches('/').to_string()),
        cors: http::Cors::parse(&cli.cors_origin).map_err(|e| anyhow::anyhow!("--cors-origin: {e}"))?,
    };
    let mut trusted = Vec::new();
    for t in &cli.net_trusted {
        trusted.push(guard::Cidr::parse(t).ok_or_else(|| anyhow::anyhow!("--net-trusted: {t} is not an address or network"))?);
    }
    let limits = guard::Limits {
        max_connections: cli.max_connections,
        max_per_ip: cli.max_connections_per_ip,
        max_rate: cli.max_receiver_rate.max(1.0),
        max_new_aircraft: cli.max_new_aircraft,
        max_aircraft: cli.max_aircraft,
        trusted,
    };
    let ctx = Arc::new(Ctx {
        cfg,
        store: Store::new(),
        reg: receivers::Registry::default(),
        hub: net::Hub::new(),
        db,
        stats: Stats::default(),
        cache: http::Cache::default(),
        guard: guard::Guard::new(limits),
        heat: heatmap::Heatmap::default(),
        state: persist::StateFiles::default(),
        started: now_ms(),
    });

    if let Some(dir) = &cli.write_state {
        match persist::load_state(&ctx, dir, now_ms()) {
            Ok(n) => eprintln!("aggd: restored the traces of {n} aircraft from {dir}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("aggd: state in {dir} not restored: {e}"),
        }
        // the file was read whole; give that space back
        release_memory();
    }

    // an IPv6 address needs brackets before a port
    let bind = &if cli.net_bind_address.contains(':') && !cli.net_bind_address.starts_with('[') {
        format!("[{}]", cli.net_bind_address)
    } else {
        cli.net_bind_address.clone()
    };
    for (ports, proto) in [
        (&cli.net_bi_port, Proto::BeastIn),
        (&cli.net_bo_port, Proto::BeastOut),
        (&cli.net_beast_reduce_out_port, Proto::BeastReduceOut),
        (&cli.net_sbs_port, Proto::SbsOut),
        (&cli.net_sbs_in_port, Proto::SbsIn),
        (&cli.net_sbs_jaero_in_port, Proto::SbsInJaero),
        (&cli.net_uat_in_port, Proto::UatIn),
        (&cli.net_sbs_jaero_port, Proto::SbsJaeroOut),
        (&cli.net_json_port, Proto::JsonOut),
    ] {
        for port in ports.iter().filter(|p| **p != 0) {
            net::listen(ctx.clone(), format!("{bind}:{port}"), proto).await?;
        }
    }
    for port in cli.http_port.iter().chain(cli.net_api_port.iter()).filter(|p| **p != 0) {
        http::listen(ctx.clone(), format!("{bind}:{port}")).await?;
    }
    for c in &cli.net_connector {
        let parts: Vec<&str> = c.split(',').map(|p| p.trim()).collect();
        anyhow::ensure!(parts.len() >= 3, "--net-connector wants host,port,protocol: {c}");
        let port: u16 = parts[1].parse().map_err(|_| anyhow::anyhow!("bad port in --net-connector {c}"))?;
        let proto = Proto::parse(parts[2]).ok_or_else(|| anyhow::anyhow!("unknown protocol {} in --net-connector {c}", parts[2]))?;
        // readsb's fourth field onward: uuid=... for this connection
        let uuid = parts[3..].iter().find_map(|x| x.strip_prefix("uuid=")).map(str::to_string);
        net::connect(ctx.clone(), parts[0].to_string(), port, proto, uuid);
    }

    // housekeeping, once a second
    let hk = ctx.clone();
    let (hist_dir, state_dir) = (cli.write_globe_history.clone(), cli.write_state.clone());
    let heat_dir = cli.heatmap_dir.clone().or_else(|| cli.write_globe_history.clone());
    tune_allocator();
    let heat_every = cli.heatmap.map(|n| n.clamp(5, 1800) as u64);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            n += 1;
            let now = now_ms();
            hk.reg.tick(now, 1.0);
            if n % 10 == 0 {
                let c = hk.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let keep = c.cfg.trace_keep_ms;
                    c.store.expire(now, c.cfg.list_window_ms.max(300_000), keep);
                    c.take_sample(now);
                    if n % 600 == 0 {
                        release_memory();
                    }
                })
                .await;
            }
            // history: the whole table every ten minutes, a shard at a
            // time. A day file is rewritten whole each time its trace has
            // grown, so the cost of a pass grows with the day: written
            // every minute, it was about half of all CPU time with eight
            // hours of traces in memory (measured side by side, live). Shutdown writes what is left.
            if let Some(dir) = &hist_dir {
                let at = |k: u64| (k % HISTORY_PASS_S) as usize * track::SHARDS / HISTORY_PASS_S as usize;
                let (from, to) = (at(n - 1), at(n));
                if from != to {
                    let (c, dir) = (hk.clone(), dir.clone());
                    let _ = tokio::task::spawn_blocking(move || persist::write_history(&c, &dir, to, to + 1)).await;
                }
            }
            if let (Some(dir), Some(every)) = (&heat_dir, heat_every) {
                if n % every == 0 {
                    let (c, dir) = (hk.clone(), dir.clone());
                    let _ = tokio::task::spawn_blocking(move || c.heat.sample(&c, &dir, now, every * 1000)).await;
                }
            }
            // The state, whole: a minute after starting, then every hour.
            // Stopping then only has to write what was gained since (the
            // first of them soon, so that this holds from the start, also
            // when what was restored came from an earlier version's file).
            if n % 3600 == 60 {
                if let Some(dir) = &state_dir {
                    let (c, dir) = (hk.clone(), dir.clone());
                    // (not awaited: on a slow disk a gigabyte takes a
                    // while, and housekeeping goes on meanwhile)
                    tokio::task::spawn_blocking(move || {
                        match persist::save_state(&c, &dir, false) {
                            Err(e) if e.kind() != std::io::ErrorKind::Interrupted => eprintln!("aggd: state not saved: {e}"),
                            _ => {}
                        }
                        release_memory();
                    });
                }
            }
        }
    });

    // the trace files on disk: a quarter of the table each second, so an
    // aircraft adding points has its trace_recent rewritten every few
    // seconds; first, files no aircraft kept answers for are removed, and
    // again every ten minutes as traces age out
    if let (true, Some(dir)) = (cli.write_traces, cli.write_json.clone()) {
        let c = ctx.clone();
        tokio::spawn(async move {
            if (0..256).any(|i| std::fs::create_dir_all(format!("{dir}/traces/{i:02x}")).is_err()) {
                eprintln!("aggd: --write-traces: cannot create {dir}/traces");
            }
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let step = track::SHARDS / persist::TRACE_PASS_S as usize;
            let (mut n, mut failing) = (0u64, false);
            loop {
                tick.tick().await;
                let (c, dir2, sweep) = (c.clone(), dir.clone(), n % 600 == 0);
                let from = (n % persist::TRACE_PASS_S) as usize * step;
                n += 1;
                let res = tokio::task::spawn_blocking(move || {
                    if sweep {
                        persist::sweep_traces(&c, &dir2);
                    }
                    persist::write_traces(&c, &dir2, from, from + step)
                })
                .await;
                // say so once when it stops working, and once when it works again
                match res {
                    Ok((_, failed)) if failed > 0 && !failing => {
                        failing = true;
                        eprintln!("aggd: --write-traces: cannot write to {dir}/traces");
                    }
                    Ok((written, 0)) if written > 0 && failing => {
                        failing = false;
                        eprintln!("aggd: --write-traces: writing again");
                    }
                    _ => {}
                }
            }
        });
    }

    // the metrics as a file, as readsb's --write-prom writes them
    if let Some(path) = cli.write_prom.clone() {
        let c = ctx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(c.cfg.json_every_ms));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut failing = false;
            loop {
                tick.tick().await;
                let body = c.metrics(now_ms());
                let (dir, name) = path.rsplit_once('/').unwrap_or((".", &path));
                let ok = write_atomic(dir, name, &body).is_ok();
                if ok == failing {
                    failing = !ok;
                    eprintln!("aggd: --write-prom {path}: {}", if ok { "writing again" } else { "cannot write" });
                }
            }
        });
    }

    // the JSON files on disk, for whatever reads them from there
    if let Some(dir) = cli.write_json.clone() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("aggd: --write-json {dir}: {e}");
        }
        let c = ctx.clone();
        let every = Duration::from_millis(c.cfg.json_every_ms);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut failing = false;
            loop {
                tick.tick().await;
                let (c, dir) = (c.clone(), dir.clone());
                let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    let now = now_ms();
                    write_atomic(&dir, "aircraft.json", &json::aircraft_json(&c.store, &c.view(now), c.cfg.list_window_ms))?;
                    write_atomic(&dir, "clients.json", &c.reg.clients_json(now))?;
                    write_atomic(&dir, "receivers.json", &c.reg.receivers_json(now))?;
                    write_atomic(&dir, "stats.json", &c.stats_json(now))?;
                    write_atomic(&dir, "status.json", &c.status_json(now))?;
                    write_atomic(&dir, "receiver.json", &c.receiver_json())
                })
                .await;
                // say so once when it stops working, and once when it works again
                match res {
                    Ok(Err(e)) if !failing => {
                        failing = true;
                        eprintln!("aggd: --write-json: cannot write: {e}");
                    }
                    Ok(Ok(())) if failing => {
                        failing = false;
                        eprintln!("aggd: --write-json: writing again");
                    }
                    _ => {}
                }
            }
        });
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    eprintln!("aggd: stopping");
    // The state first: it is a second or two (what the traces gained
    // since the last hourly save) and with it nothing is lost, the points
    // no day file has yet included neither. Should the time allowed for
    // stopping run out during the day files, the next start writes them.
    ctx.state.stopping.store(true, Ordering::Relaxed);
    if let Some(dir) = &cli.write_state {
        let t = std::time::Instant::now();
        match persist::save_state(&ctx, dir, true) {
            Ok(n) => eprintln!("aggd: saved the traces of {n} aircraft to {dir} in {:.1} s", t.elapsed().as_secs_f64()),
            Err(e) => eprintln!("aggd: state not saved to {dir}: {e}"),
        }
    }
    if let Some(dir) = &cli.write_globe_history {
        let t = std::time::Instant::now();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let files = persist::write_history_all(&ctx, dir, threads);
        eprintln!("aggd: wrote {files} trace files to {dir} in {:.1} s", t.elapsed().as_secs_f64());
    }
    Ok(())
}

// The table is many small buffers that live for a day, written beside
// megabyte strings that live for a second. With glibc's defaults each
// thread keeps its own heap, and the freed strings leave them all full
// of holes: over eight hours of live traffic the process held 123 MB for
// 20 MB of table. Two heaps and a periodic hand-back keep it near what
// is in use. Other C libraries need neither.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" {
    fn mallopt(param: i32, value: i32) -> i32;
    fn malloc_trim(pad: usize) -> i32;
    fn mallinfo2() -> MallInfo;
}

/// glibc's `struct mallinfo2`: ten counters.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[repr(C)]
struct MallInfo([usize; 10]);

/// (bytes the allocator has handed out, bytes it holds free), where the
/// C library can say.
fn heap() -> Option<(usize, usize)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo2 returns ten counters by value and changes nothing.
        let m = unsafe { mallinfo2() }.0;
        // in use in the heaps and in separate mappings; free in the heaps
        return Some((m[7] + m[4], m[8]));
    }
    #[allow(unreachable_code)]
    None
}

/// Seconds over which every trace that has grown reaches its day file.
const HISTORY_PASS_S: u64 = 600;

fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("AGGD_ALLOCATOR_DEFAULTS").is_none() {
        const M_ARENA_MAX: i32 = -8;
        // SAFETY: mallopt only sets a number in the allocator's settings.
        unsafe { mallopt(M_ARENA_MAX, 2) };
        // Left alone, glibc raises the size above which a buffer gets its
        // own mapping each time a large one is freed, until the megabyte
        // strings of the outputs are cut from the heaps too. Fixed, they
        // go back to the system when freed. (Measured live with a day of
        // traces: held-but-free memory grew half as fast.)
        const M_MMAP_THRESHOLD: i32 = -3;
        unsafe { mallopt(M_MMAP_THRESHOLD, 128 * 1024) };
    }
}

fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("AGGD_ALLOCATOR_DEFAULTS").is_none() {
        // SAFETY: malloc_trim takes the allocator's own locks and returns
        // free pages to the system; it touches nothing in use.
        unsafe { malloc_trim(0) };
    }
}
