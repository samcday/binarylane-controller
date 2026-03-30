FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake musl-dev protobuf-dev

WORKDIR /src

# Copy manifests, build script, and proto definitions first for layer caching
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
COPY binarylane-client/ binarylane-client/

# Strip xtask and integration-tests from workspace — dev-only, not shipped
RUN sed -i '/"xtask"/d; /"integration-tests"/d' Cargo.toml

# Create dummy source so cargo can fetch + compile all dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/.fingerprint/binarylane-controller-*

# Now copy real sources — only our crate recompiles
COPY src/ src/
RUN cargo build --release

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/binarylane-controller /binarylane-controller
ENTRYPOINT ["/binarylane-controller"]
