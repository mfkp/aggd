# aggd

An ADS-B aggregator in Rust: receivers connect and send Beast, aggd keeps
one table of aircraft and serves it in readsb's formats, so tar1090, API
clients and downstream feeds work unchanged. It takes the place of readsb
in its `--net-only` aggregator role. It is not a radio; on a receiver use
[rx](https://github.com/flightportrait/rx).

Status: experimental. It has been run beside readsb 3.16.21 on synthetic
traffic, on recorded real traffic (25 minutes of one network's stream,
about 700 aircraft, replayed to both), and live on that stream for eight
hours at a time, and the two compared field by field; the numbers are
below. **It has not yet carried a network in production**, nor run for
days. The longest runs so far are two of 27 hours beside readsb: it
stayed up and used 57 to 59 % of readsb's CPU time, but ended at 192 MB
and 155 MB against readsb's 62 MB. Memory has since been reworked (see
"Memory" below): with a day of history it holds about 82 MB, where a
readsb keeping traces as well passed 100 MB within its first hour. The step before relying
on it is a shadow run of days;
[docs/MIGRATING.md](docs/MIGRATING.md) says how.

## Run

Option names follow readsb where the meaning is the same.

```sh
cargo build --release
./target/release/aggd \
    --net-bi-port 30004 \
    --net-sbs-jaero-in-port 30009 \
    --net-connector mlat-host,31003,sbs_in_mlat \
    --net-bo-port 30005 --net-beast-reduce-out-port 30006 --net-sbs-port 30003 \
    --http-port 8080 --web-root /usr/local/share/tar1090/html \
    --db-file aircraft.csv.gz \
    --write-globe-history /var/globe_history --write-state /var/lib/aggd
```

`aggd --help` lists everything. Flags readsb needs and aggd does not
(`--net-only`, `--net-ingest`, `--write-json-globe-index`, …) are accepted
and ignored. `Dockerfile` and `deploy/aggd.service` are there to start from.

## What it does

| | |
|---|---|
| In | Beast (receiver UUID; forwarded receiver ids from trusted links); BaseStation: plain, MLAT results, JAERO ADS-C, priority; UAT (dump978 raw lines) |
| Out, streams | Beast full, Beast reduced, either with receiver ids (`--net-receiver-id`; MLAT results included with `--forward-mlat`), BaseStation (optionally rate-limited, optionally with MLAT), BaseStation JAERO only, JSON position lines; each as a listener or an outbound connector |
| Out, HTTP | `aircraft.json`, `receiver.json`, `clients.json`, `receivers.json`, `stats.json`, `status.json`, `aircraft.binCraft[.zst]`, `globe_N.binCraft[.zst]`, `globeMil_N…`, `traces/xx/trace_{full,recent}_HEX.json`, `/globe_history/…`, `/metrics` (Prometheus), and the re-api queries: `all`, `circle`, `closest`, `box`, `find_hex`, `find_callsign`, `find_reg`, `find_type` (combinable, as tar1090 does), the `filter_*` set, `jv2`, `binCraft`, `zstd` |
| Web | `--web-root DIR` serves a directory beside the data. With tar1090's html directory aggd is the whole map server: no nginx, nothing written to a tmpfs |
| Kept | per-day trace files in tar1090's `globe_history` layout; traces across restarts; heat map files (`--heatmap N`); with `--write-traces`, each aircraft's live trace files beside the `--write-json` files, as readsb keeps them |
| Decoded | DF0/4/5/11/16/17/18/20/21; ADS-B versions 0–2 (identification, surface and airborne position, velocity, emergency, ACAS advisories, target state, operational status), TIS-B and ADS-R; Comm-B registers 2,0 3,0 4,0 5,0 6,0 |
| Derived | true heading from magnetic (WMM2025), wind, outside and total temperature, track from positions, airframe registration/type/flags from a tar1090-db file, military address blocks, round-trip time to each feeder |

## Protection against bad feeders

A network takes Beast from anyone. aggd assumes some of them are broken
and a few are hostile:

- **Connections**: a total limit, a limit per address, idle timeouts.
- **Rate**: messages per second per link (`--max-receiver-rate`); the excess
  is dropped, and a link far over for twenty seconds is refused for a
  minute, doubling on each repeat up to an hour.
- **Garbage**: a link sending mostly frames that fail a checksum the
  receiver should have checked is refused for five minutes.
