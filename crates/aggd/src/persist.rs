//! What outlives the process: per-day trace files in tar1090's
//! globe_history layout, and a state file so traces survive a restart.

use crate::trace::{self, Packed, Point};
use crate::track::Ms;
use crate::Ctx;
use std::io::{Read, Seek, Write};

/// UTC calendar date of a timestamp.
pub fn civil(ms: Ms) -> (i64, i64, i64) {
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

const DAY: Ms = 86_400_000;

/// One day's trace document as text: for each point its time, in tenths
/// of a second since the epoch, and its row after the time. aggd's own
/// files and readsb's both read this way.
fn day_rows(doc: &str) -> Option<Vec<(u64, &str)>> {
    let at = doc.find("\"timestamp\":")? + 12;
    let ts: f64 = doc[at..at + doc[at..].find(|c| c == ',' || c == '}')?].trim().parse().ok()?;
    let trace = doc.find("\"trace\":")?;
    let open = trace + doc[trace..].find('[')? + 1;
    let (mut depth, mut quoted, mut escaped, mut start) = (0usize, false, false, 0usize);
    let mut rows = Vec::new();
    for (i, c) in doc.bytes().enumerate().skip(open) {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => quoted = true,
            b'[' | b'{' => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            // the end of the trace itself
            b']' | b'}' if depth == 0 => break,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let (t, rest) = doc[start..i].split_once(',')?;
                    rows.push((((ts + t.trim().parse::<f64>().ok()?) * 10.0).round() as u64, rest));
                }
            }
            _ => {}
        }
    }
    Some(rows)
}

/// `fresh`, a day's trace as the table has it, with the points that
/// only `old`, the file already on disk, has, each put back at its
/// time. None when the file has nothing the table lacks (or cannot be
/// read: then it is replaced, as before).
fn with_rows_of(old: &str, fresh: &str) -> Option<String> {
    use std::fmt::Write;
    let mut rows = day_rows(fresh)?;
    let ours = rows.len();
    let old = day_rows(old)?;
    let only: Vec<(u64, &str)> = old.iter().filter(|r| rows.binary_search_by_key(&r.0, |x| x.0).is_err()).copied().collect();
    rows.extend(only);
    if rows.len() == ours {
        return None;
    }
    rows.sort_by_key(|r| r.0);
    let base = rows[0].0;
    let at = fresh.find("\"timestamp\":")? + 12;
    let comma = at + fresh[at..].find(',')?;
    let trace = fresh.find("\"trace\":")?;
    let open = trace + fresh[trace..].find('[')? + 1;
    let mut out = String::with_capacity(fresh.len() + old.len() * 80);
    out.push_str(&fresh[..at]);
    let _ = write!(out, " {:.3}", base as f64 / 10.0);
    out.push_str(&fresh[comma..open]);
    out.push_str(" \n");
    for (i, (t, rest)) in rows.iter().enumerate() {
        let _ = write!(out, "[{:.2},{rest}]{}\n", (t - base) as f64 / 10.0, if i + 1 < rows.len() { "," } else { "" });
    }
    out.push_str("]}\n");
    Some(out)
}

fn gunzip(path: &str) -> Option<String> {
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).ok()?).read_to_string(&mut text).ok()?;
    Some(text)
}

/// A day file to write: where, what the table has for that day, and
/// whether the file already there may hold more than that.
struct DayFile {
    addr: u32,
    day: u32,
    path: String,
    name: String,
    body: String,
    look: bool,
}

/// Write the day files of every aircraft in shards `from..to` whose
/// trace has grown since its last write. Rendering happens under the
/// shard lock, compression and file I/O after it is released.
///
/// A day file is the day as the table has it, and the table does not
/// always have the whole day: after a start without a state file it has
/// nothing from before; after a crash, nothing since the hourly state
/// file; with `--trace-hours` under 24, or an aircraft heard for more
/// than a day, the beginning has aged out. Writing the table's day over
/// the file then lost what only the file had (side by side on live
/// traffic, 150 s, a restart, 120 s more: 346 of 420 files began later
/// than before, with no state file; with `--trace-hours` cut to match,
/// 120 of 418). So the file is read first whenever this process has not
/// yet written it from this trace, or found more in it than the trace
/// had, or the trace's day now begins later than it did; what only the
/// file has stays. In the ordinary case, a trace that has had its whole
/// day in the table since this process first wrote it, nothing is read.
pub fn write_history(ctx: &Ctx, dir: &str, from: usize, to: usize) -> usize {
    let mut files = 0;
    for s in &ctx.store.shards()[from..to] {
        let mut jobs: Vec<DayFile> = Vec::new();
        {
            let mut s = s.lock();
            let s = &mut *s;
            let live = s.map.values_mut().map(|a| (a.addr, a.db.clone(), &mut a.trace));
            let gone = s.gone.iter_mut().map(|(addr, g)| (*addr, None, &mut g.trace));
            for (addr, db, trace) in live.chain(gone) {
                let n = trace.len();
                if trace.written >= n {
                    continue;
                }
                let db = db.or_else(|| ctx.db.as_ref().and_then(|d| d.describe(addr)));
                let (points, details) = trace.all();
                // every day that has an unwritten point gets its file rewritten
                let first_day = points[trace.written].t() / DAY;
                let last_day = points[n - 1].t() / DAY;
                let mut noted = (trace.file_day, trace.file_more, trace.file_first);
                for day in first_day..=last_day {
                    let lo = points.partition_point(|p| p.t() / DAY < day);
                    let hi = points.partition_point(|p| p.t() / DAY <= day);
                    if lo == hi {
                        continue;
                    }
                    let (y, m, d) = civil(day * DAY);
                    let hex = crate::json::hex(addr);
                    let mut body = String::with_capacity((hi - lo) * 70 + 256);
                    trace::render(addr, db.as_deref(), &points[lo..hi], &details, &mut body);
                    let first = points[lo].t();
                    let look = trace.file_day != day as u32 || trace.file_more || first > trace.file_first;
                    noted = (day as u32, false, if trace.file_day == day as u32 { first.min(trace.file_first) } else { first });
                    jobs.push(DayFile {
                        addr,
                        day: day as u32,
                        path: format!("{dir}/{y:04}/{m:02}/{d:02}/traces/{}", &hex[hex.len() - 2..]),
                        name: format!("trace_full_{hex}.json"),
                        body,
                        look,
                    });
                }
                drop((points, details));
                trace.written = n;
                (trace.file_day, trace.file_more, trace.file_first) = noted;
            }
        }
        // traces whose file held more than they did: looked at again next time
        let mut more: Vec<(u32, u32)> = Vec::new();
        for DayFile { addr, day, path, name, mut body, look } in jobs {
            if std::fs::create_dir_all(&path).is_err() {
                continue;
            }
            if look {
                if let Some(whole) = gunzip(&format!("{path}/{name}")).and_then(|old| with_rows_of(&old, &body)) {
                    body = whole;
                    more.push((addr, day));
                }
            }
            let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 5), flate2::Compression::new(4));
            let _ = e.write_all(body.as_bytes());
            let Ok(gz) = e.finish() else { continue };
            let tmp = format!("{path}/.{name}.tmp");
            if std::fs::write(&tmp, gz).is_ok() && std::fs::rename(&tmp, format!("{path}/{name}")).is_ok() {
                files += 1;
            }
        }
        if !more.is_empty() {
            let mut s = s.lock();
            let s = &mut *s;
            for (addr, day) in more {
                let trace = match s.map.get_mut(&addr) {
                    Some(a) => Some(&mut a.trace),
                    None => s.gone.get_mut(&addr).map(|g| &mut g.trace),
                };
                if let Some(t) = trace.filter(|t| t.file_day == day) {
                    t.file_more = true;
                }
            }
        }
    }
    files
}

/// Seconds in which every aircraft's trace_recent file is looked at once:
/// readsb's are rewritten about every 4 s while the aircraft adds points.
pub const TRACE_PASS_S: u64 = 4;
// each pass takes an equal share of the shards
const _: () = assert!(crate::track::SHARDS % TRACE_PASS_S as usize == 0);
/// trace_full is rewritten once it lacks half the points trace_recent
/// holds, so that the two together always have the whole trace, and at
/// least this often while the trace grows.
const FULL_EVERY: Ms = 5 * 60_000;
/// Rendered documents held before the shard is let go and they are written.
const TRACE_BATCH_BYTES: usize = 4 << 20;

