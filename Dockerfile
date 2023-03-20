FROM rust:1.68 AS builder
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim
COPY --from=builder ./target/release/postrs ./target/release/postrs

EXPOSE 4000
CMD ["/target/release/postrs", "serve", "--bind", "0.0.0.0:4000"]