- **Invented aircraft**: a link may introduce a thousand aircraft when it
  connects and `--max-new-aircraft` a minute after; when a link is refused,
  aircraft only it ever reported are forgotten. The table has a hard cap.
- **False positions**: the same transmission heard by two receivers is
  strong evidence; one receiver's word is not. A position that contradicts
  a confirmed track is held back until a second receiver relays the same
  message. If none does, the receiver is at fault; a receiver at fault on
  five different aircraft in a minute has its positions ignored for ten.
  If others do, the many are believed over the few: the track moves only
  when more receivers relay the new position than stand behind the old
  one. Equal numbers are two aircraft under one address, or a recording
  played back: the first is kept for as long as it goes on being heard,
  as readsb keeps it, and the second is shown once the first has been
  silent for seven seconds and seventeen of the second's messages have
  come. That is how an aircraft whose GPS is interfered
  with is followed (every receiver hears its jump), and how receivers
  that lie identically lose to an honest majority, and are blamed. The
  contradictions must also be a tenth or more of what the receiver sent
  that minute, so a relay carrying half a network is not taken for a liar
  on the strength of its volume.
- **A lying majority**: counting heads fails when the liars outnumber
  the honest. Against that, a position supplied or confirmed by a link
  listed in `--net-trusted` holds its aircraft for half a minute: while
  it does, no number of other receivers moves it, and those that try are
  blamed. A trusted link's position replaces at once one that only
  strangers gave, and a receiver heard over a trusted link is never
  distrusted. Your own receivers are the one thing a group of strangers
  cannot forge; with none of them hearing an aircraft, the majority is
  still believed. In the test of this (25 liars, 10 honest), the 20 or so
  aircraft only the liars could hear stay listed, by what they sent of
  identity and altitude, without a position once the liars are
  distrusted.
- **Input**: BaseStation lines are length-limited and numbers must be
  possible; HTTP paths cannot leave the directories they serve.
- `--net-trusted` exempts your own nodes, and only they may forward
  receiver ids. `status.json` and `/metrics` say what was refused and why.

`tools/hostile.sh` runs two instances on the same traffic, one of them also
fed by receivers that send garbage, lie about positions (each differently)
and invent aircraft at 20,000 messages a second. It passes when the two
outputs are the same and the receivers distrusted are exactly the ones
that lied; it does, also with 800 ms of delay and 30 % loss on every
link, and when the liars all tell the same lie (`--collude`). A fourth
scenario puts 25 colluding liars against 10 honest receivers on trusted
links: no aircraft moves and no honest receiver is distrusted (22 of the
25 liars were). The same scenario without `--net-trusted` is the
control: every aircraft is moved and all ten honest receivers are
distrusted.
`tools/fuzz.py` throws malformed input at every port.

What this does not stop: liars that are the majority of the receivers
hearing an aircraft that no trusted receiver hears (including a single
liar that is the only one), and
floods below the rate limits from many addresses.

## Why it is lighter

- Nothing is written to be served. readsb rewrites `aircraft.json`, every
  globe tile and the traces to a tmpfs every second whether or not anyone
  is looking. aggd renders a document when it is asked for and keeps it
  for one refresh interval. (`--write-traces` writes the trace files
  anyway, for readers of readsb's files on disk: only an aircraft's
  that has grown, and never the globe tiles.)
- A transmission heard by fifteen receivers is decoded once. The other
  fourteen copies match a short per-aircraft list of recent frames.
- Memory follows the aircraft in the air, not the day's traffic. A day
  of history is mostly aircraft that left hours ago: once silent, an
  aircraft is reduced to its trace and last position, a tenth of a
  tracking record, and is restored if it returns. Trace points are 24
  bytes; an aircraft in flight keeps its newest few dozen open and packs
  the rest away in chunks of about a hundred (differences between
  successive points, by column, then zstd: about 12 bytes a point on
  real traces). A chunk is written once and dropped whole when it ages
  out, so the heap is not churned: recompressing a growing trace in
  place left 14 MB of holes within an hour. The same chunks, as they
  are, make up the state file: saving copies them and restoring reads
  them, and since none is ever rewritten, stopping only has to save the
  ones added since the hourly save.
- The airframe database (645,000 airframes, 34 MB of text) is held
  compressed in 32 KB blocks, 20 MB less; a block is opened for the one
  lookup each aircraft gets.

