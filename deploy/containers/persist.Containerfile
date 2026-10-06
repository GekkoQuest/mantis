# Owner: deploy-engine
# The persist role: one process, `mantisd persist`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime:dev
FROM ${RUNTIME}
# 7505: RPC between roles (the services network only). 7605: /live, /ready, /metrics.
EXPOSE 7505/tcp 7605/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7605", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "persist", "--config", "/etc/mantis/node.toml"]
