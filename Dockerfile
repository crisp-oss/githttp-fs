FROM rust:1.87-alpine AS build

RUN apk add --no-cache musl-dev cmake make perl

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM alpine:3.22

WORKDIR /app

COPY --from=build /build/target/release/flavio ./flavio
COPY config.toml .

EXPOSE 5355

ENTRYPOINT ["./flavio"]
