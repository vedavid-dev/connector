FROM rust:1.97-bookworm AS build

RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/vedavid-connector /usr/local/bin/vedavid-connector
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/vedavid-connector"]
