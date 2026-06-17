FROM rust:1.87-alpine AS build

RUN apk add --no-cache musl-dev cmake make perl

WORKDIR /build
COPY . /build

RUN cargo build --release

FROM scratch

WORKDIR /usr/src/githttp-fs

COPY --from=build /build/target/release/githttp-fs /usr/local/bin/githttp-fs
COPY --from=build /build/config.toml /etc/githttp-fs.toml

CMD [ "githttp-fs", "-c", "/etc/githttp-fs.toml" ]

EXPOSE 5355