## Measured

`tools/bench.sh` feeds the same synthetic traffic to readsb (in Docker)
and aggd, measures both, and compares their `aircraft.json` field by
field. One machine (12 cores), 2026-11-16, 60 s windows, both with the
airframe database loaded, aggd polled for `aircraft.json` once a second:

| traffic | readsb | aggd |
|---|---|---|
| 235k msg/s, 3,800 aircraft, 300 receivers; readsb with globe index, binCraft, traces | 38 % of a core, 661 MB | 12 %, 73 MB |
| the same; readsb writing `aircraft.json` only | 19 %, 76 MB | 13 %, 65 MB |
| 1.06M msg/s, 8,600 aircraft, 600 receivers; readsb with globe index, binCraft, traces | 59 %, 882 MB | 26 %, 82 MB |

Memory was read at the end of each run; neither process had reached a
steady state for trace history by then. In the 235k runs
the two listed the same aircraft and differed in about 10 of 11,412
aircraft comparisons (signal-level averages, an aircraft caught mid-turn).
In the 1.06M run readsb lacked 16 aircraft aggd had (some generator links
to it had dropped) and 245 of 25,767 comparisons differed in signal level.

What these numbers do not show: real traffic. The generator's messages are
clean, every receiver hears a transmission in the same 100 ms (which
flatters duplicate handling), readsb ran with `--net-ingest` off because
its bad-receiver heuristic dropped the synthetic feeders, and aggd was not
serving tiles to a map during the runs, where readsb wrote them all. For
plain ingest the two are close: readsb is efficient C.

## On real traffic

`aggbench record` taped 25 minutes of a live network's Beast stream (an
edge forwarding to a hub: rate-limited, with receiver ids, about 700
aircraft worldwide, 1,800 messages a second). `tools/replay.sh` plays a
tape to a fresh readsb and a fresh aggd at the speed it arrived and
compares their `aircraft.json` as it plays.

| | aircraft | fields compared | differing |
|---|---|---|---|
| 25-minute tape, at 5 min | 697 | 73,352 | 0.83 % |
| at 10 min | 677 | 70,662 | 0.46 % |
| at 15 min | 642 | 68,860 | 0.99 % |
| at 20 min | 637 | 68,410 | 0.72 % |

(Each row is three snapshots three seconds apart; the aircraft count is per
snapshot. The first comparison of all, before any of the rules below,
was 2.4 %.) Both list the same aircraft but for a handful at the edge of
the 60-second listing window.

Live on the same stream, both started together and compared every ten
minutes (`tools/soak.sh run`): aggd stayed up for each of two eight-hour
runs and a shorter one, on about four fifths of readsb's CPU time while
also keeping a day of traces, which readsb was not asked to. The first
run found two things no shorter test had: aircraft parked for more than
ten minutes lost their position, and aircraft back after hours of
silence were remembered when readsb had forgotten them. Both are fixed
(see the rules below). The share of fields differing follows the time of
day, with the traffic, rather than how long either has been up: a fresh
start mid-day began where the previous run had ended.

A fourth run found the worst fault so far, in the protection itself: two
hours in, the link that relays half the network was taken for a lying
receiver and its positions were ignored from then on, leaving about 700
aircraft without a position for six hours. The penalty was meant to last
ten minutes but the connections were never told it had ended, and it was
triggered by a count of contradicted positions that a large relay reaches
on honest traffic. Now the penalty ends, and a receiver is distrusted
only when the contradictions are a tenth or more of what it sent. The
run after that, eight hours overnight at about half the daytime traffic,
stayed between 0.64 % and 0.78 % differing in every hour (worst single
check 1.34 %), with nothing distrusted.

The long runs also showed two costs that grew with uptime, neither
visible in a one-minute benchmark. Memory rose in steps on the hour: the
hourly state file was built whole in memory, and glibc kept the space;
it is now streamed, the allocator is held to two heaps and free pages
are handed back (`aggd_table_bytes` in the metrics says how much is
real data). And CPU time crept up towards readsb's: every trace that had
grown was rewritten to its day file each minute, which with eight hours
of traces in memory was about half of all CPU time; the pass now takes
ten minutes. A sixth run, eight daytime hours with the memory fixes,
held 0.76 % to 1.03 % differing in every hour and grew smoothly instead
of in steps; the history change was measured side by side on live
traffic (1,410 against 2,658 CPU ticks in 40 minutes). A seventh run,
eight hours from evening into night with everything above, used half of
readsb's CPU time throughout (9,366 ticks against 18,234; the same
share in the last hour as in the first), differed in 0.64 % to 0.89 % of
fields in every hour, and ended at 97 MB. The same traces loaded into a
fresh process take 89 MB, of which 43 MB is the table and its traces and
the rest mostly the airframe database: what remains is data, and it is
bounded by the hours of trace kept (`--trace-hours`, 24 by default).