/// The live trace files beside aircraft.json, for whatever reads
/// readsb's from disk: traces/<last two hex digits>/trace_recent_<hex>.json
/// with the newest points and trace_full_<hex>.json with all of them,
/// gzipped, as the HTTP server serves them. Writes those of aircraft in
/// shards `from..to` whose trace has grown since; returns how many were
/// written and how many could not be.
pub fn write_traces(ctx: &Ctx, dir: &str, from: usize, to: usize) -> (usize, usize) {
    let n_recent = ctx.cfg.trace_recent_points;
    let (mut files, mut failed) = (0, 0);
    for s in &ctx.store.shards()[from..to] {
        // what needs writing, then a batch at a time: rendered whole, a
        // day of traces (a restart, all of them at once) would be held
        // in memory together
        let mut due: Vec<(u32, bool, bool)> = Vec::new();
        {
            let s = s.lock();
            let live = s.map.values().map(|a| (a.addr, &a.trace));
            for (addr, trace) in live.chain(s.gone.iter().map(|(addr, g)| (*addr, &g.trace))) {
                let Some(last) = trace.last_point().map(|p| p.t()) else { continue };
                let f = trace.files.as_deref().copied().unwrap_or_default();
                let recent = last > f.recent;
                let full = last > f.full
                    && (f.full == 0 || trace.len().saturating_sub(f.full_len) * 2 >= n_recent || last >= f.full + FULL_EVERY);
                if recent || full {
                    due.push((addr, recent, full));
                }
            }
        }
        let mut due = due.into_iter().peekable();
        // (when stopping, the processors and the shards are left to what
        // has to be on disk before the process ends; these files are
        // brought up to date by the next start)
        while due.peek().is_some() && !ctx.state.stopping.load(std::sync::atomic::Ordering::Relaxed) {
            let mut jobs: Vec<(String, String)> = Vec::new();
            let mut bytes = 0;
            {
                let mut s = s.lock();
                let s = &mut *s;
                for (addr, recent, full) in due.by_ref() {
                    let (db, trace) = match s.map.get_mut(&addr) {
                        Some(a) => (a.db.clone(), &mut a.trace),
                        None => match s.gone.get_mut(&addr) {
                            Some(g) => (None, &mut g.trace),
                            None => continue,
                        },
                    };
                    let Some(last) = trace.last_point().map(|p| p.t()) else { continue };
                    let db = db.or_else(|| ctx.db.as_ref().and_then(|d| d.describe(addr)));
                    let hex = crate::json::hex(addr);
                    let sub = format!("{dir}/traces/{}", &hex[hex.len() - 2..]);
                    for (kind, wanted) in [("recent", recent), ("full", full)] {
                        if !wanted {
                            continue;
                        }
                        let (points, details) = if kind == "recent" { trace.last(n_recent) } else { trace.all() };
                        let mut body = String::with_capacity(points.len() * 70 + 256);
                        trace::render(addr, db.as_deref(), &points, &details, &mut body);
                        bytes += body.len();
                        jobs.push((format!("{sub}/trace_{kind}_{hex}.json"), body));
                    }
                    let len = trace.len();
                    let f = trace.files.get_or_insert_with(Default::default);
                    if recent {
                        f.recent = last;
                    }
                    if full {
                        (f.full, f.full_len) = (last, len);
                    }
                    if bytes >= TRACE_BATCH_BYTES {
                        break;
                    }
                }
            }
            for (path, body) in jobs {
                // readsb's are gzipped at its fastest setting
                let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 5), flate2::Compression::fast());
                let _ = e.write_all(body.as_bytes());
                let Ok(gz) = e.finish() else { continue };
                let (parent, name) = path.rsplit_once('/').unwrap_or((".", &path));
                let tmp = format!("{parent}/.{name}.tmp");
                if std::fs::write(&tmp, gz).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                    files += 1;
                } else {
                    let _ = std::fs::remove_file(&tmp);
                    failed += 1;
                }
            }
        }
    }
    (files, failed)
}

/// Remove the trace files of aircraft no longer kept (their trace has
/// aged out, or they are from an earlier run that kept no state) and
/// files left half-written. Run between passes of `write_traces`, never
/// beside one. Returns how many went.
pub fn sweep_traces(ctx: &Ctx, dir: &str) -> usize {
    let mut removed = 0;
    for i in 0..256 {
        let Ok(entries) = std::fs::read_dir(format!("{dir}/traces/{i:02x}")) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let keep = match trace::file_addr(name) {
                Some((_, addr)) => {
                    let s = ctx.store.shard(addr).lock();
                    s.map.contains_key(&addr) || s.gone.contains_key(&addr)
                }
                // ours, from a write cut short; anything else is left alone
                None => !(name.starts_with(".trace_") && name.ends_with(".tmp")),
            };
            if !keep && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Write every day file that is behind, the shards shared out over
/// `threads`: what is left to write when stopping is the whole day of
/// every aircraft heard in the last ten minutes, and rendering and
/// compressing them is all processor time.
pub fn write_history_all(ctx: &Ctx, dir: &str, threads: usize) -> usize {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        let mut files = 0;
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i >= crate::track::SHARDS {
                return files;
            }
            files += write_history(ctx, dir, i, i + 1);
        }
    };
    std::thread::scope(|s| {
        let others: Vec<_> = (1..threads.clamp(1, crate::track::SHARDS)).map(|_| s.spawn(work)).collect();
        work() + others.into_iter().map(|t| t.join().unwrap_or(0)).sum::<usize>()
    })
}

/// The state on disk is two files. `state.bin` is every trace, written
/// each hour; `state.add` is what the traces have gained since, written
/// when stopping. A chunk of a trace is never rewritten once packed, so
/// what an hour adds is new chunks and the points still open, a
/// twentieth of the whole: stopping writes tens of megabytes where the
/// whole is a gigabyte, and does not depend on how fast the disk takes a
/// gigabyte. Both hold traces as they are in memory, chunks already
/// compressed, so saving is a copy and a write, and restoring a read.
///
/// ```text
/// either file:  8 bytes magic, u8 size of a packed point, u8 of a detail,
///               u64 the whole file's id (state.add: of the one it adds to),
///               blocks, end
/// block:  u64 bytes  u32 aircraft  u32 CRC-32 of the bytes  the bytes
///         in them, per aircraft: u32 address, the trace (`Trace::save`)
/// end:    u64::MAX   u64 aircraft in the file
/// ```
///
/// all little-endian; a block is a shard of the table. In `state.add` an
/// aircraft is there if it has gained points or had some written to its
/// day file, with the chunks packed since the whole file and its open
/// points. Each file is written under another name and renamed when it is
/// whole and on the disk: stopped at any moment, what is there is the
/// last file that was finished, and `state.add` is only read with the
/// `state.bin` it was written for.
///
/// The file before these (`state.zst`, magic `aggdst01`, up to 0.1.2) was
/// every point unpacked to 36 bytes and the whole compressed again, and
/// it left out the details.
const MAGIC: &[u8; 8] = b"aggdst02";
const MAGIC_ADD: &[u8; 8] = b"aggdsa02";
const FILE: &str = "state.bin";
const FILE_ADD: &str = "state.add";
const MAGIC_V1: &[u8; 8] = b"aggdst01";
const FILE_V1: &str = "state.zst";
const POINT_V1: usize = 36;
const END: u64 = u64::MAX;
const HEAD: usize = 18;

/// What the last whole file holds of each aircraft: the time up to which
/// its chunks are in it, and the points it had not yet in a day file.
/// That is all it takes to know what a trace has gained since.
struct Whole {
    id: u64,
    of: std::collections::HashMap<u32, (Ms, u32)>,
}

/// The state files as this process knows them.
#[derive(Default)]
pub struct StateFiles {
    /// The whole file that is in place. Held while what was gained since
    /// is written, and for the moment a new whole file takes its place.
    whole: parking_lot::Mutex<Option<Whole>>,
    /// Held while a whole file is written: one at a time.
    writing: parking_lot::Mutex<()>,
    /// Set when stopping: an hourly save under way gives up, even with
    /// the file written and only the disk to wait for, so that the stop
    /// does not wait behind a gigabyte going to disk.
    pub stopping: std::sync::atomic::AtomicBool,
}

fn bad() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "state file is damaged")
}

fn head(magic: &[u8; 8], id: u64) -> [u8; HEAD] {
    let mut h = [0u8; HEAD];
    h[..8].copy_from_slice(magic);
    (h[8], h[9]) = (trace::PACKED as u8, trace::DETAIL as u8);
    h[10..].copy_from_slice(&id.to_le_bytes());
    h
}

/// The id of the whole file in `dir`, if there is one of ours.
fn whole_id(dir: &str) -> Option<u64> {
    let mut h = [0u8; HEAD];
    std::fs::File::open(format!("{dir}/{FILE}")).ok()?.read_exact(&mut h).ok()?;
    (h[..10] == head(MAGIC, 0)[..10]).then(|| u64::from_le_bytes(h[10..].try_into().unwrap()))
}

