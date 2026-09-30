# A **bare** Source dedicated server (srcds): run the DS against content the
# node mounts, and nothing else. Sibling of images/goldsrc, same shape.
#
# This image carries no game files. The node bind-mounts the content it
# installed with steamcmd (Team Fortress 2 is app 232250), read-only, and gives
# each instance writable space inside it. Which game directory runs is
# `SRCDS_GAME`, the operator's config, set in the game's `[games.<id>].env`.
#
#   docker build -t gpp/source:1 images/source
#
# In the agent's config:
#
#   [games.team-fortress-2]
#   image = "gpp/source:1"
#   content_root = "/game"
#   content_version = "app232250"
#   env = { SRCDS_GAME = "tf" }

FROM debian:bookworm-slim

# `srcds_linux` is 32-bit x86, so the i386 C and C++ runtimes are needed.
#
# `libcurl3-gnutls` is not optional, and a missing one is hard to read: srcds
# gets as far as `server_srv.so loaded for "Team Fortress"`, then dies on
# `FATAL ERROR: Could not load: replay_srv.so` — a file that is plainly there,
# failing on its own dependency `libcurl-gnutls.so.4`.
#
# `bsdutils` carries `script`, which gives srcds the terminal its console
# needs (see entrypoint.sh); it is in the base image already, and named here
# so that stays true.
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      libc6:i386 libstdc++6:i386 libgcc-s1:i386 libcurl3-gnutls:i386 \
      bsdutils ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Not root. The uid matches the agent's default so the writable directories the
# node creates are writable here.
RUN useradd --system --uid 1000 --create-home srcds || true

# srcds looks for `$HOME/.steam/sdk32/steamclient.so`, and Docker gives a
# numeric `USER` a `HOME` of `/`, which nothing may write to.
ENV HOME=/home/srcds
USER 1000

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
