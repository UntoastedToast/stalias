FROM rust:1-alpine AS build
RUN apk add --no-cache build-base cmake perl
WORKDIR /src

# Build the dependencies first so source changes don't recompile them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release && rm -rf src

COPY . .
RUN touch src/main.rs src/lib.rs && cargo build --release \
    && mkdir /data-dir

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/stalias /stalias
COPY --from=build --chown=nonroot:nonroot /data-dir /data
ENV PORT=8080 DATA_DIR=/data
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/stalias", "healthcheck"]
ENTRYPOINT ["/stalias"]
