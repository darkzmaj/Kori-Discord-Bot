# syntax=docker/dockerfile:1

######################################################################################
# Build the Rust bot
######################################################################################
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY db ./db
COPY shared ./shared
COPY vendor ./vendor
RUN cargo build --release --locked

######################################################################################
# Runtime: the Rust binary plus the Ruby operations tooling (migrate, commands:*)
######################################################################################
FROM ruby:3.4-slim-bookworm AS runtime
LABEL org.opencontainers.image.title="Kori"       org.opencontainers.image.description="Multi-server, panel-based Discord ticket and support bot"       org.opencontainers.image.source="https://github.com/darkzmaj/Kori-Discord-Bot"       org.opencontainers.image.licenses="MIT"       org.opencontainers.image.authors="DarkZmaj"       org.opencontainers.image.version="1.0.0"
WORKDIR /app
ENV BUNDLE_WITHOUT=test BUNDLE_DEPLOYMENT=1 BUNDLE_USER_HOME=/tmp/.bundle

RUN groupadd --system kori && useradd --system --gid kori --home /app kori

COPY Gemfile Gemfile.lock Rakefile ./
RUN bundle install && rm -rf /usr/local/bundle/cache
COPY ruby ./ruby
COPY db ./db
COPY shared ./shared
COPY --from=build /app/target/release/kori /usr/local/bin/kori

RUN mkdir -p /app/data && chown -R kori:kori /app

USER kori

VOLUME ["/app/data"]

# Discord bots are outbound-gateway only; no port is exposed.
CMD ["kori"]