An eighth and a ninth run of 27 hours each (a full day and the start of
the next, 680 to 2,250 aircraft) had aggd alive at each of their 162
checks, differing from readsb in 0.88 % and 0.77 % of fields at the
median (0.39 % to 1.37 %), on 59 % and 57 % of readsb's CPU time.

### Memory

Those two runs ended at 192 MB and 155 MB against readsb's 62 MB, and
were still rising slowly after the day of history had filled. Nearly all
of it was three things: a full 1.9 KB tracking record for each of the
day's 15,000 aircraft, of which 2,000 are in the air at once; traces
held uncompressed; and the airframe database as 34 MB of text. All three
were changed (see "Why it is lighter"). The day of traces the ninth run
ended with (15,260 aircraft, 2.8 million points), loaded into each
build:

| | eighth run's build | ninth run's | now |
|---|---|---|---|
| empty, database loaded | 42 MB | 42 MB | 20 MB |
| with the day of traces | 160 MB | 166 MB | 55 MB |

Traces fetched from the old and new builds are byte for byte the same.
(The saved file of that time left out the detail kept once a minute or
so beside the points, so a day lived through was somewhat larger than a
day loaded; it is saved too now.) On the synthetic load the process went from 62 MB to 39 MB.

Started beside readsb with that day of history already loaded, so in
the state the earlier runs took 24 hours to reach, and at the afternoon
peak of about 2,100 aircraft, aggd began at 67 MB against readsb's
59 MB. Over the next five hours it rose to 82 MB and then stayed at 81
to 82 MB for the following three and a half, while readsb settled at
54 MB. Of aggd's 82 MB, 46 MB was the table and its traces (2.9 million
points, 15,000 aircraft) and 14 MB was free space the allocator had not
returned. `/metrics` gives the table's size (`aggd_table_bytes`) and the
allocator's used and free bytes (`aggd_heap_used_bytes`,
`aggd_heap_free_bytes`), which is how the growth was found to be holes
and not data. That readsb was run bare, though: no globe index, no traces, no history
files, while aggd always keeps a day of traces and writes the history.
A second readsb started on the same stream with what a tar1090
aggregator runs (`--write-json-globe-index --write-globe-history
--heatmap 30`) held 82 MB a minute after starting, with no history yet,
124 MB after three hours, 256 MB after fifteen and 347 MB after 38,
when it was stopped, and used 42,500 CPU ticks an hour (an eighth of a
core). Over the 48 hours of the same run aggd stayed at 91 to 92 MB and
used 1,620 ticks an hour, and the bare readsb 57 MB and 2,720.

#### A large network

An operator with 740 receivers (50,000 messages a second, 12,000 to
17,400 aircraft at once, 75,561 with a trace over the day) saw memory
grow by about 50 MB an hour, to 1,416 MB at the evening peak, of which
the table was 1,000 to 1,020 MB. That is the day filling, at his 76
million or so points a day, and it stops there: a trace is cut at
`--trace-hours` (whole chunks, so a trace may run up to one chunk, 127
points at most, past it), and an aircraft not heard for that long is
dropped. What the table holds in the steady state:

    11.5 MB for each million points of the last 24 hours
     + 4 KB for each aircraft in the air (2 KB of tracking record, and
            its newest points, up to 127, at 24 bytes)
     + 0.3 KB for each aircraft kept for its trace
     + 36 bytes for each detail (what else the aircraft said of itself,
            kept beside a point when it changed, and every ten minutes)

