# A **bare** GoldSrc dedicated server (HLDS): run the DS against content the
# node mounts, and nothing else. Sibling of images/sven-coop, and the same
# shape — the differences are the binary (`hlds_linux`, not `svends_run`) and
# the mod directory, which one image serves several packs from.
#
# steamcmd app 90 installs the Half-Life Dedicated Server once; which mod that
# install runs — `valve`, `cstrike`, `dod`, `tfc` — is a start argument, so it
# is the node operator's config and not a pack field. This image reads it from
# `HLDS_MOD`, which an operator sets in the game's `[games.<id>].env`. The
# agent refuses to let operator env override anything named `GPP_*`
# (crates/platform-agent/src/docker.rs:368), so the knob deliberately is not.
#
# This image carries no game files. The node bind-mounts the content it
# installed with steamcmd, read-only, and gives each instance writable space
# inside it. One copy serves every instance of every GoldSrc mod on the host.
#
#   docker build -t gpp/goldsrc:1 images/goldsrc
#
# In the agent's config:
#
#   [games.counter-strike-16]
#   image = "gpp/goldsrc:1"
#   content_root = "/game"
#   content_version = "app90"
#   env = { HLDS_MOD = "cstrike" }

FROM debian:bookworm-slim

# `hlds_linux` is a 32-bit x86 binary, so a 64-bit-only image cannot run it at
# all — the failure is `no such file or directory` on a file that plainly
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
RUN useradd --system --uid 1000 --create-home hlds || true

# `HOME` is not cosmetic here. HLDS looks for `$HOME/.steam/sdk32/steamclient.so`
# at start and shuts down with `FATAL ERROR (shutting down): Unable to
# initialize Steam` when it is missing, and Docker gives a numeric `USER` a
# `HOME` of `/`, which nothing may write to. The entrypoint makes the link;
# this says where.
ENV HOME=/home/hlds
USER 1000

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
