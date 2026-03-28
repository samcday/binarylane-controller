FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake musl-dev protobuf-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY xtask/ xtask/
COPY proto/ proto/
COPY src/ src/
RUN cargo build --release -p binarylane-controller

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/binarylane-controller /binarylane-controller
ENTRYPOINT ["/binarylane-controller"]
