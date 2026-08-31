# A **bare** Sven Co-op dedicated server: run the DS against content the node
# mounts, and nothing else.
#
# Deliberately not `ghcr.io/idan2025/svencoop-prns`. That image is the whole
# standalone product — its own controller, its own web GUI on 8080, its own
# Reticulum bridge — and it manages a DS lifecycle of its own. Started by this
# agent it comes up, listens on 8080, and waits for somebody to press start in
# *its* interface. Nothing plays. It is a fine product and the wrong shape for a
# node that already decides what runs.
#
# This image carries no game files. The node bind-mounts the content it
# installed with steamcmd, read-only, and gives each instance writable space
# inside it. One 2.7 GB copy serves every instance on the host.
#
#   docker build -t gpp/sven-coop:1 images/sven-coop
#
# In the agent's config:
#
#   [games.sven-coop]
#   image = "gpp/sven-coop:1"
#   content_root = "/game"
#   content_version = "5.26"

FROM debian:bookworm-slim

# The GoldSrc dedicated server is a 32-bit x86 binary (`svends_i686`: "ELF
# 32-bit LSB pie executable, Intel 80386"), so a 64-bit-only image cannot run it
# at all — the failure is `no such file or directory` on a file that plainly
# exists, which is what a missing ELF interpreter looks like.
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      libc6:i386 libstdc++6:i386 libgcc-s1:i386 \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Not root. The server binds a high UDP port and writes only into the writable
# binds the node nested inside the read-only content mount.
#
# The uid matches the agent's default so the writable directories the node
# creates are writable here. An operator running the agent as a different user
# passes `--user` through their own config.
RUN useradd --system --uid 1000 --create-home sven || true
USER 1000

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
