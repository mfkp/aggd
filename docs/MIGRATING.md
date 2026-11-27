# From readsb to aggd on an aggregator

aggd replaces the readsb that runs with `--net-only` on the server. The
receivers, mlat-client on the receivers, the MLAT server, and anything
that is not readsb (ACARS, VDL2, HFDL, AIS, OGN ingest) stay as they are.

Do not switch a network over on the strength of this document. Run aggd
beside readsb first (step 1) and look at the differences.

## 1. Shadow run

readsb can forward everything it accepts to a second aggregator. On the
existing server, add one connector to readsb and restart it:

```
--net-connector=AGGD-HOST,30004,beast_reduce_plus_out --net-receiver-id
```

(ultrafeeder: add `adsb,AGGD-HOST,30004,beast_reduce_plus_out` to
`ULTRAFEEDER_CONFIG`; `--net-receiver-id` goes in `READSB_EXTRA_ARGS`.)
With `--net-receiver-id` each forwarded message carries the id of the
receiver it came from, so aggd sees the receivers, not one big feeder.

On the aggd host, trust that server's address, since it speaks for all
its receivers and will exceed any single-receiver limit:

```sh
aggd --net-bi-port 30004 --net-trusted READSB-HOST-ADDRESS \
     --http-port 8080 --db-file aircraft.csv.gz
```

MLAT and JAERO positions do not travel on that link. Point aggd at the
same sources readsb uses (step 2) or leave them out of the comparison.

Then compare, as often as you like:

```sh
tools/compare.py http://READSB-HOST/data/aircraft.json http://AGGD-HOST:8080/data/aircraft.json --rounds 20
```

What to expect: `rssi` differs (the forwarded stream is rate-limited, so
aggd averages fewer samples); positions and altitudes differ by what an
aircraft flies in the time between the two reads. Anything else is worth a
look, and worth reporting: an aircraft in one and not the other, a field
one has and the other never does, a position somewhere else entirely.

To look into one difference, tape the stream and replay it to both:
`aggbench record READSB-HOST:30005 day.tape --seconds 1800`, then
`tools/replay.sh day.tape`, and `aggbench dump day.tape HEX` for one
aircraft's messages. On the traffic aggd was developed against, about
one field in a hundred differs at any moment (README, "On real traffic").

`tools/soak.sh start READSB-HOST 30005 DIR` sets up exactly this on one
machine (its own readsb in Docker beside aggd) and logs the comparison
every ten minutes.

Leave it for a few days. Watch `/metrics` on aggd: memory
(`process` RSS from your own monitoring), `aggd_aircraft_tracked`,
`aggd_bad_frames_total`, `aggd_receivers_distrusted`.

## 2. The same inputs and outputs

