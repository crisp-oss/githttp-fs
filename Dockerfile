FROM rust:1.87-alpine AS build

RUN apk add --no-cache musl-dev cmake make perl

WORKDIR /build
COPY . /build

RUN cargo build --release

FROM scratch

WORKDIR /usr/src/flavio

COPY --from=build /build/target/release/flavio /usr/local/bin/flavio
COPY --from=build /build/config.toml /etc/flavio.toml

CMD [ "flavio", "-c", "/etc/flavio.toml" ]

EXPOSE 5355
