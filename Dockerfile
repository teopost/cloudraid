# cloudraid + rclone in un'immagine sola.
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
# le pagine dell'interfaccia web finiscono dentro il binario (include_str!)
COPY web web
# cache di BuildKit per crate e compilazione: le ricostruzioni sono veloci
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/cloudraid /usr/local/bin/cloudraid

# rclone ufficiale, binario statico: molto più recente di quello dei pacchetti Debian
FROM rclone/rclone:1.71.2 AS rclone

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rclone /usr/local/bin/rclone /usr/local/bin/rclone
COPY --from=builder /usr/local/bin/cloudraid /usr/local/bin/cloudraid
ENV CLOUDRAID_CONFIG=/config/cloudraid.toml \
    RCLONE_CONFIG=/config/rclone.conf
EXPOSE 8080
ENTRYPOINT ["cloudraid"]
CMD ["--help"]
