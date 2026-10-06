# Owner: deploy-engine
# The one multi-stage base every role image is built from (deploy/containers/*.Containerfile).
#
#   build    the Rust builder: compiles mantisd (every service role) and the toy package's
#            server binary (its cell-host role) in release, with the workspace's lock file.
#   runtime  a minimal Debian runtime: mantisd only, an unprivileged user, the state and
#            output directories it may write. No compiler, no source, no cargo.
#
# Base images are pinned by digest so a rebuild is reproducible. Build from the workspace
# root (scripts/deploy-local.ps1 does):
#   docker build -f deploy/containers/base.Containerfile --target build   -t mantis/build:dev .
#   docker build -f deploy/containers/base.Containerfile --target runtime -t mantis/runtime:dev .
# The build context is filtered by base.Containerfile.dockerignore (no target directories).

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /src
COPY . .
# Cache mounts keep the registry and the target directory between builds; the binaries are
# copied out in the same step because a cache mount is not part of the image.
RUN --mount=type=cache,id=mantis-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=mantis-target,target=/src/target \
    cargo build --release --locked -p mantis-deploy --bin mantisd -p toy-server --bin toy-server \
    && mkdir -p /out \
    && cp target/release/mantisd target/release/toy-server /out/

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime
# The node writes only under /var/lib/mantis: state (snapshots, logs) and out (certificates
# for clients). Created here so named volumes mounted there inherit the owner.
RUN useradd --system --uid 10001 --user-group --home-dir /var/lib/mantis --create-home mantis \
    && mkdir -p /var/lib/mantis/state /var/lib/mantis/out /etc/mantis \
    && chown -R mantis:mantis /var/lib/mantis
COPY --from=build /out/mantisd /usr/local/bin/mantisd
USER mantis
WORKDIR /var/lib/mantis
