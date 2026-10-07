# Owner: deploy-engine
# The gateway role: one process, `mantisd gateway`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets (its mutual-TLS identity and the
# client-facing chain) arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime-debian:dev
FROM ${RUNTIME}
# 7400/udp: the game's front door (QUIC), the one address native clients dial.
# 7630/tcp: /live, /ready, /metrics.
EXPOSE 7400/udp 7630/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7630", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "gateway", "--config", "/etc/mantis/node.toml"]
