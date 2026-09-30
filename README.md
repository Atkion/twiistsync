# twiistsync

Sync data from Twiist Pump into Nightscout.

The Twiist Insight Follower API is used for real time data, while Tidepool may optionally be used for historical backfill.


## Config

Roll your own `config.json` based on `config.example.json`.
Be sure to fill in Twiist follower, Nightscout, and optionally Tidepool credentials. Runtime state defaults to `$XDG_STATE_HOME/twiistsync/`.

Required: `twiist.username`, `twiist.password`, `twiist.pwd_uuid` (see `--list-pwds`), `nightscout.website`, and `nightscout.permission_role`. Everything else is optional:

| Field | Default |
| --- | --- |
| `twiist.refresh_token_path` | `$XDG_STATE_HOME/twiistsync/session.json` |
| `twiist.cognito_pool`, `twiist.cognito_client_id`, `twiist.follower_service_url` | Twiist Insight app values |
| `nightscout.timezone` | derived from Tidepool pumpSettings (profile push only) |
| `nightscout.glucose_unit` | `mg/dl` (profile push only) |
| `nightscout.carbs_hr`, `nightscout.delay` | `20` (profile push only) |
| `sync` (whole section) | all defaults below |
| `sync.interval_secs` | `300`; `--poll-interval-secs` overrides it |
| `sync.tidepool_watermark_path` | `$XDG_STATE_HOME/twiistsync/tidepool_state.json` |
| `sync.emit_glucose`, `emit_insulin`, `emit_pump_events`, `emit_food`, `emit_device_status` | `true` |
| `tidepool` (whole section) | needed only for `--backfill`, `--backfill-profile`, `--tidepool-refresh-secs` |
| `tidepool.base_url` | `https://api.tidepool.org` |
| `tidepool.patient_uuid` | none; required by the Tidepool features |

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

Run continuously every 5 minutes, with an initial 7-day Tidepool backfill and hourly Tidepool top-ups:

```bash
cargo run --release -- \
  --config config.json \
  -v \
  --daemon \
  --poll-interval-secs 300 \
  --align-period-secs 300 \
  --backfill 7 \
  --tidepool-refresh-secs 3600
```

Use `-v`, `-vv`, or `-vvv` when you want more stderr detail during a run. `-vv` and above print every Twiist package and every document posted, health data included.

Replay a saved Twiist package:

```bash
cargo run --release -- \
  --config config.json \
  --from-package twiist-package.json \
  --dry-run
```