which for him is 875 + 70 + 23 MB and the details, or 13 MB for every
thousand aircraft of the day at his thousand points each. (Details came
to one for every ten points in 25 minutes of real traffic, which would be
a fifth more; his table says less than a tenth over a day.) (The 11.5 is 10.7 bytes a point
packed, and the rest what each chunk and each buffer costs besides; it
was 12.0 before chunks were compressed harder, zstd level 13 for 3: a
table of 76 million points went from 916 to 877 MB, read back the same.
What else was tried on a day of real traces: chunks four times the size,
7 % less but a trace then outlives its hours by as much; a trained
dictionary, 6 %; level 19, 1 % more than 13 for five times the time.)
`aggd_trace_points` and `aggd_table_bytes` in `/metrics` are the two
sides of it.

That it is flat after the first day is tested three ways. A week of
steady traffic in simulated time (a test, `a_week_of_steady_traffic_
holds_one_day_of_it`): the points, bytes and aircraft held on days three
to seven are within 2 % of the second day's (5 % for the aircraft). A
25-minute tape of real traffic (900 aircraft at once) played three times
over with `--trace-hours 0.1`, so that the six minutes kept pass twelve
times: from the first quarter hour to the last the table held 14,000 to
14,700 points in 2.7 to 2.9 MB (4.0 for the minutes when the end of the
tape and its beginning overlap), and the process 10.0 to 10.5 MB from
the 25th minute on. And the same with 18 minutes kept, so that aircraft
are forgotten and return, for 75 minutes: 43,500 to 45,900 points in 4.6
to 5.3 MB from the 25th minute on, the process 12.1 to 12.8 MB.

The memory of the process beyond the table (his 400 MB) is not accounted
for here: `aggd_heap_used_bytes` less `aggd_table_bytes` is what else is
in use (the rendered documents, the connections' buffers, the airframe
database), `aggd_heap_free_bytes` what the allocator holds and has not
given back.

Stopping and starting at that size are in `docs/MIGRATING.md`, step 6;
`at_scale` in `persist.rs` builds such a table from any real state file
and times them.

Real traffic is where the rules that no standard states show up, and
most of the work was finding them, one experiment at a time, with
readsb as the reference (`tools/ttl.py`, `tools/wind_probe.py`,
`tools/alt_probe.py`, `tools/probe.py`, and reading single aircraft out
of the tape with `aggbench dump`):

- how long each field is kept (60 s for most, three minutes for the
  callsign) and when a lesser source may replace a better one (15 s);
- an aircraft is an ADS-B one until a message comes a minute and a half
  after its last own position; without a position its stated version
  holds 45 s, stating it again renewing nothing; and nothing lapses
  until a message arrives to find it old (`tools/version_probe.py`);
- a surveillance or air-to-air reply's altitude is taken at once only
  while a better one is fresh, otherwise when it agrees or is said twice;
- one position message is placed relative to the last position up to ten
  minutes old; even and odd messages pair up to 30 s apart when a ground speed was
  heard in the last 20 s, less above 318 kt (about 2.65 NM of travel),
  and 10 s when no recent speed is known (`tools/pair_probe.py`);
- Comm-B registers 4,0 5,0 6,0 with fields marked absent; a reply that
  reads as one register only is that register, and one that reads as two
  is settled by whether Mach and airspeed agree at the altitude in the
  reply itself, or dropped (`tools/bds50_probe.py`);
- wind is derived when airspeed and heading arrive within a second and a
  quarter of ground speed and track (shown at 1.2 s apart, not at 1.25),
  with true airspeed up to two and a half seconds old and less than ten
  degrees of roll, and lasts a minute or until the aircraft is 500 ft above or below
  where it was taken; outside and total temperature when Mach and true
  airspeed arrive within two and a half seconds of each other
  (`tools/wind_probe2.py`); a squawk counts when heard twice;
- a reply addressed by parity alone (altitude, identity, Comm-B) is
  taken only within 15 s of a message that carries its own checksum
  from the same aircraft (taken at 14 s, ignored at 15), and it is that
  checked message, not the replies, that keeps the aircraft listed;
- true heading follows from a magnetic one as that heading arrives, and
  only while the position is under a minute old; a position that comes
  afterwards does not bring it;
- a geometric altitude follows from a barometric one and the difference
  a velocity message carries, once per new altitude whichever kind of
  message brought it, and lapses with the altitude
  (`tools/geom_probe.py`);
- on the ground, the heading field is the track unless the aircraft's
  status message says otherwise; rebroadcast (ADS-R, TIS-B) versions are
  kept apart from the aircraft's own;
