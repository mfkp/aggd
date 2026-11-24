//! A small HTTP/1.1 server for the files tar1090 and API clients fetch.
//! Everything is rendered from memory on request and cached for the
//! refresh interval; nothing is written to disk to be served.

use crate::track::Ms;
use crate::{api, json, now_ms, Ctx};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Access-Control-Allow-Origin on every answer (--cors-origin).
#[derive(Clone, Debug, PartialEq)]
pub enum Cors {
    /// `*`: any page may read the data (the default)
    Any,
    /// no header: a proxy in front sets its own, or no browser reads us
    Off,
    /// only these origins, each answered with its own name
    Origins(Vec<String>),
}

impl Cors {
    /// `*`, `none`, or origins separated by commas (scheme://host[:port]).
    pub fn parse(s: &str) -> Result<Cors, String> {
        match s.trim() {
            "*" => return Ok(Cors::Any),
            "none" | "" => return Ok(Cors::Off),
            _ => {}
        }
        let mut origins = Vec::new();
        for o in s.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            let host = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://"));
            match host {
                Some(h) if !h.is_empty() && !h.contains(['/', ' ', '*']) => origins.push(o.to_ascii_lowercase()),
                _ => return Err(format!("{o} is not an origin (scheme://host[:port], no path)")),
            }
        }
        Ok(Cors::Origins(origins))
    }

    /// The header lines for an answer to a request from `origin`.
    fn headers(&self, origin: Option<&str>) -> String {
        match self {
            Cors::Any => "Access-Control-Allow-Origin: *\r\nVary: Accept-Encoding\r\n".into(),
            Cors::Off => "Vary: Accept-Encoding\r\n".into(),
            // the answer now depends on who asks, so caches must keep them apart
            Cors::Origins(list) => match origin.filter(|o| list.iter().any(|l| l == o)) {
                Some(o) => format!("Access-Control-Allow-Origin: {o}\r\nVary: Accept-Encoding, Origin\r\n"),
                None => "Vary: Accept-Encoding, Origin\r\n".into(),
            },
        }
    }
}

#[derive(Clone)]
struct Entry {
    at: Ms,
    plain: Arc<Vec<u8>>,
    gz: Option<Arc<Vec<u8>>>,
    zst: Option<Arc<Vec<u8>>>,
}

#[derive(Default)]
pub struct Cache {
    map: Mutex<HashMap<String, Entry>>,
    tiles: Mutex<Tiles>,
}

/// Globe tiles are cut in one pass over the table and compressed one
/// by one as they are asked for.
#[derive(Default)]
struct Tiles {
    at: Ms,
    all: HashMap<u16, Tile>,
    mil: HashMap<u16, Tile>,
}

struct Tile {
    raw: Arc<Vec<u8>>,
    zst: Option<Arc<Vec<u8>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Plain,
    Gzip,
    Zstd,
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 4), flate2::Compression::new(1));
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

impl Cache {
    /// The document `name`, regenerated when older than `ttl`, in the
    /// best encoding the client takes.
    fn get(&self, name: &str, ttl: Ms, enc: Enc, make: impl FnOnce() -> Vec<u8>) -> Arc<Vec<u8>> {
        let now = now_ms();
        let mut m = self.map.lock();
        let fresh = m.get(name).map_or(false, |e| now.saturating_sub(e.at) < ttl);
        if !fresh {
            m.insert(name.to_string(), Entry { at: now, plain: Arc::new(make()), gz: None, zst: None });
        }
        let e = m.get_mut(name).unwrap();
        match enc {
            Enc::Plain => e.plain.clone(),
            Enc::Gzip => e.gz.get_or_insert_with(|| Arc::new(gzip(&e.plain))).clone(),
            Enc::Zstd => e.zst.get_or_insert_with(|| Arc::new(zstd::bulk::compress(&e.plain, 1).unwrap_or_default())).clone(),
        }
    }
}

struct Resp {
    status: u16,
    ctype: &'static str,
    body: Arc<Vec<u8>>,
    enc: Enc,
}

fn text(status: u16, s: &str) -> Resp {
    Resp { status, ctype: "text/plain", body: Arc::new(s.as_bytes().to_vec()), enc: Enc::Plain }
}

