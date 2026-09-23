FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p kuberic-controller --bin kuberic-controller

FROM debian:bookworm-slim
RUN useradd --create-home --uid 65532 controller
COPY --from=build /src/target/release/kuberic-controller /usr/local/bin/kuberic-controller
USER 65532
ENTRYPOINT ["/usr/local/bin/kuberic-controller"]
