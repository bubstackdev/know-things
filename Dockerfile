FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS build
WORKDIR /build
ARG KB_BUILD_VERSION=
ENV KNOW_THINGS_BUILD_VERSION=${KB_BUILD_VERSION}
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=know-things-target-arm64,target=/build/target,sharing=locked \
    cargo clean --release -p know-things \
    && cargo build --locked --release --jobs 2 \
    && install -D -m 0755 target/release/know-things /out/know-things

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
RUN install -d -o 65532 -g 65532 -m 0700 /data /run/know-things \
    && install -o 65532 -g 65532 -m 0600 /dev/null /data/.keep
COPY --from=build /out/know-things /usr/local/bin/know-things
ENV TMPDIR=/run/know-things
ENV SQLITE_TMPDIR=/data
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/know-things"]
CMD ["serve"]
