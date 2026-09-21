# 独立 relay 镜像以 services/relay-server/ 为构建上下文。
# 该 crate 有独立 workspace 和已提交的 Cargo.lock；rusqlite 自带 SQLite，
# 构建和运行时均不需要系统 SQLite。
FROM rust:1.84-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --create-home --home-dir /data rebon
COPY --from=build /src/target/release/rebon-relay-server /usr/local/bin/rebon-relay-server
USER rebon
WORKDIR /data
# The SQLite database (trusted pairing ids + token HMACs only — never tokens,
# payloads, or E2E secrets) lives on the /data volume so pairings survive
# container replacement. REBON_RELAY_PUBLIC_URL and REBON_RELAY_TOKEN_HMAC_KEY
# must come from the deployment environment.
ENV REBON_RELAY_BIND=0.0.0.0:8080 \
    REBON_RELAY_DATABASE_PATH=/data/relay.sqlite3 \
    RUST_LOG=rebon_relay_server=info,tower_http=info
EXPOSE 8080
ENTRYPOINT ["rebon-relay-server"]
