FROM rust:1-slim-trixie AS build
WORKDIR /app
ENV SQLX_OFFLINE=true
COPY . .
RUN cargo build --release --locked -p dual-rail-api

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/dual-rail-api /usr/local/bin/dual-rail-api
USER nobody
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
CMD ["dual-rail-api"]
