# A **bare** Minecraft: Bedrock Edition server (`MINECRAFT.md` §16): the
# edition phones, consoles and Windows play. Mojang's own dedicated server,
# run against the copy the node downloaded, and nothing else.
#
# This image carries no Minecraft files. The pack pins the server's zip by
# SHA-256 and the node's archive driver fetches, checks and extracts it,
# read-only; each instance gets its own writable `server/` inside it, where
# the world and the settings live and the server's other files are linked in.
#
# Linux x86-64 only: that is the only Linux build Mojang ships.
#
#   docker build -t gpp/minecraft-bedrock:1 images/minecraft-bedrock
#
# In the agent's config:
#
#   [games.minecraft-bedrock]
#   image = "gpp/minecraft-bedrock:1"
#   content_root = "/game"
#   content_version = "1.26.52.3"

FROM debian:bookworm-slim

# Certificates for Xbox Live, which checks players' accounts (online mode).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

RUN useradd --system --uid 1000 --create-home bedrock || true
ENV HOME=/home/bedrock
USER 1000

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
