# Runtime image for the `server` on AWS Lightsail Container Service.
#
# Lightsail runs amd64 (x86_64) containers only, so the binary is built natively
# on the pinned ubuntu-24.04 GitHub runner and copied in — we don't compile
# inside Docker, so the deploy workflow's cargo target/ cache stays effective.
# The runtime base is pinned to ubuntu:24.04 to match the runner's glibc; bumping
# the runner to a newer ubuntu would require bumping this base too.
#
# sqlx::migrate!("./migrations") embeds the migrations at compile time, so no
# migration files are needed in this image. All TLS (Postgres + S3) goes through
# rustls with webpki-roots baked into the binary, so no system CA store or
# OpenSSL is required at runtime.

FROM ubuntu:24.04

# ca-certificates is harmless to include and keeps TLS root stores current for
# any future client that falls back to the system store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The prebuilt binary is the only file in the build context (the deploy stages
# just this file next to the Dockerfile so `docker build` never uploads the rest
# of the workspace).
COPY server /usr/local/bin/server

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

CMD ["server"]