/// Compress a one-off body if the client takes it and it is worth it.
fn dynamic(bytes: Vec<u8>, ctype: &'static str, enc: Enc) -> Resp {
    if bytes.len() < 1024 || ctype == "application/zstd" {
        return Resp { status: 200, ctype, body: Arc::new(bytes), enc: Enc::Plain };
    }
    let (body, enc) = match enc {
        Enc::Zstd => (zstd::bulk::compress(&bytes, 1).unwrap_or_default(), Enc::Zstd),
        Enc::Gzip => (gzip(&bytes), Enc::Gzip),
        Enc::Plain => (bytes, Enc::Plain),
    };
    Resp { status: 200, ctype, body: Arc::new(body), enc }
}

impl Cache {
    /// One globe tile, raw or zstd-compressed.
    fn tile(&self, ctx: &Ctx, now: Ms, index: u16, mil: bool, zst: bool) -> Arc<Vec<u8>> {
        let mut t = self.tiles.lock();
        if now.saturating_sub(t.at) >= ctx.cfg.json_every_ms {
            let (all, m) = crate::bincraft::tiles(ctx, now);
            let wrap = |m: HashMap<u16, Vec<u8>>| m.into_iter().map(|(k, v)| (k, Tile { raw: Arc::new(v), zst: None })).collect();
            t.all = wrap(all);
            t.mil = wrap(m);
            t.at = now;
        }
        let at = t.at;
        let map = if mil { &mut t.mil } else { &mut t.all };
        match map.get_mut(&index) {
            Some(tile) if zst => tile.zst.get_or_insert_with(|| Arc::new(zstd::bulk::compress(&tile.raw, 1).unwrap_or_default())).clone(),
            Some(tile) => tile.raw.clone(),
            None => {
                let raw = crate::bincraft::empty(ctx, at, index as u32);
                Arc::new(if zst { zstd::bulk::compress(&raw, 1).unwrap_or_default() } else { raw })
            }
        }
    }
}

fn route(ctx: &Ctx, path: &str, query: &str, enc: Enc) -> Resp {
    let now = now_ms();
    let p = path.strip_prefix("/data").unwrap_or(path);
    let cached = |name: &'static str, ttl: Ms, make: &dyn Fn() -> String| Resp {
        status: 200,
        ctype: "application/json",
        body: ctx.cache.get(name, ttl, enc, || make().into_bytes()),
        enc,
    };
    let binary = |body: Arc<Vec<u8>>, zst: bool| Resp {
        status: 200,
        ctype: if zst { "application/zstd" } else { "application/octet-stream" },
        body,
        enc: Enc::Plain,
    };
    match p {
        // with a web interface, / is the page (its query string is the
        // page's own) and the API is at /re-api/
        "/" | "" if ctx.cfg.web_root.is_some() => static_file(ctx, "/index.html", enc).unwrap_or_else(|| text(404, "not found\n")),
        "/" | "" | "/re-api" | "/re-api/" => match api::query(ctx, query, now) {
            Ok((body, ctype)) => dynamic(body, ctype, enc),
            Err(e) => text(400, &format!("{e}\n")),
        },
        "/aircraft.json" => cached("aircraft", ctx.cfg.json_every_ms, &|| {
            json::aircraft_json(&ctx.store, &ctx.view(now), ctx.cfg.list_window_ms)
        }),
        "/receiver.json" => cached("receiver", 60_000, &|| ctx.receiver_json()),
        "/clients.json" => cached("clients", 1000, &|| ctx.reg.clients_json(now)),
        "/receivers.json" => cached("receivers", 1000, &|| ctx.reg.receivers_json(now)),
        "/stats.json" => cached("stats", 1000, &|| ctx.stats_json(now)),
        "/status.json" => cached("status", 1000, &|| ctx.status_json(now)),
        "/health" => text(200, "ok\n"),
        "/metrics" => text(200, &ctx.metrics(now)),
        "/aircraft.binCraft" => binary(ctx.cache.get("bincraft", ctx.cfg.json_every_ms, Enc::Plain, || crate::bincraft::whole(ctx, now)), false),
        "/aircraft.binCraft.zst" => binary(ctx.cache.get("bincraft", ctx.cfg.json_every_ms, Enc::Zstd, || crate::bincraft::whole(ctx, now)), true),
        _ => {
            if let Some(r) = trace(ctx, p, enc) {
                return r;
            }
            // /globe_<index>.binCraft[.zst], /globeMil_<index>.binCraft[.zst]
            if let Some(rest) = p.strip_prefix("/globe") {
                let (mil, rest) = match rest.strip_prefix("Mil_") {
                    Some(r) => (true, r),
                    None => (false, rest.strip_prefix('_').unwrap_or("")),
                };
                let (name, zst) = match rest.strip_suffix(".zst") {
                    Some(n) => (n, true),
                    None => (rest, false),
                };
                if let Some(Ok(index)) = name.strip_suffix(".binCraft").map(|n| n.parse::<u16>()) {
                    return binary(ctx.cache.tile(ctx, now, index, mil, zst), zst);
                }
            }
            if let Some(r) = history_file(ctx, path) {
                return r;
            }
            static_file(ctx, path, enc).unwrap_or_else(|| text(404, "not found\n"))
        }
    }
}

