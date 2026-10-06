# Owner: deploy-engine
# The social role: one process, `mantisd social`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime:dev
FROM ${RUNTIME}
# 7503: RPC between roles (the services network only). 7603: /live, /ready, /metrics.
EXPOSE 7503/tcp 7603/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7603", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "social", "--config", "/etc/mantis/node.toml"]