/// Write a state file under its temporary name, whole and (with `sync`)
/// on the disk: `entry` puts what is to be saved of a trace into the
/// buffer and says whether it put anything. `put_in_place` then gives it
/// its name.
fn write_file(ctx: &Ctx, dir: &str, name: &str, head: [u8; HEAD], give_up: bool, sync: bool, mut entry: impl FnMut(u32, &trace::Trace, &mut Vec<u8>) -> bool) -> std::io::Result<usize> {
    let tmp = format!("{dir}/.{name}.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&head)?;
    let mut raw = Vec::with_capacity(1 << 16);
    let mut n = 0u64;
    for s in ctx.store.shards() {
        if give_up && ctx.state.stopping.load(std::sync::atomic::Ordering::Relaxed) {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        raw.clear();
        let mut count = 0u32;
        {
            let s = s.lock();
            let live = s.map.values().map(|a| (a.addr, &a.trace));
            for (addr, trace) in live.chain(s.gone.iter().map(|(addr, g)| (*addr, &g.trace))) {
                if trace.is_empty() {
                    continue;
                }
                let at = raw.len();
                raw.extend_from_slice(&addr.to_le_bytes());
                if entry(addr, trace, &mut raw) {
                    count += 1;
                } else {
                    raw.truncate(at);
                }
            }
        }
        // the checksum and the disk are kept outside the shard's lock
        let mut crc = flate2::Crc::new();
        crc.update(&raw);
        f.write_all(&(raw.len() as u64).to_le_bytes())?;
        f.write_all(&count.to_le_bytes())?;
        f.write_all(&crc.sum().to_le_bytes())?;
        f.write_all(&raw)?;
        n += count as u64;
    }
    f.write_all(&END.to_le_bytes())?;
    f.write_all(&n.to_le_bytes())?;
    if sync {
        f.sync_all()?;
    }
    Ok(n as usize)
}

fn put_in_place(dir: &str, name: &str, sync: bool) -> std::io::Result<()> {
    std::fs::rename(format!("{dir}/.{name}.tmp"), format!("{dir}/{name}"))?;
    // (the rename itself is on the disk once the directory is)
    if sync {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Save every aircraft's trace. Returns the number of aircraft whose
/// trace is saved (when stopping: of those, the ones that had changed).
///
/// `stopping` writes only what the traces have gained since the last
/// whole file, if this process wrote or read one and it is still there;
/// otherwise, and every hour, the whole. What is written when stopping
/// is not waited for to reach the disk: on a busy one that wait was
/// seconds (15 were seen for 72 MB, behind other files' data), and a
/// stop that outlasts its time is killed. A process killed loses nothing
/// it has written; only the machine losing power in the seconds after
/// could leave that file incomplete, which the next start finds out, and
/// restores the last hourly save. A whole file is written a shard
/// at a time: built in memory first it was a transient that grew with
/// the table and that the allocator did not give back. An hourly save
/// gives up (`Interrupted`) when the process is stopping.
pub fn save_state(ctx: &Ctx, dir: &str, stopping: bool) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let added = || {
        let whole = ctx.state.whole.lock();
        let w = whole.as_ref().filter(|w| stopping && whole_id(dir) == Some(w.id))?;
        let res = write_file(ctx, dir, FILE_ADD, head(MAGIC_ADD, w.id), false, false, |addr, trace, out| {
            let (until, behind) = w.of.get(&addr).copied().unwrap_or((0, u32::MAX));
            if trace.packed_until() <= until && trace.points.is_empty() && trace.behind() as u32 == behind {
                return false;
            }
            trace.save(until, out);
            true
        });
        Some(res.and_then(|n| put_in_place(dir, FILE_ADD, false).map(|_| n)))
    };
    if let Some(res) = added() {
        return res;
    }
    // (a whole file may have been put in place while we waited for this)
    let _one = ctx.state.writing.lock();
    if let Some(res) = added() {
        return res;
    }
    // A file's id only has to differ from the last one's.
    let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ (std::process::id() as u64) << 44;
    let mut of = std::collections::HashMap::new();
    let n = write_file(ctx, dir, FILE, head(MAGIC, id), !stopping, true, |addr, trace, out| {
        of.insert(addr, (trace.packed_until(), trace.behind() as u32));
        trace.save(0, out);
        true
    })?;
    let mut whole = ctx.state.whole.lock();
    // The stop did not wait for the disk to take this file: it has
    // written, or is about to, what was gained since the one in place.
    if !stopping && ctx.state.stopping.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(format!("{dir}/.{FILE}.tmp"));
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    put_in_place(dir, FILE, true)?;
    // what was added to the file this one replaced does not add to this one
    let _ = std::fs::remove_file(format!("{dir}/{FILE_ADD}"));
    *whole = Some(Whole { id, of });
    drop(whole);
    // The file of an earlier version stays for as long as that version
    // could still restore something from it, should it be put back.
    let old = format!("{dir}/{FILE_V1}");
    let age = |m: std::fs::Metadata| m.modified().ok()?.elapsed().ok();
    if std::fs::metadata(&old).ok().and_then(age).is_some_and(|a| a.as_millis() as u64 > ctx.cfg.trace_keep_ms) {
        let _ = std::fs::remove_file(&old);
    }
    Ok(n)
}

/// Restore the traces saved by `save_state`, or by an earlier version if
/// its file is the newer one. Aircraft come back unlisted (not heard
/// since) but with their history, and are forgotten on the usual
/// schedule if they stay silent. Returns the number of aircraft restored.
pub fn load_state(ctx: &Ctx, dir: &str, now: Ms) -> std::io::Result<usize> {
    let mut whole = ctx.state.whole.lock();
    let open = |name: &str| {
        let f = std::fs::File::open(format!("{dir}/{name}"))?;
        let t = f.metadata()?.modified()?;
        Ok::<_, std::io::Error>((f, t))
    };
    match (open(FILE), open(FILE_V1)) {
        (Ok((_, t)), Ok((old, t1))) if t1 > t => load_v1(ctx, old, now),
        (Ok((f, _)), _) => {
            let (n, w) = load_v2(ctx, f, std::fs::File::open(format!("{dir}/{FILE_ADD}")).ok(), now)?;
            *whole = Some(w);
            Ok(n)
        }
        (Err(_), Ok((old, _))) => load_v1(ctx, old, now),
        (Err(e), Err(_)) => Err(e),
    }
}

/// Put a restored trace in the table. Returns whether any of it was kept.
fn restore(ctx: &Ctx, addr: u32, trace: trace::Trace, now: Ms) -> bool {
    let Some(gone) = restore_one(trace, now, ctx.cfg.trace_keep_ms) else { return false };
    ctx.store.shard(addr).lock().gone.insert(addr, gone);
    true
}

/// One aircraft's saved trace, back as an aircraft forgotten while
/// running: its trace, less what has aged out since it was saved (whole
/// chunks, as when running), and where it last was for its surface
/// messages to be read against. Without that place, an aircraft parked
/// through a restart and heard only by receivers with no location of
/// their own went unplaced until it moved (seen beside readsb, which
/// went on showing it). None when no point is recent enough to keep.
fn restore_one(mut trace: trace::Trace, now: Ms, keep_ms: Ms) -> Option<crate::track::Gone> {
    trace.trim_before(now.saturating_sub(keep_ms));
    let p = trace.last_point()?.unpack();
    let last_place = Some((p.lat as f64 / 1e6, p.lon as f64 / 1e6));
    Some(crate::track::Gone { seen: p.t, pos: Default::default(), nic: 0, rc: 0, last_place, heat_tag: 0, trace })
}

/// Read a state file a block at a time, each checked whole before any of
/// it is used. Returns the id in its head.
fn read_file(file: std::fs::File, magic: &[u8; 8], mut each: impl FnMut(u32, trace::Saved)) -> std::io::Result<u64> {
    let size = file.metadata()?.len();
    let mut f = std::io::BufReader::with_capacity(1 << 20, file);
    let mut h = [0u8; HEAD];
    f.read_exact(&mut h).map_err(|_| bad())?;
    if h[..10] != head(magic, 0)[..10] {
        return Err(bad());
    }
    let id = u64::from_le_bytes(h[10..].try_into().unwrap());
    let mut raw = Vec::new();
    let mut found = 0u64;
    loop {
        let mut h = [0u8; 16];
        f.read_exact(&mut h).map_err(|_| bad())?;
        let len = u64::from_le_bytes(h[..8].try_into().unwrap());
        if len == END {
            // every aircraft counted when saving must have been there
            return if u64::from_le_bytes(h[8..].try_into().unwrap()) == found { Ok(id) } else { Err(bad()) };
        }
        let count = u32::from_le_bytes(h[8..12].try_into().unwrap());
        let sum = u32::from_le_bytes(h[12..].try_into().unwrap());
        if len > size {
            return Err(bad());
        }
        raw.resize(len as usize, 0);
        f.read_exact(&mut raw).map_err(|_| bad())?;
        let mut crc = flate2::Crc::new();
        crc.update(&raw);
        if crc.sum() != sum {
            return Err(bad());
        }
        let mut b = &raw[..];
        let mut traces = Vec::with_capacity((count as usize).min(raw.len() / 9));
        for _ in 0..count {
            if b.len() < 4 {
                return Err(bad());
            }
            let addr = u32::from_le_bytes(b[..4].try_into().unwrap());
            b = &b[4..];
            traces.push((addr, trace::Trace::load(&mut b).ok_or_else(bad)?));
        }
        if !b.is_empty() {
            return Err(bad());
        }
        found += count as u64;
        traces.into_iter().for_each(|(addr, t)| each(addr, t));
    }
}

fn load_v2(ctx: &Ctx, file: std::fs::File, add: Option<std::fs::File>, now: Ms) -> std::io::Result<(usize, Whole)> {
    let mut h = [0u8; HEAD];
    (&file).read_exact(&mut h).map_err(|_| bad())?;
    (&file).seek(std::io::SeekFrom::Start(0))?;
    let id = u64::from_le_bytes(h[10..].try_into().unwrap());
    // What was added since comes first: it is small, and each trace of
    // the whole file is then finished as it is read. A file of additions
    // that is damaged, or was written for another whole file (the process
    // was killed between an hourly save and its tidying up), is left out.
    let mut added = std::collections::HashMap::new();
    if let Some(f) = add {
        let mut all = Vec::new();
        match read_file(f, MAGIC_ADD, |addr, t| all.push((addr, t))) {
            Ok(of) if of == id => added.extend(all),
            Ok(_) => {}
            Err(e) => eprintln!("aggd: what the state gained after its last hourly save is not restored: {e}"),
        }
    }
    let mut of = std::collections::HashMap::new();
    let mut n = 0;
    read_file(file, MAGIC, |addr, mut t| {
        // (as the file has it, not as it is once brought up to date)
        of.insert(addr, (t.until, t.behind as u32));
        if let Some(newer) = added.remove(&addr) {
            t.trace.extend(t.open, newer);
        }
        n += restore(ctx, addr, t.trace, now) as usize;
    })?;
    // aircraft first heard after the whole file was written
    for (addr, t) in added {
        n += restore(ctx, addr, t.trace, now) as usize;
    }
    Ok((n, Whole { id, of }))
}

fn get_point(b: &[u8]) -> Point {
    let u16_ = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let i32_ = |i: usize| i32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    Point {
        t: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        lat: i32_(8),
        lon: i32_(12),
        alt: i32_(16),
        alt_geom: i32_(20),
        gs: u16_(24),
        track: u16_(26),
        rate: u16_(28) as i16,
        geom_rate: u16_(30) as i16,
        ias: u16_(32),
        flags: b[34],
        addr_type: b[35],
    }
}

/// The file of versions up to 0.1.2: one zstd stream of the magic and,
/// per aircraft, u32 address, u32 points, u32 points already in a history
/// file, and the points unpacked at 36 bytes each. Read an aircraft at a
/// time; its points are packed away again, which is most of the time
/// this takes.
fn load_v1(ctx: &Ctx, file: std::fs::File, now: Ms) -> std::io::Result<usize> {
    let mut z = std::io::BufReader::with_capacity(1 << 20, zstd::stream::Decoder::new(file)?);
    let mut magic = [0u8; 8];
    z.read_exact(&mut magic).map_err(|_| bad())?;
    if &magic != MAGIC_V1 {
        return Err(bad());
    }
    let mut n = 0;
    let mut raw = Vec::new();
    loop {
        let mut h = [0u8; 12];
        // the file ends where an aircraft would begin, or it was cut short
        match z.read(&mut h[..1])? {
            0 => return Ok(n),
            _ => z.read_exact(&mut h[1..]).map_err(|_| bad())?,
        }
        let addr = u32::from_le_bytes(h[..4].try_into().unwrap());
        let count = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
        let written = u32::from_le_bytes(h[8..].try_into().unwrap()) as usize;
        // (not `resize`: a count that is not true must not reserve memory)
        raw.clear();
        let want = count as u64 * POINT_V1 as u64;
        if (&mut z).take(want).read_to_end(&mut raw)? as u64 != want {
            return Err(bad());
        }
        let points: Vec<Packed> = raw.chunks_exact(POINT_V1).map(get_point).filter(|p| now.saturating_sub(p.t) <= ctx.cfg.trace_keep_ms).map(|p| Packed::pack(&p)).collect();
        if points.is_empty() {
            continue;
        }
        let trace = trace::Trace::restored(&points, written.saturating_sub(count - points.len()).min(points.len()));
        n += restore(ctx, addr, trace, now) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::Aircraft;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const HOUR: Ms = 3_600_000;
    /// 2026-11-16 12:00 UTC
    const NOON: Ms = 1_794_830_400_000;

    pub fn ctx(trace_keep_ms: Ms) -> Ctx {
        let cfg = crate::Config {
            site: None,
            reduce_interval_ms: 250,
            json_port_interval_ms: 1000,
            json_port_noposition: false,
            json_every_ms: 1000,
            trace_interval_ms: 30_000,
            trace_recent_points: 142,
            trace_keep_ms,
            list_window_ms: 60_000,
            forward_receiver_id: false,
            uuid: None,
            long_db: false,
            jaero_timeout_min: 35.0,
            receiver_count: false,
            forward_mlat: false,
            forward_mlat_sbs: false,
            sbs_reduce: false,
            web_root: None,
            history_dir: None,
            cors: crate::http::Cors::parse("*").unwrap(),
        };
        let limits = crate::guard::Limits { max_connections: 10, max_per_ip: 10, max_rate: 1000.0, max_new_aircraft: 10, max_aircraft: 1_000_000, trusted: Vec::new() };
        Ctx {
            cfg,
            store: crate::track::Store::new(),
            reg: Default::default(),
            hub: crate::net::Hub::new(),
            db: None,
            stats: Default::default(),
            cache: Default::default(),
            guard: crate::guard::Guard::new(limits),
            heat: Default::default(),
            state: Default::default(),
            started: NOON,
        }
    }

    /// A directory of its own for each test.
    pub fn scratch(name: &str) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("aggd-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        dir.to_str().unwrap().to_string()
    }

    fn point(addr: u32, i: u32, t: Ms) -> Point {
        let k = (addr % 97 + i) as i32;
        Point { t, lat: 45_000_000 + k * 913, lon: 9_000_000 - k * 1291, alt: 30_000 + (k % 40) * 25, alt_geom: 30_500 + (k % 40) * 25, gs: 4400 + (i % 7) as u16, track: (i * 3 % 3600) as u16, rate: 64 * (i % 5) as i16, geom_rate: i16::MIN, ias: 280, flags: (i % 3 == 0) as u8, addr_type: (addr % 3) as u8 }
    }

    fn detail(addr: u32, t: Ms) -> trace::Detail {
        let mut d: trace::Detail = unsafe { std::mem::zeroed() };
        d.t = t;
        d.callsign = *b"AZA1234 ";
        d.squawk = (addr % 7777) as u16;
        d.nav_qnh = 1013.2;
        d.mach = 0.78;
        d
    }

    /// An aircraft with `n` points 20 s apart, the last at `end`, and a
    /// detail with every sixth; built a point at a time, as in flight.
    fn flown(addr: u32, n: u32, end: Ms) -> Box<Aircraft> {
        let mut a = Box::new(Aircraft::new(addr, end));
        for i in 0..n {
            let t = end - (n - 1 - i) as Ms * 20_000;
            a.trace.points.push(Packed::pack(&point(addr, i, t)));
            if i % 6 == 0 {
                a.trace.details.push(detail(addr, t));
            }
            a.trace.tidy();
        }
        a
    }

    /// A table of `n` aircraft whose last points are at `end` or a few
    /// minutes before: two in three gone silent, the others in flight
    /// with points still open; three in four with their newest points
    /// not yet in a day file.
    fn fill(c: &Ctx, n: u32, end: Ms) {
        for k in 0..n {
            let addr = 0x400000 + k * 7919;
            let mut a = flown(addr, 3 + (k * 37) % 700, end - (k % 5) as Ms * 60_000);
            a.trace.written = a.trace.len() - (k as usize % 4).min(a.trace.len());
            let mut s = c.store.shard(addr).lock();
            if k % 3 == 0 {
                s.map.insert(addr, a);
            } else {
                a.trace.compact(0);
                s.gone.insert(addr, crate::track::Gone { seen: a.seen, pos: a.pos, nic: 0, rc: 0, last_place: None, heat_tag: 0, trace: std::mem::take(&mut a.trace) });
            }
        }
    }

    /// address → (points, details, points already in a day file, last
    /// point, time of the last detail), as bytes: all that is served
    type Traces = std::collections::BTreeMap<u32, (Vec<u8>, Vec<u8>, usize, Vec<u8>, Option<Ms>)>;

    fn bytes<T: Copy>(v: &[T]) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }.to_vec()
    }

    fn traces(c: &Ctx) -> Traces {
        let mut all = Traces::new();
        for s in c.store.shards() {
            let s = s.lock();
            for (addr, t) in s.map.values().map(|a| (a.addr, &a.trace)).chain(s.gone.iter().map(|(a, g)| (*a, &g.trace))) {
                let (p, d) = t.all();
                all.insert(addr, (bytes(&p), bytes(&d), t.written, bytes(&[t.last_point().unwrap()]), t.last_detail().map(|d| d.t)));
            }
        }
        all
    }

    /// The state of 0.1.2 and before, as that version wrote it.
    pub fn save_v1(c: &Ctx, dir: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut z = zstd::stream::Encoder::new(std::fs::File::create(format!("{dir}/{FILE_V1}")).unwrap(), 3).unwrap();
        z.write_all(MAGIC_V1).unwrap();
        for s in c.store.shards() {
            let s = s.lock();
            for (addr, trace) in s.map.values().map(|a| (a.addr, &a.trace)).chain(s.gone.iter().map(|(a, g)| (*a, &g.trace))) {
                let (points, _) = trace.all();
                let mut o = Vec::new();
                o.extend_from_slice(&addr.to_le_bytes());
                o.extend_from_slice(&(points.len() as u32).to_le_bytes());
                o.extend_from_slice(&(trace.written as u32).to_le_bytes());
                for p in points.iter().map(|p| p.unpack()) {
                    o.extend_from_slice(&p.t.to_le_bytes());
                    for v in [p.lat, p.lon, p.alt, p.alt_geom] {
                        o.extend_from_slice(&v.to_le_bytes());
                    }
                    for v in [p.gs, p.track, p.rate as u16, p.geom_rate as u16, p.ias] {
                        o.extend_from_slice(&v.to_le_bytes());
                    }
                    o.extend_from_slice(&[p.flags, p.addr_type]);
                }
                z.write_all(&o).unwrap();
            }
        }
        z.finish().unwrap();
    }

    #[test]
    fn a_restored_aircraft_keeps_where_it_last_was() {
        let now: Ms = 1_795_000_000_000;
        let place = |t: Ms, lat: i32, lon: i32| Packed::pack(&Point { t, lat, lon, alt: trace::GROUND, alt_geom: trace::NONE, gs: 0, track: 0, rate: 0, geom_rate: 0, ias: 0, flags: 0, addr_type: 0 });
        let saved = || trace::Trace::restored(&[place(now - 7_200_000, 40_640_000, -73_780_000), place(now - 100_000, 40_689_500, -74_174_400)], 2);
        let g = restore_one(saved(), now, 86_400_000).unwrap();
        assert_eq!(g.seen / 100, (now - 100_000) / 100);
        let (lat, lon) = g.last_place.unwrap();
        assert!((lat - 40.6895).abs() < 1e-6 && (lon + 74.1744).abs() < 1e-6);
        assert!(g.pos.src == modes::Source::Invalid && g.trace.len() == 2);
        // a trace older than the kept window brings nothing back
        assert!(restore_one(saved(), now + 86_400_000, 86_400_000).is_none());
        // and it is so from either file: ours, and the earlier version's
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 30, NOON);
        save_state(&a, &dir, false).unwrap();
        save_v1(&a, &format!("{dir}/v1"));
        for d in [dir.clone(), format!("{dir}/v1")] {
            let b = ctx(24 * HOUR);
            load_state(&b, &d, NOON).unwrap();
            let s = b.store.shard(0x400000).lock();
            let (g, p) = (&s.gone[&0x400000], point(0x400000, 2, NOON));
            assert_eq!(g.last_place, Some((p.lat as f64 / 1e6, p.lon as f64 / 1e6)), "{d}");
        }
    }

    #[test]
    fn a_saved_state_restores_every_trace_as_it_was() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 300, NOON);
        assert_eq!(save_state(&a, &dir, false).unwrap(), 300);
        // saving changed nothing in the table: points in flight stay open
        assert!(a.store.shards().iter().any(|s| s.lock().map.values().any(|x| !x.trace.points.is_empty())));
        let b = ctx(24 * HOUR);
        assert_eq!(load_state(&b, &dir, NOON + 60_000).unwrap(), 300);
        // points, details, what the day files already hold, and the last
        // point and detail that the next point will be measured against
        assert!(traces(&a) == traces(&b));
        // restored aircraft are unlisted until heard again
        assert!(b.store.shards().iter().all(|s| s.lock().map.is_empty()));
        let seen: Vec<Ms> = b.store.shards().iter().flat_map(|s| s.lock().gone.values().map(|g| g.seen).collect::<Vec<_>>()).collect();
        assert!(seen.len() == 300 && seen.iter().all(|t| (NOON - 4 * 60_000..=NOON).contains(t)));
        // a restored table saved again restores the same
        let again = scratch("state");
        assert_eq!(save_state(&b, &again, false).unwrap(), 300);
        let c = ctx(24 * HOUR);
        load_state(&c, &again, NOON + 60_000).unwrap();
        assert!(traces(&b) == traces(&c));
        // only the file itself is left behind
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(left, [FILE]);
    }

    #[test]
    fn an_empty_table_saves_and_restores() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        assert_eq!(save_state(&a, &dir, false).unwrap(), 0);
        assert_eq!(load_state(&a, &dir, NOON).unwrap(), 0);
        // and with no file there is nothing to restore, which is not damage
        assert_eq!(load_state(&a, &scratch("state"), NOON).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_state_file_of_the_earlier_format_still_loads() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 200, NOON);
        save_v1(&a, &dir);
        let b = ctx(24 * HOUR);
        assert_eq!(load_state(&b, &dir, NOON + 60_000).unwrap(), 200);
        // that format had the points and what was written, not the details
        let (a, b) = (traces(&a), traces(&b));
        assert!(a.keys().eq(b.keys()));
        assert!(a.values().zip(b.values()).all(|(x, y)| (&x.0, x.2, &x.3) == (&y.0, y.2, &y.3) && y.1.is_empty()));
    }

    #[test]
    fn the_newer_of_two_state_files_is_the_one_restored() {
        let (dir, old, new) = (scratch("state"), ctx(24 * HOUR), ctx(24 * HOUR));
        fill(&old, 20, NOON);
        fill(&new, 30, NOON);
        // upgraded: the earlier version's file is there, ours is newer
        save_v1(&old, &dir);
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_state(&new, &dir, false).unwrap();
        assert_eq!(load_state(&ctx(24 * HOUR), &dir, NOON).unwrap(), 30);
        // the earlier version put back for a while, then ours again
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_v1(&old, &dir);
        assert_eq!(load_state(&ctx(24 * HOUR), &dir, NOON).unwrap(), 20);
    }

    #[test]
    fn the_earlier_state_file_goes_once_nothing_in_it_could_be_restored() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 5, NOON);
        save_v1(&a, &dir);
        save_state(&a, &dir, false).unwrap();
        assert!(std::fs::metadata(format!("{dir}/{FILE_V1}")).is_ok());
        // a day later (here: nothing is kept even a moment)
        std::thread::sleep(std::time::Duration::from_millis(5));
        save_state(&ctx(0), &dir, false).unwrap();
        assert!(std::fs::metadata(format!("{dir}/{FILE_V1}")).is_err());
    }

    #[test]
    fn what_aged_out_since_the_save_is_not_restored() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        // one flight of two hours and a quarter ending at noon, one that
        // ended at ten
        for (addr, end) in [(1, NOON), (2, NOON - 2 * HOUR)] {
            a.store.shard(addr).lock().map.insert(addr, flown(addr, 400, end));
        }
        save_state(&a, &dir, false).unwrap();
        save_v1(&a, &format!("{dir}/v1"));
        for d in [dir.clone(), format!("{dir}/v1")] {
            // an hour kept, read 35 minutes after noon: the first flight's
            // last 25 minutes (from our own file a chunk more, as chunks
            // go whole), nothing of the second
            let b = ctx(HOUR);
            assert_eq!(load_state(&b, &d, NOON + 35 * 60_000).unwrap(), 1, "{d}");
            let s = b.store.shard(1).lock();
            let t = &s.gone[&1].trace;
            assert!((76..76 + 128).contains(&t.len()), "{}", t.len());
            assert_eq!((s.gone[&1].seen, t.last_point().unwrap().t(), t.written <= t.len()), (NOON, NOON, true));
            // read the next day, there is nothing
            assert_eq!(load_state(&ctx(HOUR), &d, NOON + 24 * HOUR).unwrap(), 0);
        }
    }

    #[test]
    fn a_damaged_state_file_is_refused_not_trusted() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 40, NOON);
        save_state(&a, &dir, false).unwrap();
        let path = format!("{dir}/{FILE}");
        let good = std::fs::read(&path).unwrap();
        let damaged = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            load_state(&ctx(24 * HOUR), &dir, NOON).map_err(|e| e.kind())
        };
        // cut short anywhere: in the header, in a block, before the end mark
        for cut in [0, 5, 9, 10, 20, good.len() / 3, good.len() / 2, good.len() - 16, good.len() - 1] {
            assert_eq!(damaged(&good[..cut]), Err(std::io::ErrorKind::InvalidData), "cut at {cut}");
        }
        // a byte changed anywhere
        for at in [0, 9, 18, 26, 30, good.len() / 2, good.len() - 20, good.len() - 3] {
            let mut bytes = good.clone();
            bytes[at] ^= 0x10;
            assert_eq!(damaged(&bytes), Err(std::io::ErrorKind::InvalidData), "changed at {at}");
        }
        // another program's file
        assert_eq!(damaged(b"not a state file at all"), Err(std::io::ErrorKind::InvalidData));
        assert_eq!(damaged(&good), Ok(40));
        // the earlier format cut short is refused as well
        let v1 = scratch("state");
        save_v1(&a, &v1);
        let whole = std::fs::read(format!("{v1}/{FILE_V1}")).unwrap();
        std::fs::write(format!("{v1}/{FILE_V1}"), &whole[..whole.len() / 2]).unwrap();
        assert!(load_state(&ctx(24 * HOUR), &v1, NOON).is_err());
    }

    #[test]
    fn a_trace_that_is_not_one_does_not_load() {
        let a = flown(7, 300, NOON);
        let mut good = Vec::new();
        a.trace.save(0, &mut good);
        let whole = |b: &[u8]| {
            let mut rest = b;
            trace::Trace::load(&mut rest).filter(|_| rest.is_empty()).map(|t| t.trace.len())
        };
        assert_eq!(whole(&good), Some(300));
        for cut in 0..good.len() {
            assert!(whole(&good[..cut]).is_none(), "cut at {cut}");
        }
        // chunks said to be there that are not; a chunk said to hold more
        // points than its bytes unpack to; one that ends before it begins
        for (at, v) in [(4, u32::MAX), (9, 97), (25, 1)] {
            let mut b = good.clone();
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
            assert!(whole(&b).is_none(), "{v} at {at}");
        }
        // the second chunk put first: a trace does not run backwards
        let first = 9 + 28 + u32::from_le_bytes(good[33..37].try_into().unwrap()) as usize;
        let second = first + 28 + u32::from_le_bytes(good[first + 24..first + 28].try_into().unwrap()) as usize;
        let swapped = [&good[..9], &good[first..second], &good[9..first], &good[second..]].concat();
        assert!(swapped.len() == good.len() && whole(&swapped).is_none());
    }

    #[test]
    fn a_save_cut_short_leaves_the_last_one_in_place() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 50, NOON);
        save_state(&a, &dir, false).unwrap();
        // killed while writing the next: half a file under the temporary name
        let good = std::fs::read(format!("{dir}/{FILE}")).unwrap();
        std::fs::write(format!("{dir}/.{FILE}.tmp"), &good[..good.len() / 2]).unwrap();
        assert_eq!(load_state(&ctx(24 * HOUR), &dir, NOON).unwrap(), 50);
        assert_eq!(std::fs::read(format!("{dir}/{FILE}")).unwrap(), good);
        // and the save after that is whole again
        fill(&a, 60, NOON);
        assert_eq!(save_state(&a, &dir, false).unwrap(), 60);
        assert_eq!(load_state(&ctx(24 * HOUR), &dir, NOON).unwrap(), 60);
        assert!(std::fs::metadata(format!("{dir}/.{FILE}.tmp")).is_err());
    }

    #[test]
    fn saves_at_the_same_moment_do_not_spoil_each_other() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 400, NOON);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| assert_eq!(save_state(&a, &dir, false).unwrap(), 400));
            }
        });
        let b = ctx(24 * HOUR);
        assert_eq!(load_state(&b, &dir, NOON).unwrap(), 400);
        assert!(traces(&a) == traces(&b));
    }

    /// The day goes on: more points for the aircraft in flight, some of
    /// those gone are heard again, one lands and falls silent, one is
    /// new, day files are brought up to date.
    fn go_on(c: &Ctx, from: Ms) {
        let more = |t: &mut trace::Trace, addr: u32, n: u32| {
            for i in 0..n {
                let at = from + i as Ms * 20_000;
                t.points.push(Packed::pack(&point(addr, i, at)));
                if i % 6 == 0 {
                    t.details.push(detail(addr, at));
                }
                t.tidy();
            }
        };
        for s in c.store.shards() {
            let s = &mut *s.lock();
            for a in s.map.values_mut() {
                more(&mut a.trace, a.addr, a.addr % 251);
                if a.addr % 5 == 0 {
                    a.trace.written = a.trace.len();
                }
            }
            for (addr, g) in s.gone.iter_mut() {
                match addr % 7 {
                    0 => more(&mut g.trace, *addr, 3),
                    1 => {
                        more(&mut g.trace, *addr, 200);
                        g.trace.compact(0);
                    }
                    2 => g.trace.written = g.trace.len(),
                    _ => {}
                }
            }
            let landed: Vec<u32> = s.map.keys().copied().filter(|a| a % 11 == 0).collect();
            for addr in landed {
                let mut a = s.map.remove(&addr).unwrap();
                a.trace.compact(0);
                s.gone.insert(addr, crate::track::Gone { seen: a.seen, pos: a.pos, nic: 0, rc: 0, last_place: None, heat_tag: 0, trace: std::mem::take(&mut a.trace) });
            }
        }
        let new = 0x700000 + (from / 60_000 % 1000) as u32;
        c.store.shard(new).lock().map.insert(new, flown(new, 150, from));
    }

    fn size(dir: &str, name: &str) -> u64 {
        std::fs::metadata(format!("{dir}/{name}")).map_or(0, |m| m.len())
    }

    #[test]
    fn stopping_saves_only_what_the_traces_gained_and_all_of_it_is_restored() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 600, NOON);
        // one with whole chunks of points no day file has yet
        a.store.shard(0x400000).lock().map.get_mut(&0x400000).unwrap().trace.written = 0;
        // no hourly save yet: stopping saves the whole
        assert_eq!((save_state(&a, &dir, true).unwrap(), size(&dir, FILE_ADD)), (600, 0));
        let whole = size(&dir, FILE);
        go_on(&a, NOON + 60_000);
        let all = traces(&a);
        let changed = save_state(&a, &dir, true).unwrap();
        // a third were in flight, and three in seven of the others changed
        assert!(changed > 200 && changed < 420 && all.len() == 601, "{changed} of {}", all.len());
        assert!(size(&dir, FILE) == whole && size(&dir, FILE_ADD) > 0 && size(&dir, FILE_ADD) < whole / 2);
        let b = ctx(24 * HOUR);
        assert_eq!(load_state(&b, &dir, NOON + 120_000).unwrap(), all.len());
        assert!(traces(&b) == all);
        // stopped again before the next hourly save: what is added is
        // what was gained since that same whole file, both times
        go_on(&b, NOON + 2 * HOUR);
        save_state(&b, &dir, true).unwrap();
        let c = ctx(24 * HOUR);
        load_state(&c, &dir, NOON + 4 * HOUR).unwrap();
        assert!(size(&dir, FILE) == whole && traces(&c) == traces(&b));
        // the hourly save is whole again and needs nothing added
        go_on(&c, NOON + 4 * HOUR);
        save_state(&c, &dir, false).unwrap();
        assert!(size(&dir, FILE) > whole && size(&dir, FILE_ADD) == 0);
        let d = ctx(24 * HOUR);
        load_state(&d, &dir, NOON + 6 * HOUR).unwrap();
        assert!(traces(&d) == traces(&c));
        // nothing gained since: all that is added are the points that
        // were open at the hourly save, now packed
        let open = traces(&c).keys().filter(|a| c.store.shard(**a).lock().gone.get(a).map_or(true, |g| !g.trace.points.is_empty())).count();
        assert_eq!((save_state(&d, &dir, true).unwrap(), size(&dir, FILE_ADD) < size(&dir, FILE) / 10), (open, true));
        let e = ctx(24 * HOUR);
        load_state(&e, &dir, NOON + 6 * HOUR).unwrap();
        assert!(traces(&d) == traces(&e));
    }

    #[test]
    fn what_was_added_to_another_whole_file_or_is_damaged_is_left_out() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 200, NOON);
        save_state(&a, &dir, false).unwrap();
        let at_the_hour = traces(&a);
        go_on(&a, NOON + 60_000);
        save_state(&a, &dir, true).unwrap();
        let (add, kept) = (format!("{dir}/{FILE_ADD}"), format!("{dir}/kept"));
        std::fs::copy(&add, &kept).unwrap();
        let restored = || {
            let b = ctx(24 * HOUR);
            load_state(&b, &dir, NOON + 6 * HOUR).map(|_| traces(&b)).map_err(|e| e.kind())
        };
        assert!(restored().unwrap() == traces(&a));
        // cut short or changed: the hourly save alone is restored
        let good = std::fs::read(&add).unwrap();
        std::fs::write(&add, &good[..good.len() / 2]).unwrap();
        assert!(restored().unwrap() == at_the_hour);
        let mut bytes = good.clone();
        bytes[good.len() / 2] ^= 1;
        std::fs::write(&add, &bytes).unwrap();
        assert!(restored().unwrap() == at_the_hour);
        // killed after the next hourly save, before the additions to the
        // one before it were removed: they are not added to this one
        go_on(&a, NOON + 2 * HOUR);
        save_state(&a, &dir, false).unwrap();
        assert!(std::fs::metadata(&add).is_err());
        std::fs::copy(&kept, &add).unwrap();
        assert!(restored().unwrap() == traces(&a));
        // and stopping after that replaces them with what belongs
        go_on(&a, NOON + 4 * HOUR);
        save_state(&a, &dir, true).unwrap();
        assert!(restored().unwrap() == traces(&a));
        // the whole file itself damaged: nothing is restored, whatever was added
        std::fs::write(format!("{dir}/{FILE}"), b"aggdst02 but no more").unwrap();
        assert_eq!(restored().err(), Some(std::io::ErrorKind::InvalidData));
        // and stopping then writes the whole anew
        assert_eq!(save_state(&a, &dir, true).unwrap(), traces(&a).len());
        assert!(restored().unwrap() == traces(&a));
    }

    #[test]
    fn an_hourly_save_gives_way_to_stopping() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 100, NOON);
        save_state(&a, &dir, false).unwrap();
        let whole = std::fs::read(format!("{dir}/{FILE}")).unwrap();
        go_on(&a, NOON + 60_000);
        a.state.stopping.store(true, Ordering::Relaxed);
        // the hourly save that would run now leaves everything as it was
        assert_eq!(save_state(&a, &dir, false).unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        let left: std::collections::BTreeSet<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert!(left.iter().eq([FILE]) && std::fs::read(format!("{dir}/{FILE}")).unwrap() == whole);
        // and the save of the stop itself goes through
        assert!(save_state(&a, &dir, true).unwrap() > 0);
        let b = ctx(24 * HOUR);
        load_state(&b, &dir, NOON + 60_000).unwrap();
        assert!(traces(&a) == traces(&b));
    }

    /// Steady traffic for a week of simulated time: aircraft come, fly
    /// for a while, fall silent, are forgotten, and some return. What
    /// the table holds grows through the first day and no further.
    #[test]
    fn a_week_of_steady_traffic_holds_one_day_of_it() {
        let c = ctx(24 * HOUR);
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        // forty in the air at any time, out of two thousand airframes
        let mut flying: Vec<(u32, Ms, u32)> = Vec::new();
        let (mut days, mut sum, mut peak) = (Vec::new(), [0usize; 3], [0usize; 3]);
        for minute in 0..7 * 24 * 60u64 {
            let now = NOON + minute * 60_000;
            flying.retain(|f| f.1 > now);
            while flying.len() < 40 {
                let addr = 0x300000 + rnd(2000) as u32;
                if flying.iter().all(|f| f.0 != addr) {
                    flying.push((addr, now + (20 + rnd(160)) * 60_000, 0));
                }
            }
            for (addr, _, i) in flying.iter_mut() {
                let mut s = c.store.shard(*addr).lock();
                s.wake(*addr, now);
                let a = s.map.entry(*addr).or_insert_with(|| Box::new(Aircraft::new(*addr, now)));
                a.seen = now + 40_000;
                // (to be forgotten as one that had a position)
                a.pos.src = modes::Source::Adsb;
                for k in 0..3 {
                    let t = now + k * 20_000;
                    a.trace.points.push(Packed::pack(&point(*addr, *i, t)));
                    if *i % 6 == 0 {
                        a.trace.details.push(detail(*addr, t));
                    }
                    a.trace.tidy();
                    *i += 1;
                }
            }
            c.store.expire(now + 59_000, 300_000, c.cfg.trace_keep_ms);
            // what is held: points, bytes, aircraft
            let mut held = [0usize; 3];
            for s in c.store.shards() {
                let s = s.lock();
                held[1] += s.map.len() * std::mem::size_of::<Aircraft>() + s.gone.len() * (std::mem::size_of::<crate::track::Gone>() + 8);
                held[2] += s.map.len() + s.gone.len();
                for t in s.map.values().map(|a| &a.trace).chain(s.gone.values().map(|g| &g.trace)) {
                    held[0] += t.len();
                    held[1] += t.bytes();
                    // nothing older than a day and the chunk it is in, which
                    // is 127 points at most and may span a silence short
                    // of being forgotten
                    assert!(t.first_t().unwrap() + 24 * HOUR + 127 * 20_000 + 17 * 60_000 > now, "minute {minute}");
                }
            }
            for k in 0..3 {
                sum[k] += held[k];
                peak[k] = peak[k].max(held[k]);
            }
            if minute % 1440 == 1439 {
                days.push((sum.map(|s| s / 1440), peak));
                (sum, peak) = ([0; 3], [0; 3]);
            }
        }
        // a day of it is 40 aircraft x 3 points x 1440 minutes
        let (second, last) = (days[1], days[6]);
        assert!((165_000..181_000).contains(&second.0[0]), "{days:?}");
        for (day, d) in days.iter().enumerate().skip(2) {
            // points and bytes: the mean of each later day within a
            // fiftieth of the second day's, and never a peak above its by
            // more; aircraft, of which chance brings more or fewer in a
            // day, within a twentieth
            for (k, part) in [(0, 50), (1, 50), (2, 20)] {
                assert!(d.0[k].abs_diff(second.0[k]) * part <= second.0[k], "day {day}, {k}: {days:?}");
                assert!(d.1[k] * part <= second.1[k] * (part + 1), "day {day}, {k}: {days:?}");
            }
        }
        // and what a point costs in the end, all told
        let per_point = last.0[1] as f64 / last.0[0] as f64;
        assert!(per_point < 24.0, "{per_point}");
    }

    /// Every file under `dir`, by its path.
    fn tree(dir: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut todo = vec![std::path::PathBuf::from(dir)];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().map(|e| e.unwrap()) {
                if e.file_type().unwrap().is_dir() {
                    todo.push(e.path());
                } else {
                    out.insert(e.path().strip_prefix(dir).unwrap().to_str().unwrap().to_string(), std::fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn day_files_written_by_many_threads_are_those_written_by_one() {
        let (one, many) = (scratch("hist"), scratch("hist"));
        // traces across midnight: two day files for many aircraft
        let end = NOON + 12 * HOUR + 20 * 60_000;
        let (a, b) = (ctx(24 * HOUR), ctx(24 * HOUR));
        for c in [&a, &b] {
            fill(c, 300, end);
            // nothing of the gone ones is in a day file yet
            c.store.shards().iter().for_each(|s| s.lock().gone.values_mut().for_each(|g| g.trace.written = 0));
        }
        // 250 of the 300 have points no day file holds yet
        let files = write_history(&a, &one, 0, crate::track::SHARDS);
        assert_eq!(write_history_all(&b, &many, 5), files);
        assert!(files > 250, "{files}");
        let (one, many) = (tree(&one), tree(&many));
        assert!(one.len() == files && one == many);
        assert!(one.keys().any(|k| k.starts_with("2026/11/16/traces/")) && one.keys().any(|k| k.starts_with("2026/11/17/traces/")));
        // all is written now: a second pass has nothing to do
        assert_eq!(write_history_all(&b, &scratch("hist"), 5), 0);
    }

    #[test]
    fn a_restored_table_writes_the_same_day_files() {
        let (dir, a) = (scratch("state"), ctx(24 * HOUR));
        fill(&a, 120, NOON);
        save_state(&a, &dir, false).unwrap();
        let b = ctx(24 * HOUR);
        load_state(&b, &dir, NOON).unwrap();
        let (before, after) = (scratch("hist"), scratch("hist"));
        // 90 of the 120 had points not yet in their day file, and the
        // state remembers which
        assert_eq!((write_history_all(&a, &before, 2), write_history_all(&b, &after, 2)), (90, 90));
        assert!(tree(&before) == tree(&after));
    }
}

/// Stopping and starting with a large table, timed without waiting a day
/// for one: traces from a real state file are joined end to end and
/// handed out to as many aircraft as wanted. The table is built as it
/// was an hour ago and saved whole, as every hour; then the last hour is
/// added and it is saved as when stopping.
///
/// ```sh
/// AGGD_SCALE_FROM=/var/lib/aggd AGGD_SCALE_OUT=/tmp/scale \
///   cargo test --release -p aggd at_scale -- --ignored --nocapture
/// ```
///
/// `AGGD_SCALE_AIRCRAFT` (75000), `AGGD_SCALE_POINTS` (1000, the mean per
/// aircraft), `AGGD_SCALE_HOURS` (24, what the traces span, ending now),
/// `AGGD_SCALE_DETAIL` (40, points to a detail, which gives the table
/// the size one operator saw at these numbers; 0 for none),
/// `AGGD_SCALE_THREADS` ("1,4,8", for the day files), `AGGD_SCALE_V1=1`
/// to time the earlier format too. The state is left in OUT/state (and
/// OUT/v1) for timing a whole process on it: about a quarter of the
/// aircraft have points no day file holds, as when stopping at a busy
/// hour, and a process started on it and stopped within a minute has
/// the same hour's gain to save.
#[cfg(test)]
mod scale {
    use super::tests::ctx;
    use super::*;
    use std::time::Instant;

    fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    fn rnd(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    fn each_trace(c: &Ctx, mut f: impl FnMut(u32, &mut trace::Trace, bool)) {
        for s in c.store.shards() {
            let s = &mut *s.lock();
            s.map.values_mut().for_each(|a| f(a.addr, &mut a.trace, true));
            s.gone.iter_mut().for_each(|(addr, g)| f(*addr, &mut g.trace, false));
        }
    }

    fn size(c: &Ctx) -> (usize, usize, usize, usize, usize) {
        let (mut live, mut gone, mut points, mut details, mut bytes) = (0, 0, 0, 0, 0);
        each_trace(c, |_, t, l| {
            live += l as usize;
            gone += !l as usize;
            points += t.len();
            details += t.all().1.len();
            bytes += t.bytes();
        });
        (live, gone, points, details, bytes)
    }

    fn rss_mb() -> usize {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.trim().trim_end_matches(" kB").parse::<usize>().ok()).unwrap_or(0) / 1024
    }

    fn file_mb(dir: &str, name: &str) -> f64 {
        std::fs::metadata(format!("{dir}/{name}")).map_or(0.0, |m| m.len() as f64 / 1e6)
    }

    struct Sky {
        real: Vec<Vec<Point>>,
        aircraft: usize,
        factor: f64,
        span: Ms,
        per_detail: usize,
        now: Ms,
    }

    impl Sky {
        /// Aircraft `i`: its points with the details that go with some,
        /// and whether it is in the air now. The same each time asked.
        fn flight(&self, i: usize) -> (Vec<(Point, Option<trace::Detail>)>, bool, u64) {
            let (real, now, span) = (&self.real, self.now, self.span);
            let mut x = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i as u64 + 1);
            // each takes after one real aircraft, with a trace longer by
            // the same factor for all: short and long keep their shares
            let n = ((real[i % real.len()].len() as f64 * self.factor).round() as usize).max(1);
            let mut points: Vec<Point> = real.iter().cycle().skip(i % real.len()).flat_map(|r| r.iter().copied()).take(n).collect();
            // 23 in 100 are in the air now; the others left at some time
            // in the span
            let live = rnd(&mut x) % 100 < 23;
            let end = if live { now - rnd(&mut x) % 5000 } else { now - 960_000 - rnd(&mut x) % (span - 1_200_000).max(1) };
            // times as recorded, a minute between one real trace and the
            // next, all of it pressed into the span if longer
            let mut gaps: Vec<Ms> = points.windows(2).map(|w| if w[1].t > w[0].t && w[1].t - w[0].t < 3_600_000 { w[1].t - w[0].t } else { 60_000 }).collect();
            let room = span.saturating_sub(now - end).saturating_sub(60_000).max(1);
            let total: Ms = gaps.iter().sum();
            if total > room {
                gaps.iter_mut().for_each(|g| *g = (*g as u128 * room as u128 / total as u128) as Ms / 100 * 100);
            }
            let mut at = end / 100 * 100;
            for k in (0..n).rev() {
                points[k].t = at;
                at -= if k > 0 { gaps[k - 1].max(100) } else { 0 };
            }
            let mut d: trace::Detail = unsafe { std::mem::zeroed() };
            d.callsign = *b"SCL0000 ";
            d.callsign[3..7].copy_from_slice(format!("{:04}", i % 10_000).as_bytes());
            (d.squawk, d.category, d.emergency, d.nav_alt_mcp, d.nav_qnh, d.nav_modes) = ((i % 4096) as u16, 0xA3, 0, 36_000, 1013.2, 3);
            (d.nic, d.rc, d.version, d.nic_baro, d.nac_p, d.nac_v, d.sil, d.sil_type, d.gva, d.sda) = (8, 186, 2, 1, 9, 1, 3, 1, 2, 2);
            let flight = points
                .into_iter()
                .enumerate()
                .map(|(k, p)| {
                    if self.per_detail == 0 || k % self.per_detail != 0 {
                        return (p, None);
                    }
                    // what a transponder says of itself, less steady
                    // than it might be
                    let wobble = |x: &mut u64| (rnd(x) % 200) as f32 / 100.0 - 1.0;
                    let track = if p.track == u16::MAX { 0.0 } else { p.track as f32 / 10.0 };
                    (d.t, d.track, d.tas) = (p.t, track, if p.gs == u16::MAX { u16::MAX } else { p.gs / 10 + (rnd(&mut x) % 30) as u16 });
                    (d.mach, d.roll) = (0.78 + wobble(&mut x) / 50.0, wobble(&mut x) / 2.0);
                    (d.nav_heading, d.mag_heading, d.true_heading) = (track.round(), track + wobble(&mut x), track + 2.0 + wobble(&mut x));
                    (p, Some(d))
                })
                .collect();
            (flight, live, x)
        }

        /// Bring the table to `until`: points up to then; an aircraft
        /// with more to come is in flight, another is forgotten.
        fn fly(&self, c: &Ctx, from: Ms, until: Ms) {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..std::thread::available_parallelism().map_or(1, |n| n.get()).min(8) {
                    s.spawn(|| loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= self.aircraft {
                            return;
                        }
                        let (flight, live, mut x) = self.flight(i);
                        let addr = 0x100000 + i as u32;
                        let last = flight.last().unwrap().0.t;
                        let new: Vec<_> = flight.iter().filter(|(p, _)| p.t > from && p.t <= until).collect();
                        if new.is_empty() {
                            continue;
                        }
                        let mut s = c.store.shard(addr).lock();
                        let mut a = s.map.remove(&addr).unwrap_or_else(|| Box::new(crate::track::Aircraft::new(addr, last)));
                        for (p, d) in new {
                            a.trace.points.push(Packed::pack(p));
                            a.trace.details.extend(d.iter());
                            a.trace.tidy();
                        }
                        // points since the last pass of the day files: all
                        // of those in flight have some, and a few just gone
                        let flying = last > until || live;
                        let behind = flying || rnd(&mut x) % 100 < 2;
                        let n = a.trace.len();
                        a.trace.written = n - if behind { (1 + rnd(&mut x) as usize % 20).min(n) } else { 0 };
                        if flying {
                            s.map.insert(addr, a);
                        } else {
                            a.trace.compact(0);
                            s.gone.insert(addr, crate::track::Gone { seen: a.seen, pos: a.pos, nic: 0, rc: 0, last_place: None, heat_tag: 0, trace: std::mem::take(&mut a.trace) });
                        }
                    });
                }
            });
        }
    }

    #[test]
    #[ignore]
    fn at_scale() {
        let (Ok(from), Ok(out)) = (std::env::var("AGGD_SCALE_FROM"), std::env::var("AGGD_SCALE_OUT")) else {
            panic!("set AGGD_SCALE_FROM (a directory with a state file) and AGGD_SCALE_OUT");
        };
        let aircraft: usize = env("AGGD_SCALE_AIRCRAFT", 75_000);
        let mean: usize = env("AGGD_SCALE_POINTS", 1000);
        let span: Ms = (env("AGGD_SCALE_HOURS", 24.0) * 3_600_000.0) as Ms;
        let threads: Vec<usize> = env("AGGD_SCALE_THREADS", "1,4,8".to_string()).split(',').filter_map(|t| t.parse().ok()).collect();
        let now = crate::now_ms();

        // the real traces, whenever they were recorded
        let src = ctx(Ms::MAX / 4);
        load_state(&src, &from, Ms::MAX / 4).unwrap();
        let mut real: Vec<(u32, Vec<Point>)> = Vec::new();
        each_trace(&src, |addr, t, _| real.push((addr, t.all().0.iter().map(|p| p.unpack()).collect())));
        real.sort_by_key(|r| r.0);
        let have: usize = real.iter().map(|r| r.1.len()).sum();
        let (_, _, _, details, bytes) = size(&src);
        println!("source: {} aircraft, {have} points, {details} details, {:.2} bytes a point as held", real.len(), bytes as f64 / have as f64);
        drop(src);
        let sky = Sky { factor: mean as f64 * real.len() as f64 / have as f64, real: real.into_iter().map(|r| r.1).collect(), aircraft, span, per_detail: env("AGGD_SCALE_DETAIL", 40), now };

        let (v1, dir) = (format!("{out}/v1"), format!("{out}/state"));
        let _ = (std::fs::remove_dir_all(&v1), std::fs::remove_dir_all(&dir));
        let big = ctx(span);
        let t = Instant::now();
        sky.fly(&big, 0, now - 3_600_000);
        let (live, gone, points, ..) = size(&big);
        println!("an hour ago: {live} live + {gone} gone, {points} points, built in {:.1} s", t.elapsed().as_secs_f64());
        let t = Instant::now();
        let n = save_state(&big, &dir, false).unwrap();
        println!("hourly save: {:6.2} s  {n} aircraft, {:.0} MB", t.elapsed().as_secs_f64(), file_mb(&dir, FILE));

        sky.fly(&big, now - 3_600_000, now);
        let (live, gone, points, details, bytes) = size(&big);
        let record = live * std::mem::size_of::<crate::track::Aircraft>() + gone * (std::mem::size_of::<crate::track::Gone>() + 8);
        println!("now: {live} live + {gone} gone, {points} points, {details} details");
        println!("     traces {:.1} MB ({:.2} bytes a point), records {:.1} MB, process {} MB", bytes as f64 / 1e6, bytes as f64 / points as f64, record as f64 / 1e6, rss_mb());
        let mut behind = std::collections::HashMap::new();
        each_trace(&big, |addr, t, _| {
            if t.written < t.len() {
                behind.insert(addr, t.written);
            }
        });
        println!("     {} aircraft with points no day file holds", behind.len());
        let t = Instant::now();
        let n = save_state(&big, &dir, true).unwrap();
        println!("save when stopping: {:6.2} s  {n} aircraft, {:.0} MB", t.elapsed().as_secs_f64(), file_mb(&dir, FILE_ADD));
        let t = Instant::now();
        // (kept for longer than the span, so that nothing is at its edge)
        let fresh = ctx(2 * span);
        let n = load_state(&fresh, &dir, now).unwrap();
        let took = t.elapsed().as_secs_f64();
        let (_, _, p2, d2, b2) = size(&fresh);
        println!("restore: {took:6.2} s  {n} aircraft, traces {:.1} MB", b2 as f64 / 1e6);
        assert_eq!((n, p2, d2), (live + gone, points, details));
        let t = Instant::now();
        let n = save_state(&fresh, &format!("{out}/again"), true).unwrap();
        println!("save when stopping, no hourly save to add to: {:6.2} s  {n} aircraft, {:.0} MB", t.elapsed().as_secs_f64(), file_mb(&format!("{out}/again"), FILE));
        let _ = std::fs::remove_dir_all(format!("{out}/again"));
        drop(fresh);
        if env("AGGD_SCALE_V1", 0) != 0 {
            let t = Instant::now();
            super::tests::save_v1(&big, &v1);
            println!("save, earlier format:    {:6.2} s  {:.0} MB", t.elapsed().as_secs_f64(), file_mb(&v1, FILE_V1));
            let t = Instant::now();
            let fresh = ctx(span);
            let n = load_state(&fresh, &v1, now).unwrap();
            println!("restore, earlier format: {:6.2} s  {n} aircraft, traces {:.1} MB", t.elapsed().as_secs_f64(), size(&fresh).4 as f64 / 1e6);
        }
        for n in threads {
            each_trace(&big, |addr, t, _| {
                if let Some(w) = behind.get(&addr) {
                    t.written = *w;
                }
            });
            let hist = format!("{out}/hist-{n}");
            let _ = std::fs::remove_dir_all(&hist);
            let t = Instant::now();
            let files = write_history_all(&big, &hist, n);
            println!("day files, {n} threads: {:6.2} s  {files} files", t.elapsed().as_secs_f64());
            let _ = std::fs::remove_dir_all(&hist);
        }
    }
}

