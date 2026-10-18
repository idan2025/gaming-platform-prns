# A **bare** Minecraft: Java Edition server (`MINECRAFT.md` §11): run the
# build the node downloaded, against the server directory the node gives each
# instance, and nothing else.
#
# This image carries no Minecraft files. The node fetches a build (vanilla,
# Paper, Fabric or Forge) with the `minecraft` content driver, checks it
# against its publisher's digest, and bind-mounts it read-only; each instance
# gets its own writable `server/` inside it, where the world, the settings and
# the libraries a loader fetches on first start live.
#
# Three Java runtimes, because each Minecraft release asks for one: 17 for
# 1.18.1 to 1.20.4, 21 for 1.20.5 to 1.21.x, 25 for 26.x. The build's own
# description (`lanthorn-build.txt`) says which.
#
#   docker build -t gpp/minecraft:1 images/minecraft
#
# In the agent's config:
#
#   [games.minecraft-java]
#   image = "gpp/minecraft:1"
#   content_root = "/game"
#   content_version = "paper-26.3"     # the build a server gets when none is picked

FROM eclipse-temurin:17-jre AS java17
FROM eclipse-temurin:21-jre AS java21
FROM eclipse-temurin:25-jre AS java25

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=java17 /opt/java/openjdk /opt/java/17
COPY --from=java21 /opt/java/openjdk /opt/java/21
COPY --from=java25 /opt/java/openjdk /opt/java/25

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Not root. The uid matches the agent's default, so the server directory the
# node creates is writable here. `HOME` is somewhere a JVM may write.
RUN useradd --system --uid 1000 --create-home minecraft || true
ENV HOME=/home/minecraft
USER 1000

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
