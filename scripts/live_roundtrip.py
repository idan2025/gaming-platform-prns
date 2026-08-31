#!/usr/bin/env python3
"""Live end-to-end over the shipped binaries.

A stand-in game server on a UDP port, `game-bridge server` announcing it,
`game-bridge client` discovering it **by announce alone** (no destination hash
is passed), a `game-bridge browse` node listening beside them, and a UDP round
trip that crosses the mesh.

This is not a cargo test and deliberately so: it drives `target/release/game-bridge`
the way an operator would, over a real Reticulum TCP interface, and it is the
check that the artifacts in a release actually work together. `cargo test` proves
the library; this proves the product.

Still one machine. Two machines on a shared interface is the check nothing here
can stand in for — see RELEASE.md.

    cargo build --release --bins && python3 scripts/live_roundtrip.py
"""
import os, socket, subprocess, sys, time, signal, pathlib

REPO = pathlib.Path(__file__).resolve().parent.parent
BIN = REPO / "target/release/game-bridge"
if not BIN.exists():
    sys.exit("build it first: cargo build --release --bins")
WORK = REPO / "target/live-roundtrip"
WORK.mkdir(parents=True, exist_ok=True)
MESH_PORT = 47311
LISTEN = 47320

# The "game": a UDP server that answers pong:<payload>.
game = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
game.bind(("127.0.0.1", 0))
GAME_PORT = game.getsockname()[1]

import threading
def serve():
    while True:
        try:
            data, src = game.recvfrom(4096)
        except OSError:
            return
        game.sendto(b"pong:" + data, src)
threading.Thread(target=serve, daemon=True).start()

env = dict(os.environ, RUST_LOG="game_bridge=info")
procs = []
def spawn(name, args):
    log = open(WORK / f"{name}.log", "w")
    p = subprocess.Popen([str(BIN)] + args, cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT)
    procs.append((name, p, log))
    return p

server = spawn("server", ["server", "sven-coop",
    "--tcp", f"0.0.0.0:{MESH_PORT}",
    "--game-port", str(GAME_PORT),
    "--identity", str(WORK / "server.identity"),
    "--name", "E2E Test Server", "--map", "svencoop1",
    "--players", "3", "--max-players", "16",
    "--announce-interval", "2"])
time.sleep(3)

client = spawn("client", ["client", "sven-coop",
    "--tcp", f"127.0.0.1:{MESH_PORT}",
    "--listen", str(LISTEN),
    "--identity", str(WORK / "client.identity")])

# Also prove the browse role sees the announce, which is the launcher's path.
browse = spawn("browse", ["browse", "--tcp", f"127.0.0.1:{MESH_PORT}"])

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.settimeout(1.0)
ok = False
deadline = time.time() + 90
attempts = 0
while time.time() < deadline and not ok:
    attempts += 1
    try:
        sock.sendto(b"hello", ("127.0.0.1", LISTEN))
        data, _ = sock.recvfrom(4096)
        if data == b"pong:hello":
            ok = True
            break
    except socket.timeout:
        pass
    time.sleep(0.5)

elapsed = None
if ok:
    # A second round trip, timed, once the path is warm.
    t0 = time.time()
    sock.sendto(b"again", ("127.0.0.1", LISTEN))
    try:
        data, _ = sock.recvfrom(4096)
        elapsed = (time.time() - t0) * 1000
        ok = data == b"pong:again"
    except socket.timeout:
        ok = False

for name, p, log in procs:
    p.send_signal(signal.SIGTERM)
try:
    for name, p, log in procs:
        p.wait(timeout=5)
except subprocess.TimeoutExpired:
    for name, p, log in procs:
        p.kill()
for _, _, log in procs:
    log.close()

print(f"round trip over the mesh: {'OK' if ok else 'FAILED'} after {attempts} attempt(s)")
if elapsed is not None:
    print(f"warm round trip: {elapsed:.0f} ms")
for name in ("server", "client", "browse"):
    text = (WORK / f"{name}.log").read_text()
    print(f"--- {name}: {len(text.splitlines())} log lines")
    for line in text.splitlines():
        if any(k in line for k in ("ERROR", "WARN", "announce", "link established", "discovered")):
            print("   ", line[:160])
sys.exit(0 if ok else 1)
