# Ondura Server container image.
#   docker build -t ondura-server .
#   docker run -d --name ondura -p 25:25 -p 465:465 -p 587:587 -p 443:443 -p 80:80 \
#     -v ondura-data:/var/lib/ondura-server -v /etc/ondura-server:/etc/ondura-server ondura-server
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ondura-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /var/lib/ondura-server ondura \
 && install -d -o ondura -g ondura /var/lib/ondura-server
COPY --from=build /src/target/release/ondura-server /usr/bin/ondura-server
USER ondura
VOLUME ["/var/lib/ondura-server"]
EXPOSE 25 80 443 465 587
ENTRYPOINT ["/usr/bin/ondura-server"]
CMD ["--config", "/etc/ondura-server/config.toml", "serve"]