- (found only on an eight-hour live run) a message from the ground needs
  a rough place to be read against, and the aircraft's own last position
  serves however old it is; an aircraft silent for a quarter of an hour
  (five minutes if it never had a position) starts afresh when it
  returns, but its last position is still shown, up to an hour old.

## Checked against

- **readsb on real traffic**: above.
- **readsb, in bulk, synthetic**: `tools/bench.sh` and `tools/compare.py`.
- **readsb, message by message**: `tools/probe.py` sends one frame of each
  kind to both and diffs JSON and BaseStation output after each.
- **A real readsb as feeder**: it connects with `beast_reduce_plus_out`,
  its UUID is taken, and the round trip is measured with its ping reply.
- **tar1090**: the real page (3.14.1725) served by aggd draws the aircraft,
  the table, and a selected aircraft's trace.
- **A live network's public data** (ADSB Italia's `aircraft.json`, one
  snapshot, 7,472 aircraft): wind and temperature formulas agree with the
  reported values (median error 0.6 kt, 0.5°, 0.3 °C); magnetic declination
  agrees on 2,524 aircraft (median 0.006°); the ACAS advisory in it decodes
  to the same object, byte for byte.
- **binCraft**: `tools/bincraft.py` decodes readsb's own file against
  readsb's own JSON, then aggd's against aggd's.
- **Deployment**: the Docker image builds and serves; the systemd unit
  starts, and on stop writes its history and state.

## Known differences from readsb

Replaying 25 minutes of recorded traffic (2026-11-19, 41 paired
snapshots, about 945,000 fields) four times as the rules for wind,
temperature, true heading, unchecked replies and position pairing went
in gave 0.63 %, 0.68 %, 0.64 % and 0.69 % of fields differing, against
0.74 % to 0.76 % before them; the spread between runs of the same
recording is about 0.05, so only the step from before to after is
measured, not the order within.

On the recorded traffic, in rough order of how often:

- Wind appears for some aircraft in one and not the other (about 3 % of
  aircraft with wind, after the pairing rules above were measured;
  temperature under 1 %). readsb sometimes keeps a wind reading past a
  minute or through a change of altitude, by a rule not yet found.
- An aircraft that already has a place is only placed from two halves
  heard by the same receiver, where readsb pairs any two; a first fix
  pairs across receivers in both.
- Signal level: readsb shows none (-49.5) for some aircraft aggd has a
  level for.
- Altitude, accuracy (NACp, SIL, Rc) and version differ for well under
  a percent of aircraft at any moment. One case is not understood: an
  aircraft whose version readsb set back to 0 as it was first placed.
- The DF18 non-transponder position noted in the probe.
- Two places under one address (`tools/jump_probe.py`, 40 short
  histories through one receiver and through two): while the first
  aircraft is heard neither moves to the second, through ten seconds of
  the second alone. When the first stops, readsb shows the second 8 to
  45 s later and aggd 10 to 26 s; at 60 km and at 9,000 km apart readsb
  takes about 40 s whatever the message rate, by a rule not found.
- The `box` and `circle` queries, asked of both on live traffic twelve
  times: the same aircraft, but for one whose position was 59.9 s old
  in readsb's answer and gone from aggd's, and one aggd had first
  placed 0.3 s before. An aircraft or two more or fewer at any moment
  is the two not answering at the same instant.
- An aircraft placed by a BaseStation feed and heard directly as well
  (`tools/sbs_src_probe.py`): version, accuracy figures and kind change
  at the same moments in both; a new aircraft is shown a second or two
  sooner by aggd.
- BaseStation output, plain and with `--net-sbs-reduce`, was compared
  with readsb's under load (1,400 aircraft, 40 receivers): the same
  lines by type to within 3 %. The reduced stream leaves out altitude
  replies of aircraft with a current ADS-B altitude, as readsb's did;
  whether readsb does so for every aircraft was not measured.
- MLAT and ADS-C beside direct reception (`tools/src_probe.py`): an
  aircraft's own position always wins, MLAT replaces it after 15 s of
  silence, and an ADS-C report places only an aircraft that has had no
  other position for five minutes, all as measured on readsb; 14 of the
  16 histories agree. The two that do not are when an aircraft with
  ADS-C reports waiting, and no position, is called `adsc`: readsb's
  own answer moved between runs (102 s and 146 s after the same input).
