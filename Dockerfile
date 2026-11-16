# The amel command-line tool, without the desktop app or browser.
#
#   docker build -t amel .
#   docker run --rm -v "$PWD/copies:/copies" amel copy https://example.com -o /copies

FROM rust:1.99.1-bookworm AS build
# wreq builds BoringSSL, which needs cmake and libclang.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mc-cli

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/amel /usr/local/bin/amel
WORKDIR /copies
ENTRYPOINT ["amel"]
CMD ["--help"]
