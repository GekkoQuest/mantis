# Owner: deploy-engine
# The account role: one process, `mantisd account`, built from the shared base
# (base.Containerfile). Its configuration is mounted at /etc/mantis/node.toml, the signed
# registry and the deploy public key beside it; secrets arrive as files under /run/secrets.
ARG RUNTIME=mantis/runtime-debian:dev
FROM ${RUNTIME}
# 7501: RPC between roles (the services network only). 7601: /live, /ready, /metrics.
EXPOSE 7501/tcp 7601/tcp
HEALTHCHECK --interval=5s --timeout=4s --start-period=120s --retries=3 \
    CMD ["mantisd", "probe", "127.0.0.1:7601", "/ready"]
# Exec form: mantisd is PID 1, so `docker stop` (SIGTERM) starts its drain.
ENTRYPOINT ["mantisd", "account", "--config", "/etc/mantis/node.toml"]
