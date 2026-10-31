# The network spike server. Multi-arch: `docker buildx build --platform linux/arm64,linux/amd64`
# (README.md, "Image"). The build stage runs under the target platform (QEMU for arm64 on an x86
# host); the project is small, so that takes a few minutes, not an hour.
ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && install -m 0755 target/release/cof-network-spike /cof-network-spike

FROM debian:trixie-slim
COPY --from=build /cof-network-spike /usr/local/bin/cof-network-spike
# The restricted pod security profile runs it as 65532 anyway; this keeps `docker run` the same.
USER 65532:65532
ENV SPIKE_ADDR=127.0.0.1:8080
ENTRYPOINT ["/usr/local/bin/cof-network-spike"]
