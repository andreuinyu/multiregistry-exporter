ARG VERSION=dev

# Build stage
FROM rust:1.100.0-alpine3.24 AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

# Final stage
FROM scratch

ARG VERSION
ENV APP_VERSION=${VERSION} \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
LABEL org.opencontainers.image.version=${VERSION}

COPY --from=builder /build/target/release/multiregistry-exporter /multiregistry-exporter
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

USER 65532:65532
EXPOSE 9101

HEALTHCHECK --interval=60s --timeout=5s --start-period=10s --retries=3 CMD ["/multiregistry-exporter", "healthcheck"]

ENTRYPOINT ["/multiregistry-exporter"]