- The reduced Beast stream carries frames unaltered, with the receiver's
  timestamp and signal as readsb's does, but chooses differently which
  to keep: of 1,296 frames, readsb's kept 833 and aggd's 720, 612 of
  them the same. The full stream was identical, all 1,296
  (`tools/beast_out_probe.py`). readsb also sends an empty Mode A/C
  frame as a keep-alive every few seconds; aggd sends one only after
  30 s of silence.
- A consumer too slow for an output stream stays connected and loses
  the oldest of what it could not take, whole frames at a time
  (`aggd_output_skipped_total` counts them); one that takes nothing for
  20 s is dropped. Until 0.1.2 output went out a small batch per write,
  and on a busy network a consumer of the full stream was cut off every
  couple of seconds (600 feeders: 17 connections in 30 s; one since).
- `stats.json` has every key readsb has, but the CPR and CPU breakdowns are
  zero or approximate.
- Mode A/C: `--modeac` is accepted and, as with readsb on an aggregator,
  changes nothing shown: 180 Mode A/C messages sent to readsb with
  `--modeac` produced no aircraft and no message count.
- The heat map files decode with the same decoder as readsb's and tar1090
  fetches and parses them; the dots themselves were not seen, the test
  machine having no WebGL. readsb samples about a quarter of the aircraft
  per interval where aggd writes all of them.
- UAT input shows the exact position the message carries, where readsb
  shows it re-quantised by a metre or two, and shows a position for
  aircraft on the ground, which readsb does not. It was checked with
  frames built from the standard's layout (`tools/uat_probe.py`), sent to
  both; no recorded UAT traffic was available.
- Traces: which positions become points is readsb's rule as far as it
  could be measured (the list is at `trace::due`), and on scripted
  flights the two keep the same points at the same moments
  (`tools/trace_probe.sh`). At first aggd kept a point every interval
  and at 2 degrees or 200 ft of change, and an operator's shadow run
  found its day files with a median 66 % of readsb's points: readsb
  keeps far more of a climb or descent low down (every 75 ft below
  7,000 ft), of the ground, of an aircraft's first minute and of the
  minute after take-off and landing, and a point for every change of
  vertical rate over 200 ft/min. Beside readsb on the same live traffic
  for 35 minutes (840 aircraft, interval 30 s), counting what each
  holds for good: 96.8 % of readsb's points (per aircraft a median of
  96 %, 88 % at the tenth percentile, 110 % at the ninetieth), 314
  points an aircraft-hour to its 322; the old rule had 75.0 % (75 %,
  43 %, 102 %) and 246. `trace_recent` the same: 96.9 % against 78.0 %.
  It costs what the points cost: with the same traffic from the same
  start, after half an hour, 61,384 points to the old rule's 41,238,
  4.5 MB of table to 3.4 MB, 12.0 MB resident to 10.3 MB, and a tenth
  more processor time (0.34 % of a core to 0.31 %).
  What is left: readsb's `trace_full` of an aircraft it has stopped
  hearing, written some ten to sixteen minutes later, ends with every
  position of its last 47 s (about 95 points), which are gone from the
  file again if the aircraft returns; aggd keeps the last position
  heard and no more (keeping them would take 2.3 KB for every aircraft
  in the air and a kilobyte of trace for every time one is lost).
  Counting those files too, aggd has 81.8 % of all the points readsb
  published in the 35 minutes (median 94 %, tenth percentile 30 %: the
  aircraft heard briefly), the old rule 63.4 % (68 %, 19 %). readsb keeps a point 350 ft from the last above
  12,000 ft about one time in four, where aggd waits for 375; an
  aircraft that sends no track gets points from the track readsb works
  out of its positions. `trace_recent` holds 92 points in readsb and
  142 here. readsb wrote no day file in two and a half hours of running
  nor when stopped, so its day files were not compared: what is
  compared is what it publishes in `trace_recent` and `trace_full`.
  One point in four carries what else was known of the aircraft
  (callsign, squawk, selected altitude and the rest), as in readsb's
  files; they were kept once a minute at first, and stamped so that
  hardly any was ever shown.
- Day files (`--write-globe-history`) keep what is in them. A day file
  is written from the table, and at first was simply replaced: after
  a start without `--write-state` the day's earlier points were lost
  from the file of every aircraft heard again (346 of 420 files in a
  five-minute trial), after a crash those since the hourly state file
  (214 of 493), and with `--trace-hours` under 24 whatever had aged
  out. Now the file is read first in those cases and its points stay
  (0 files lost points in the same trials). A day begun by readsb in a
  shared directory is carried on the same way.
