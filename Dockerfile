FROM rust:latest AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/twiistsync /usr/local/bin/twiistsync

ENV XDG_STATE_HOME=/data

ENTRYPOINT ["twiistsync", "--daemon", "--config", "/config/config.json", "--backfill", "7", "--tidepool-refresh-secs", "86400", "-vv"]