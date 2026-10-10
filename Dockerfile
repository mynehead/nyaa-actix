# syntax=docker/dockerfile:1
# nyaa-actix web site. Build: docker build -t nyaa-actix .
# The full stack (site, PostgreSQL, tracker, optional Meilisearch) is in compose.yml; see docker/DEPLOY.md.

# ---- build ----
FROM rust:1-bookworm AS build
# The rust image already has libssl-dev and pkg-config, which bundled libpq needs (see Cargo.toml)
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
# Built into the binary for MAINTENANCE_MODE_OFFLINE
COPY static/maintenance.html static/maintenance.html
# Cache the registry and target dir between builds; copy the binary out of the cache mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/nyaa-actix /nyaa-actix

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin nyaa \
 && mkdir -p /data/torrents /data/avatars \
 && chown -R nyaa:nyaa /data
WORKDIR /app
# Templates and static files are read from the working directory at runtime
COPY templates templates
COPY static static
COPY --from=build /nyaa-actix /usr/local/bin/nyaa-actix

ENV TORRENT_STORAGE_PATH=/data/torrents \
    AVATAR_STORAGE_PATH=/data/avatars \
    DATABASE_URL=/data/nyaa.db
VOLUME /data
USER nyaa
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS -o /dev/null http://127.0.0.1:8080/rules || exit 1
ENTRYPOINT ["nyaa-actix"]
