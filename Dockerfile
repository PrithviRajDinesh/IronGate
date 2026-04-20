FROM rust:latest AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM rust:latest
WORKDIR /app
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/tachyon .
COPY config.toml .
COPY localhost.pem .
COPY localhost-key.pem .
EXPOSE 8080
CMD ["./tachyon", "--cert", "localhost.pem", "--key", "localhost-key.pem", "--addr", "0.0.0.0:8080"]
