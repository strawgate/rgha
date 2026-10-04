# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rgha && cp target/release/rgha /rgha

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /rgha /usr/local/bin/rgha
ENV RGHA_CONFIG=/etc/rgha/rgha.toml \
    RGHA_METRICS_ADDR=0.0.0.0:9464
EXPOSE 9464
ENTRYPOINT ["/usr/local/bin/rgha"]
CMD ["run"]
