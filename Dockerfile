# Builder and runtime must share a Debian release: a binary linked against a
# newer glibc than the runtime has fails at startup.
FROM rust:1-trixie AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/twiistsync /usr/local/bin/twiistsync

ENV XDG_STATE_HOME=/data

# --align-period-secs polls just after each 300 s pump upload rather than at a
# phase fixed by container start time, which had made every reading ~293 s
# late. --poll-interval-secs is the longest gap between polls.
ENTRYPOINT ["twiistsync", "--daemon", "--config", "/config/config.json", \
            "--backfill", "7", "--tidepool-refresh-secs", "3600", \
            "--poll-interval-secs", "300", "--align-period-secs", "300", "-v"]