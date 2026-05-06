# twiistsync

Sync data from Twiist Pump into Nightscout.

The Twiist Insight Follower API is used for real time data, while Tidepool may optionally be used for historical backfill.


## Config

Roll your own `config.json` based on `config.example.json`.
Be sure to fill in Twiist follower, Nightscout, and optionally Tidepool credentials. Runtime state defaults to `$XDG_STATE_HOME/twiistsync/`.

## Usage

List followed PWDs:

```bash
cargo run --release -- --config config.json --list-pwds
```

Run one live sync without posting:

```bash
cargo run --release -- \
  --config config.json \
  --once \
  --dry-run
```

Run continuously every 5 minutes, with an initial 7-day Tidepool backfill and daily Tidepool top-ups:

```bash
cargo run --release -- \
  --config config.json \
  -vv \
  --daemon \
  --poll-interval-secs 300 \
  --backfill 7 \
  --tidepool-refresh-secs 86400
```

Use `-v`, `-vv`, or `-vvv` when you want more stderr detail during a run.

Replay a saved Twiist package:

```bash
cargo run --release -- \
  --config config.json \
  --from-package twiist-package.json \
  --dry-run
```
