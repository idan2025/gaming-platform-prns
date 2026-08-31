# The headless roles — game-bridge, platform-agent, platform-index — in one
# image. Not the launcher: that is a desktop GUI, and §13.1 launch profiles
# start the player's own game on their own machine, which a container cannot do.
# See PLAN.md §13.5 for what a containerised web UI can and cannot be.
#
# Built for linux/amd64 and linux/arm64, because Reticulum's natural home
# includes single-board machines.

FROM rust:1-bookworm AS build
WORKDIR /src

# git, because the engine is a pinned git dependency rather than a crates.io
# one (ENGINE.md). No long-path workaround needed here: this is not Windows.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY . .

# `--bins` and not the launcher: `launcher/src-tauri` is excluded from the
# workspace, so this never pulls a webview toolchain into the image.
ENV CARGO_INCREMENTAL=0
RUN cargo build --release --bins

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/game-bridge /usr/local/bin/
COPY --from=build /src/target/release/platform-agent /usr/local/bin/
COPY --from=build /src/target/release/platform-index /usr/local/bin/
COPY packs /opt/gaming-platform-prns/packs

# Not root. The bridge binds a game port and a Reticulum interface; neither
# needs privilege, and the agent talks to a Docker socket the operator mounts
# deliberately rather than to one it assumes.
RUN useradd --system --create-home --uid 10001 mesh
USER mesh
WORKDIR /home/mesh

# No ENTRYPOINT of its own beyond the binary: which role this container plays is
# the operator's decision, exactly as which image a game runs in is. An image
# that picked a role would be choosing from the wrong side of the machine.
ENTRYPOINT ["game-bridge"]
CMD ["--help"]
