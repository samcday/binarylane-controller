FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev protobuf-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
COPY src/ src/
RUN cargo build --release

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/binarylane-controller /binarylane-controller
ENTRYPOINT ["/binarylane-controller"]
