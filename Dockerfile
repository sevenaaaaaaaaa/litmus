# ── build ──
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p litmus
# ── runtime ──
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/litmus /usr/local/bin/litmus
VOLUME /data
ENV LITMUS_LISTEN=0.0.0.0:7700
EXPOSE 7700
ENTRYPOINT ["litmus"]
CMD ["serve", "/data/kb", "--addr", "0.0.0.0:7700"]
