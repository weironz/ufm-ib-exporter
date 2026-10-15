# syntax=docker/dockerfile:1
FROM rust:1.88-slim-bookworm@sha256:38bc5a86d998772d4aec2348656ed21438d20fcdce2795b56ca434cf21430d89 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    --mount=type=secret,id=proxy_ca \
    if [ -s /run/secrets/proxy_ca ]; then export CARGO_HTTP_CAINFO=/run/secrets/proxy_ca; fi; \
    cargo build --locked --release && cp target/release/ufm-ib-exporter /build/exporter

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /build/exporter /usr/local/bin/ufm-ib-exporter
COPY config.example.yaml /etc/ufm-ib-exporter/config.example.yaml
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
ENV UFM_CONFIG=/etc/ufm-ib-exporter/config.example.yaml
USER 65532:65532
EXPOSE 9627
ENTRYPOINT ["/usr/local/bin/ufm-ib-exporter"]
