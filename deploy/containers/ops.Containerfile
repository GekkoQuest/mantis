# Owner: deploy-engine
# The ops role: one process, `mantisd ops`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime-debian:dev
FROM ${RUNTIME}
# 7506: RPC between roles (the services network only). 7606: /live, /ready, /metrics.
# 7480: the HTTPS dashboard, on its own listener and its own network, never a game port.
EXPOSE 7506/tcp 7606/tcp 7480/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7606", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "ops", "--config", "/etc/mantis/node.toml"]