/// A path is safe to join to a directory when it has no way out of it.
fn safe(path: &str) -> bool {
    !path.contains("..") && !path.contains('\\') && !path.contains('\0') && path.starts_with('/')
}

/// /globe_history/YYYY/MM/DD/traces/xx/trace_full_HEX.json: the per-day
/// files, stored gzipped and sent as they are.
fn history_file(ctx: &Ctx, path: &str) -> Option<Resp> {
    let rest = path.strip_prefix("/globe_history")?;
    let dir = ctx.cfg.history_dir.as_ref()?;
    if !safe(rest) {
        return None;
    }
    // tar1090 asks for /YYYY-MM-DD/, the files live under /YYYY/MM/DD/
    let b = rest.as_bytes();
    let rest = if b.len() > 11 && b[5] == b'-' && b[8] == b'-' && b[11] == b'/' {
        format!("{}/{}/{}{}", &rest[..5], &rest[6..8], &rest[9..11], &rest[11..])
    } else {
        rest.to_string()
    };
    let body = std::fs::read(format!("{dir}{rest}")).ok()?;
    let enc = if body.starts_with(&[0x1f, 0x8b]) { Enc::Gzip } else { Enc::Plain };
    Some(Resp { status: 200, ctype: "application/json", body: Arc::new(body), enc })
}

/// A file of the web interface (tar1090's html directory, for one).
fn static_file(ctx: &Ctx, path: &str, enc: Enc) -> Option<Resp> {
    let root = ctx.cfg.web_root.as_ref()?;
    if !safe(path) {
        return None;
    }
    let mut full = format!("{root}{path}");
    if full.ends_with('/') {
        full.push_str("index.html");
    }
    let meta = std::fs::metadata(&full).ok()?;
    if meta.is_dir() || meta.len() > 64 << 20 {
        return None;
    }
    let body = std::fs::read(&full).ok()?;
    let ctype = match full.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "json" | "geojson" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    // tar1090 ships its database files already gzipped under .js names
    if body.starts_with(&[0x1f, 0x8b]) && !full.ends_with(".gz") {
        return Some(Resp { status: 200, ctype, body: Arc::new(body), enc: Enc::Gzip });
    }
    let compressible = ctype.starts_with("text/") || ctype.starts_with("application/j") || ctype == "image/svg+xml";
    if !compressible || body.len() < 1024 {
        return Some(Resp { status: 200, ctype, body: Arc::new(body), enc: Enc::Plain });
    }
    Some(dynamic(body, ctype, enc))
}

/// /traces/<last two hex digits>/trace_(full|recent)_<hex>.json
fn trace(ctx: &Ctx, p: &str, enc: Enc) -> Option<Resp> {
    let (recent, addr) = crate::trace::file_addr(p.strip_prefix("/traces/")?.split('/').nth(1)?)?;
    let mut out = String::new();
    {
        let s = ctx.store.shard(addr).lock();
        // tracked now, or gone silent with its trace kept
        let (trace, db) = match s.map.get(&addr) {
            Some(a) => (&a.trace, a.db.clone()),
            None => (&s.gone.get(&addr)?.trace, ctx.db.as_ref().and_then(|d| d.describe(addr))),
        };
        let (pts, details) = if recent { trace.last(ctx.cfg.trace_recent_points) } else { trace.all() };
        crate::trace::render(addr, db.as_deref(), &pts, &details, &mut out);
    }
    Some(dynamic(out.into_bytes(), "application/json", enc))
}

