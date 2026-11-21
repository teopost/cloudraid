# cloudraid in un'immagine sola: un binario, nessun programma esterno.
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

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/cloudraid /usr/local/bin/cloudraid
ENV CLOUDRAID_CONFIG=/config/cloudraid.toml
EXPOSE 8080
ENTRYPOINT ["cloudraid"]
CMD ["--help"]
