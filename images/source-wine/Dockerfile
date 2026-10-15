# A **bare** Source dedicated server for games whose server exists only for
# Windows, or whose Linux one does not run: `srcds.exe` under Wine, against
# content the node mounts. Sibling of images/source, same contract.
#
# The pack only says to fetch the Windows build (`[content] platform =
# "windows"`); that this image runs it is the node operator's choice, in
# agent.toml, like every image (a pack cannot name what runs).
#
#   docker build -t gpp/source-wine:1 images/source-wine
#
#   [games.synergy]
#   image = "gpp/source-wine:1"
#   content_root = "/game"
#   content_version = "app17525"
#   env = { SRCDS_GAME = "synergy" }

FROM debian:bookworm-slim

# srcds.exe is 32-bit, so Wine's 32-bit half is the one that runs it.
# `bsdutils` carries `script`, the terminal the console needs, as in
# images/source.
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      wine wine32 libwine:i386 xvfb xauth bsdutils ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

RUN useradd --system --uid 1000 --create-home srcds || true

# Wine keeps its prefix in `$HOME`, so the user needs a writable one. The
# prefix is made at build time, once, rather than on every start.
ENV HOME=/home/srcds WINEPREFIX=/home/srcds/.wine WINEARCH=win32 WINEDEBUG=-all
USER 1000
RUN wineboot --init && wineserver --wait

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