| readsb | aggd |
|---|---|
| `--net-bi-port=30004` (receivers' `beast_reduce_plus_out`) | the same |
| `--net-sbs-in-port`, `--net-sbs-jaero-in-port`, `--jaero-timeout` | the same |
| `--net-connector=HOST,PORT,sbs_in_mlat` (results from mlatd / mlat-server) | the same |
| `--net-bo-port`, `--net-beast-reduce-out-port`, `--net-beast-reduce-interval` | the same |
| `--net-connector=HOST,PORT,beast_reduce_plus_out,uuid=UUID` | the same; MLAT results go out on the Beast outputs with `--forward-mlat`. Any number of consumers may connect to an output port; a readsb consuming one must not be in `--net-ingest` mode, which is for single receivers |
| `--net-sbs-port`, `--net-sbs-reduce`, `--forward-mlat`, `--forward-mlat-sbs`, `--net-sbs-jaero-port` | the same |
| `--net-json-port`, `--net-json-port-interval`, `--net-json-port-include-noposition` | the same |
| `--net-api-port=PORT` | the same (or `--http-port`; one server answers both) |
| `--write-json=DIR` + nginx serving `/data/` | `--http-port PORT` serves them from memory. `--write-json DIR` also writes `aircraft.json`, `receiver.json`, `stats.json`, `status.json`, `clients.json` and `receivers.json` to DIR every `--write-json-every` seconds (the directory is created; a failure to write is logged once), for bots and scripts that read from disk. readsb also keeps each aircraft's trace there when it keeps traces; with `--write-traces` aggd does too (`traces/xx/trace_recent_HEX.json` and `trace_full_HEX.json`, gzipped, rewritten as the trace grows) |
| `--write-json-globe-index`, binCraft, zstd | always on, rendered on request |
| `--write-globe-history=DIR`, `--write-state=DIR` | the same. aggd's state files are its own format: it does not read readsb's, so traces start empty on the first run. Stopping and starting: step 6 |
| `--db-file`, `--db-file-lt` | the same |
| `--json-trace-interval`, `--interactive-ttl` | the same; the interval is 30 s when not given, as readsb's is (it was 20 s at first), and the points kept between intervals follow readsb's rule (README, "Known differences") |
| `--net-receiver-id`, `--uuid`, outbound `beast_reduce_plus_out` to a hub | the same; the hub lists the edge under `--net-trusted`. As with readsb, `--net-receiver-id` also names the receiver on the full and the reduced Beast outputs (`--net-bo-port`, `--net-beast-reduce-out-port`, `beast_out` and `beast_reduce_out` connectors): one marker, `0x1a 0xe3` and the eight-byte id, before each batch of frames from a receiver. A receiver that gave no UUID has an id aggd makes up for the connection (`ffff…`), where readsb derives one of its own; ids arriving on a link are passed on only from links under `--net-trusted` |
| `--net-ingest` | accepted; aggd's own protections are always on (README) |
| `--heatmap`, `--heatmap-dir` | the same |
| `--modeac` | accepted; like readsb on an aggregator it shows nothing |
| `--net-uat-in-port` | the same |
| `--write-prom=FILE` | the same: what `/metrics` serves, written to FILE every `--write-json-every` seconds. aggd's own metric names, not readsb's |
| `--debug=…` | accepted; aggd logs what it logs |
| `--net-vrs-port`, `--net-ro-port`, `--net-ri-port`, ASTERIX | not there |

Receivers need no change: same host, same port, same protocol.

## 3. The map

Either keep nginx and proxy `/data/`, `/re-api/` and `/globe_history/` to
aggd, or let aggd serve the page too:

```sh
aggd ... --http-port 8080 --web-root /usr/local/share/tar1090/html
```

With `--web-root`, `/` is the page and the API is at `/re-api/`. A custom
map that reads `data/aircraft.json`, `data/aircraft.binCraft.zst`,
`data/globe_N.binCraft.zst`, `data/traces/…` or `re-api/?…` needs no change.
aggd sends `Access-Control-Allow-Origin: *` by default. Behind a proxy that
sets its own (nginx with `add_header Access-Control-Allow-Origin …`),
`--cors-origin none` leaves it out, so browsers do not get two values and
refuse the answer; `--cors-origin https://map.example.org,https://…` answers
only those pages, each with its own name.

Fields a patched readsb may add are behind flags: `--json-receiver-count`
adds `rcvrs` and `receiverCount` to each aircraft.

## 4. Limits to set

Defaults suit a network of single receivers on separate addresses.

- Many receivers behind one address (a club, CGNAT): raise
  `--max-connections-per-ip` (default 64).
- A partner network feeding in bulk over one link: `--net-trusted` it, or
  raise `--max-receiver-rate` (default 6000 messages/s per link).
- Your own receivers: list their addresses under `--net-trusted`. Besides
  exempting them from the limits, their positions then outrank any number
  of others' (see "A lying majority" in the README). Trust only what you
  run: a trusted link that relays strangers' data vouches for all of it.
- `LimitNOFILE` in the unit file must exceed the number of connections.

## 5. Switching over, and back

Stop readsb, start aggd on the same ports with the same arguments (the
unknown ones are reported and refuse to start; the ones in the table that
are "not there" must be removed). Keep readsb's configuration: going back
is stopping aggd and starting readsb. The `globe_history` directory is
shared in format, so history written by either is readable by tar1090.

## 6. Stopping and starting

On SIGTERM aggd does two things, in this order, and says how long each
took:

1. It saves the state (`--write-state`): what the traces have gained
   since the last hourly save, in `state.add` beside the hourly
   `state.bin`. That is less than a tenth of the table (72 MB of 950)
   and takes about a second. From then on nothing is lost, whatever
   happens to the process.
2. It brings the day files up to date (`--write-globe-history`): the file
   of every aircraft heard since its last ten-minute pass, which at a
   busy hour is every aircraft in the air. This is processor time, shared
   over all cores.

With 75,000 aircraft kept (76 million points, a table of 950 MB) and
17,400 of them in the air, on a six-core desktop processor of 2020 held
to four cores: 1 s for the state and 12 s for 18,600 day files; to eight
threads, 7 s in all; to two cores, 27 s. Before, the same took 85 s on
that machine whatever the cores (one operator saw 109 s on his): the day
files were written on one core, and then the whole state, recompressed.

So `TimeoutStopSec=60` in `deploy/aggd.service` is enough for that size
on four cores or more. Allow 1 s for every 400 aircraft in the air,
divided by the number of cores, and twice that on a slow machine.
Docker's default is 10 s, which is not: use `docker stop -t 60`, or
`stop_grace_period: 1m` in a compose file.

If the time runs out anyway and aggd is killed during step 2, the day
files it had not reached are written by the next start, from the state:
each aircraft's count of points not yet in a day file is part of it.
Killed during step 1 (or by a crash, or the power), the state on disk is
the last one that was finished: every state file is written under
another name and renamed when it is whole, so there is never half of
one, and a file that does not check out against its checksums is refused
rather than read. (`state.add` is not waited for to reach the disk,
which on a busy one took up to 15 s: if the machine loses power within
seconds of a stop, the start after may find it incomplete, say so, and
restore the hourly save alone.) After a crash the traces served lack at most the hour
since the last `state.bin`, and the day files at most the ten minutes
since their last pass.

Starting reads the state back as it is: under a second for that table
(before: 19 s, with the whole file and its 2.7 GB of unpacked points in
memory at once, 5 GB at the peak). Restored aircraft are unlisted until
heard again; their traces are served at once.

The first start after upgrading reads the `state.zst` of the version
before, which takes as long as it did then (17 s for that table, though
no longer 5 GB), and one minute later writes the first `state.bin`. A stop in that first minute writes
the whole state rather than an addition to it: a gigabyte, which is as
fast as the disk. `state.zst` is left where it is for a day, so that
going back within the day still finds its traces (as of the upgrade),
and is then removed. The version before cannot read the new files.

With `--write-traces`, the files in `traces/` are not completed when
stopping, and all of them are written again after a start (a minute or two
minutes for 150,000 files on four cores); until then some are as old as
the stop.
