# Agora: one static binary on a small image. Cloud Run sets PORT; give it
# MODEL_API_KEY (and BASETEN_API_KEY), AGORA_TOKEN and AGORA_HOSTS as secrets/env.
# The web assets are already built into crates/agora-server/static, so no Node here.
FROM docker.io/library/rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p agora-server -p agora-engine --bin agora-server --bin agora-verify && mkdir /data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build --chown=65532:65532 /data /data
WORKDIR /data
COPY --from=build /src/target/release/agora-server /src/target/release/agora-verify /usr/local/bin/
# State, ledgers and the signing key live in /data (mount a volume to keep them).
ENV AGORA_JOURNAL=/data/agora-journal.db AGORA_STATE=/data/agora-state.db AGORA_SIGNING_KEY=/data/agora-signing.key
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/agora-server"]