- Trace files on disk (`--write-traces`): readsb's `trace_recent` is
  rewritten about every 4 s while the aircraft adds points, as aggd's
  is; it holds 92 points where aggd's holds the 142 the HTTP server
  serves. readsb rewrites `trace_full` on a cycle that lengthens with
  the table (about 2 minutes at 300 aircraft, a median of 8 at 15,000);
  aggd rewrites it once it lacks half of what `trace_recent` holds, and
  at least every five minutes while it grows, so the two files together
  always hold the whole trace. readsb leaves the files of aircraft it no
  longer keeps; aggd removes them as their traces age out, and at start
  those of aircraft it did not restore.
- Not present: raw/ASTERIX/Kinetic/VRS formats, the range outline,
  `globeRates.json`, tar1090's `chunks/` history (traces are used
  instead), Comm-B registers beyond 2,0 3,0 4,0 5,0 6,0.

## How it was written

From the standards (Annex 10 vol. IV, DO-260B) for decoding, and from
readsb's behaviour for everything else: the same input was fed to both and
the outputs compared. readsb's source was not read. Three tables are data
taken from outside: the large globe tiles in `tiles_data.rs` (readsb
publishes them in `receiver.json`), the military address blocks in
`mil_ranges.rs` (found by showing readsb one address in every sixteen and
reading its flag back, `tools/milranges.py`), and the World Magnetic Model
coefficients in `wmm_data.rs` (NOAA, public domain).

## Tools

- `tools/trace_probe.sh`: scripted flights (`aggbench flight`: a climb,
  a turn, a silence, a landing, each at one steady rate) played to
  readsb and aggd, and the trace points each kept, flight by flight.
  `tools/trace_watch.py` collects the points readsb publishes as it
  goes (it rewrites `trace_full` rarely and not when stopped);
  `tools/trace_compare.py` compares two aggregators' traces of live
  traffic: points per aircraft, per aircraft-hour, and what the points
  only one of them has were doing.

- `tools/jump_probe.py`, `tools/mlat_track_probe.py`,
  `tools/sbs_src_probe.py`, `tools/rid_probe.py`: two aircraft under one
  address; the direction shown for an aircraft only MLAT places; one a
  BaseStation feed places that is heard directly too; receiver ids on
  the Beast outputs. Each plays the same thing to readsb and aggd.
- `tools/out_probe.py`: consumers on the Beast outputs of an aggregator
  under load, one of them slow: connections each needed, frames, bytes
  that were not a frame.
- `tools/src_probe.py`: sixteen short histories of an aircraft heard
  directly, placed by MLAT and reported over ADS-C in turn, played to
  readsb and aggd, with when each changes what it shows.
  `tools/beast_out_probe.py`: the same frames into both, their Beast
  outputs compared frame by frame.

- `tools/check.sh [quick]`: unit tests, malformed input, hostile
  receivers, readsb side by side; start here. (These scripts stop the
  aggd and aggbench they started and no other, so they can be run where
  an aggregator is working; they do use ports 37004 to 39152.)
- `aggbench record SOURCE FILE`, `tools/replay.sh TAPE`, `aggbench dump
  TAPE HEX`: tape a real Beast stream, replay it to readsb and aggd,
  read one aircraft out of it. `tools/shadow.sh HOST PORT` does the same
  live.
- `tools/ttl.py`, `tools/wind_probe.py`, `tools/alt_probe.py`,
  `tools/uat_probe.py`: experiments that measure a reference's rules.
- `tools/soak.sh start HOST PORT DIR`: a shadow run that stays up, logging
  the comparison and both processes' memory every ten minutes.
- `aggbench sky HOST:PORT...`: a synthetic sky and receiver network,
  optionally with `--garbage`, `--liars`, `--flood` receivers.
- `tools/bench.sh PLANES RECEIVERS [full|bare] [SECONDS]`: beside readsb.
- `tools/compare.py REF CANDIDATE`: field-by-field `aircraft.json` diff.
- `tools/probe.py`, `tools/run-probe.sh HEX`: one message at a time.
- `tools/hostile.sh`, `tools/fuzz.py`: misbehaving receivers, bad input.
- `tools/bincraft.py FILE [aircraft.json]`: binCraft decoder and checker.
