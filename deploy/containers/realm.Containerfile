# Owner: deploy-engine
# The realm role: one process, `mantisd realm`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime:dev
FROM ${RUNTIME}
# 7502: RPC between roles (the services network only). 7602: /live, /ready, /metrics.
EXPOSE 7502/tcp 7602/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7602", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "realm", "--config", "/etc/mantis/node.toml"]
