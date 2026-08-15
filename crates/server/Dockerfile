# Runtime image for the `server` on AWS Lightsail Container Service.
#
# Lightsail runs amd64 (x86_64) containers only, so the binary is built natively
# on the pinned ubuntu-24.04 GitHub runner and copied in — we don't compile
# inside Docker, so the deploy workflow's cargo target/ cache stays effective.
# The runtime base is pinned to ubuntu:24.04 to match the runner's glibc; bumping
# the runner to a newer ubuntu would require bumping this base too.
#
# sqlx::migrate!("./migrations") embeds the migrations at compile time, so no
# migration files are needed in this image. Postgres (sqlx) and S3 (rust-s3) TLS
# goes through rustls with webpki-roots baked into the binary, so those need no
# system CA store. The app's own outbound reqwest 0.13 client (OpenRouter calls,
# built in main.rs) uses reqwest's `rustls` feature, which routes to
# rustls-platform-verifier -> rustls-native-certs, i.e. it reads the OS CA store
# rather than webpki-roots. reqwest 0.13 has no webpki-roots feature, so we must
# install ca-certificates here — otherwise client build fails at startup with
# "No CA certificates were loaded from the system".

FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The prebuilt binary is the only file in the build context (the deploy stage
# copies just this file next to the Dockerfile so `docker build` never uploads
# the rest of the workspace).
COPY server /usr/local/bin/server

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

CMD ["server"]