async fn conn(ctx: Arc<Ctx>, mut s: TcpStream) {
    let _ = s.set_nodelay(true);
    let mut buf = Vec::with_capacity(2048);
    loop {
        // read one request head
        let end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > 16 * 1024 {
                return;
            }
            let mut chunk = [0u8; 2048];
            match tokio::time::timeout(Duration::from_secs(75), s.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end);
        let mut lines = head.split("\r\n");
        let mut rl = lines.next().unwrap_or("").split(' ');
        let (method, target) = (rl.next().unwrap_or(""), rl.next().unwrap_or("/"));
        let mut enc = Enc::Plain;
        let mut close = false;
        let mut origin = None;
        for l in lines {
            let Some((k, v)) = l.split_once(':') else { continue };
            let v = v.trim().to_ascii_lowercase();
            if k.eq_ignore_ascii_case("accept-encoding") {
                enc = if v.contains("zstd") {
                    Enc::Zstd
                } else if v.contains("gzip") {
                    Enc::Gzip
                } else {
                    Enc::Plain
                };
            } else if k.eq_ignore_ascii_case("connection") && v.contains("close") {
                close = true;
            } else if k.eq_ignore_ascii_case("origin") {
                origin = Some(v);
            }
        }
        let resp = if method != "GET" && method != "HEAD" {
            text(405, "method not allowed\n")
        } else {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            // rendering holds table locks briefly; keep it off the reactor's back
            let (c, p, q) = (ctx.clone(), path.to_string(), query.to_string());
            match tokio::task::spawn_blocking(move || route(&c, &p, &q, enc)).await {
                Ok(r) => r,
                Err(_) => text(500, "error\n"),
            }
        };
        let reason = match resp.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Error",
        };
        let mut h = format!(
            "HTTP/1.1 {} {}\r\nServer: aggd\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{}",
            resp.status,
            reason,
            resp.ctype,
            resp.body.len(),
            ctx.cfg.cors.headers(origin.as_deref())
        );
        match resp.enc {
            Enc::Gzip => h.push_str("Content-Encoding: gzip\r\n"),
            Enc::Zstd => h.push_str("Content-Encoding: zstd\r\n"),
            Enc::Plain => {}
        }
        h.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
        if s.write_all(h.as_bytes()).await.is_err() {
            return;
        }
        if method != "HEAD" && s.write_all(&resp.body).await.is_err() {
            return;
        }
        if close {
            return;
        }
    }
}

pub async fn listen(ctx: Arc<Ctx>, addr: String) -> anyhow::Result<()> {
    let l = TcpListener::bind(&addr).await.map_err(|e| anyhow::anyhow!("bind {addr} (http): {e}"))?;
    eprintln!("aggd: HTTP on {addr}");
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            // a browser holds a handful of connections; thousands is a flood
            static OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if OPEN.load(std::sync::atomic::Ordering::Relaxed) >= 4096 {
                continue;
            }
            OPEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let c = ctx.clone();
            tokio::spawn(async move {
                conn(c, s).await;
                OPEN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_origin_settings() {
        assert_eq!(Cors::parse("*"), Ok(Cors::Any));
        assert_eq!(Cors::parse("none"), Ok(Cors::Off));
        assert_eq!(
            Cors::parse("https://map.example.org, http://localhost:8080"),
            Ok(Cors::Origins(vec!["https://map.example.org".into(), "http://localhost:8080".into()]))
        );
        assert!(Cors::parse("map.example.org").is_err());
        assert!(Cors::parse("https://map.example.org/").is_err());
        assert!(Cors::parse("https://*.example.org").is_err());
    }

    #[test]
    fn cors_headers_by_setting() {
        // as before: any page, whoever asks
        assert_eq!(Cors::Any.headers(None), "Access-Control-Allow-Origin: *\r\nVary: Accept-Encoding\r\n");
        assert_eq!(Cors::Any.headers(Some("https://a.example")), Cors::Any.headers(None));
        // behind a proxy that sets its own: never a second value
        assert!(!Cors::Off.headers(Some("https://a.example")).contains("Access-Control"));
        // a list: the asking origin when it is on it, nothing otherwise
        let list = Cors::parse("https://a.example,https://b.example").unwrap();
        assert_eq!(list.headers(Some("https://b.example")), "Access-Control-Allow-Origin: https://b.example\r\nVary: Accept-Encoding, Origin\r\n");
        assert_eq!(list.headers(Some("https://evil.example")), "Vary: Accept-Encoding, Origin\r\n");
        assert_eq!(list.headers(None), "Vary: Accept-Encoding, Origin\r\n");
    }
}
