# Owner: deploy-engine
# The cell-host role. A cell host is package code (world, modules, adapters), so this image
# carries the package's server binary from the shared build stage and runs its node
# subcommand: `toy-server node cell-host` (the toy package; another package swaps the
# binary and the content mount). mantisd is in the image for the health check.
ARG RUNTIME=mantis/runtime-debian:dev
ARG BUILD=mantis/build:dev
FROM ${BUILD} AS build
FROM ${RUNTIME}
COPY --from=build /out/toy-server /usr/local/bin/toy-server
# 7400/udp: native clients (QUIC). 7401/tcp: legacy clients (TCP). Both on the game network.
# 7520/tcp: the read-only inspector, kick and drain Ops calls (services network only).
# 7620/tcp: /live, /ready, /metrics.
EXPOSE 7400/udp 7401/tcp 7520/tcp 7620/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=180s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7620", "/ready"]
ENTRYPOINT ["toy-server", "node", "cell-host", "--config", "/etc/mantis/node.toml"]
