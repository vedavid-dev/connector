FROM rust:1.97-bookworm AS build

# prost-build stopped vendoring protoc, and the well-known .proto files it
# imports are a separate Debian package from the compiler.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release --locked

# No shell and no package manager, so the surface is the binary and the
# certificates it needs to verify the relay.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/vedavid-connector /usr/local/bin/vedavid-connector
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/vedavid-connector"]
