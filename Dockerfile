FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

ARG TARGETARCH

LABEL org.opencontainers.image.title="auditstream" \
      org.opencontainers.image.description="PostgreSQL audit trail engine CLI" \
      org.opencontainers.image.url="https://auditstream.dev" \
      org.opencontainers.image.source="https://github.com/auditstream/auditstream" \
      org.opencontainers.image.licenses="MIT"

COPY --chmod=0555 dist-bin/${TARGETARCH}/auditstream /usr/local/bin/auditstream

USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/auditstream"]
