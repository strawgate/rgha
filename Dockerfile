# syntax=docker/dockerfile:1
# Pinned so a rebuild can't silently pick up an older CA bundle; bump the
# digest to take base image updates.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime

FROM busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS certs
COPY --from=runtime /etc/ssl/certs/ca-certificates.crt /bundle.crt
COPY certs/ /certs/
RUN sh /certs/distrust.sh /bundle.crt /certs/distrusted/*.pem > /ca-certificates.crt

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rgha && cp target/release/rgha /rgha

FROM runtime
COPY --from=certs /ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /rgha /usr/local/bin/rgha
ENV RGHA_CONFIG=/etc/rgha/rgha.toml \
    RGHA_METRICS_ADDR=0.0.0.0:9464
EXPOSE 9464
ENTRYPOINT ["/usr/local/bin/rgha"]
CMD ["run"]