#[cfg(test)]
mod day_tests {
    use super::*;

    fn point(t: Ms, lat: i32, lon: i32) -> Point {
        Point { t, lat, lon, alt: trace::GROUND, alt_geom: trace::NONE, gs: 0, track: 0, rate: 0, geom_rate: 0, ias: 0, flags: 0, addr_type: 0 }
    }

    fn doc(points: &[Point]) -> String {
        let packed: Vec<Packed> = points.iter().map(Packed::pack).collect();
        let mut out = String::new();
        trace::render(0x4ca123, None, &packed, &[], &mut out);
        out
    }

    fn times(doc: &str) -> Vec<u64> {
        day_rows(doc).unwrap().iter().map(|r| r.0 * 100).collect()
    }

    /// The table after a restart without a state file, or after a crash,
    /// or with its first hours aged out, has less of the day than the
    /// file written before: the file keeps what it had.
    #[test]
    fn a_day_file_keeps_the_points_the_table_no_longer_has() {
        let t0 = 1_795_680_000_000;
        let p = |s: u64| point(t0 + s * 1000, 45_000_000 + s as i32, 9_000_000);
        let before = doc(&[p(0), p(30), p(60), p(90)]);
        // a restart: the table begins again later
        let after = doc(&[p(200), p(230)]);
        let whole = with_rows_of(&before, &after).unwrap();
        assert_eq!(times(&whole), [0, 30, 60, 90, 200, 230].map(|s| t0 + s * 1000));
        assert!(whole.starts_with(&after[..after.find("\"timestamp\"").unwrap()]) && whole.contains(&format!("\"timestamp\": {:.3},", t0 as f64 / 1000.0)));
        assert!(whole.contains("[0.00,45.000000,9.000000,") && whole.contains("[230.00,45.000230,9.000000,") && whole.ends_with("null]\n]}\n"));
        // a crash: the state file had the trace to 30 s, the day file to 90 s
        let whole = with_rows_of(&before, &doc(&[p(0), p(30), p(200)])).unwrap();
        assert_eq!(times(&whole), [0, 30, 60, 90, 200].map(|s| t0 + s * 1000));
        // written again from that: nothing more to add, and so on for good
        assert!(with_rows_of(&whole, &whole).is_none());
        // the ordinary case: the table has all the file has, and more
        assert!(with_rows_of(&before, &doc(&[p(0), p(30), p(60), p(90), p(120)])).is_none());
        // a file that cannot be read is replaced
        assert!(with_rows_of("{\"icao\":", &after).is_none());
    }

