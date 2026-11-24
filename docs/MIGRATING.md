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
| `--write-globe-history=DIR`, `--write-state=DIR` | the same. aggd's state file is its own format: it does not read readsb's, so traces start empty on the first run |
| `--db-file`, `--db-file-lt` | the same |
| `--json-trace-interval`, `--interactive-ttl` | the same |
| `--net-receiver-id`, `--uuid`, outbound `beast_reduce_plus_out` to a hub | the same; the hub lists the edge under `--net-trusted` |
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