    /// A day begun by readsb and carried on by aggd, in the directory
    /// they share: readsb's rows stay, as it wrote them.
    #[test]
    fn a_day_file_written_by_readsb_is_carried_on() {
        let t0 = 1_795_680_000_000u64;
        let readsb = format!(
            "{{\"icao\":\"4ca123\",\"version\":\"readsb 3.16\",\"timestamp\": {:.3},\"trace\":[[0.0,45.1,9.1,36000,440.5,90.2,1,0,{{\"type\":\"adsb_icao\",\"flight\":\"A]B,[C \"}},\"adsb_icao\",36400,0,250,0.2],[30.02,45.2,9.2,36000,440.5,90.2,0,0,null,\"adsb_icao\",36400,0,250,0.2]]}}",
            t0 as f64 / 1000.0
        );
        let ours = doc(&[point(t0 + 30_000, 45_200_000, 9_200_000), point(t0 + 60_000, 45_300_000, 9_300_000)]);
        let whole = with_rows_of(&readsb, &ours).unwrap();
        // (the point both have, a hundredth apart, is the table's)
        assert_eq!(times(&whole), [0, 30, 60].map(|s| t0 + s * 1000));
        assert!(whole.contains("[0.00,45.1,9.1,36000,440.5,90.2,1,0,{\"type\":\"adsb_icao\",\"flight\":\"A]B,[C \"},\"adsb_icao\",36400,0,250,0.2],\n"));
    }
}
