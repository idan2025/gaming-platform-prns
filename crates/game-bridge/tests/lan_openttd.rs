//! Mode 3, step 3: a real game in a LAN room, end to end (`PLAN.md` §14.3).
//!
//! Everything here is the shipped product, not a stand-in:
//!
//! - two network namespaces joined by a veth pair — the only way between them,
//!   so it plays the part of the Internet;
//! - in each, the real `game-bridge lan-host` / `lan-join` binary, which runs
//!   the real `lan-helper` to make its adapter;
//! - behind the host, a real OpenTTD 15.3 dedicated server; behind the member,
//!   a real OpenTTD client with no display.
//!
//! The member finds the server the way OpenTTD's own LAN browser does — a
//! `PACKET_UDP_CLIENT_FIND_SERVER` broadcast to the subnet's broadcast address
//! (`src/network/network_udp.cpp`, `NetworkUDPBroadCast`) — and then the real
//! client joins the address that answered, downloads the map and starts a
//! company. Nothing in the room knows what OpenTTD is; the pack's `[lan]`
//! ports are all it is told.
//!
//! Needs OpenTTD: `scripts/fetch-openttd.sh <dir>` fetches the pinned build,
//! then `OPENTTD_DIR=<dir>/openttd-15.3-linux-generic-amd64`. Without it, or
//! without unprivileged user namespaces, the test says so and skips.

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Fixed ports are safe here: both namespaces are private to this test.
#[allow(dead_code)]
mod common;

const INSIDE: &str = "GAME_BRIDGE_LAN_OPENTTD_TEST_INSIDE";
const OPENTTD_PORT: u16 = 3979;
const MESH_PORT: u16 = 4242;

#[test]
fn a_real_openttd_client_finds_and_joins_a_real_server_through_a_lan_room() {
    if std::env::var_os(INSIDE).is_some() {
        return;
    }
    let Some(openttd) = std::env::var_os("OPENTTD_DIR") else {
        eprintln!("skipping: set OPENTTD_DIR (scripts/fetch-openttd.sh fetches the pinned build)");
        return;
    };
    if !Path::new(&openttd).join("openttd").exists() {
        panic!("OPENTTD_DIR={openttd:?} holds no openttd binary");
    }
    let probe = Command::new("unshare").args(["-rn", "true"]).output();
    if !probe.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: unprivileged user namespaces are not available here ({probe:?})");
        return;
    }
    let out = Command::new("unshare")
        .arg("-rn")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inside_a_user_namespace",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INSIDE, "1")
        .output()
        .expect("unshare runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The measurements are printed by the inner run; pass them on.
    eprintln!("{stderr}");
    assert!(
        out.status.success(),
        "the inner run failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(stdout.contains("1 passed"), "the inner run did not run\n{stdout}");
}

/// A thread living in its own network namespace. Sockets it opens and
/// processes it starts belong to that namespace, whichever thread uses them
/// afterwards.
struct Namespace {
    jobs: mpsc::Sender<Box<dyn FnOnce() + Send>>,
    tid: i32,
}

impl Namespace {
    fn new() -> Self {
        let (jobs, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let (tid_tx, tid_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: moves only this thread into a new network namespace.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNET) }, 0);
            // SAFETY: gettid cannot fail.
            tid_tx.send(unsafe { libc::gettid() }).unwrap();
            for job in rx {
                job();
            }
        });
        let ns = Self { jobs, tid: tid_rx.recv().unwrap() };
        ns.sh("ip link set lo up");
        ns
    }

    fn run<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        self.jobs.send(Box::new(move || tx.send(f()).unwrap())).unwrap();
        rx.recv().unwrap()
    }

    fn sh(&self, script: &str) -> String {
        let script = script.to_string();
        let out = self.run(move || Command::new("sh").arg("-c").arg(&script).output().unwrap());
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn spawn(&self, mut cmd: Command) -> Child {
        self.run(move || cmd.spawn().expect("the process starts"))
    }

    /// The address on this namespace's room adapter, once it has one.
    fn adapter_address(&self) -> Option<Ipv4Addr> {
        let out = self.run(|| {
            Command::new("ip").args(["-4", "-o", "addr", "show", "gbl0"]).output().unwrap()
        });
        let text = String::from_utf8_lossy(&out.stdout);
        let cidr = text.split_whitespace().skip_while(|w| *w != "inet").nth(1)?;
        cidr.split('/').next()?.parse().ok()
    }
}

fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn logged(cmd: &mut Command, log: &Path) {
    let file = std::fs::File::create(log).unwrap();
    cmd.stdout(file.try_clone().unwrap()).stderr(file).stdin(Stdio::null());
}

fn packs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs")
}

struct Killed(Vec<Child>);

impl Drop for Killed {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
#[ignore = "runs inside `unshare -rn`, started by a_real_openttd_client_finds_and_joins_a_real_server_through_a_lan_room"]
fn inside_a_user_namespace() {
    if std::env::var_os(INSIDE).is_none() {
        return;
    }
    let openttd = PathBuf::from(std::env::var_os("OPENTTD_DIR").unwrap()).join("openttd");
    let dir = common::scratch_dir("lan-openttd");
    let bridge = env!("CARGO_BIN_EXE_game-bridge");
    let mut children = Killed(Vec::new());

    // The "Internet": one veth pair between two namespaces.
    let host = Namespace::new();
    let member = Namespace::new();
    host.sh(&format!("ip link add wan type veth peer name wan netns {}", member.tid));
    host.sh("ip addr add 10.77.0.1/24 dev wan && ip link set wan up");
    member.sh("ip addr add 10.77.0.2/24 dev wan && ip link set wan up");

    // The room's host, with a real OpenTTD server behind it.
    let mut cmd = Command::new(bridge);
    cmd.args(["lan-host", "openttd", "--name", "OpenTTD night"])
        .args(["--tcp", &format!("0.0.0.0:{MESH_PORT}")])
        .arg("--packs")
        .arg(packs_dir())
        .arg("--identity")
        .arg(dir.join("host.identity"));
    logged(&mut cmd, &dir.join("host-bridge.log"));
    children.0.push(host.spawn(cmd));

    std::fs::write(
        dir.join("server.cfg"),
        "[network]\nserver_name = lan-room-test\nserver_game_type = local\n",
    )
    .unwrap();
    let mut cmd = Command::new(&openttd);
    cmd.args(["-D", &format!("0.0.0.0:{OPENTTD_PORT}"), "-x", "-X", "-d", "net=3", "-c"])
        .arg(dir.join("server.cfg"))
        .env("HOME", &dir)
        .current_dir(&dir);
    logged(&mut cmd, &dir.join("server.log"));
    children.0.push(host.spawn(cmd));

    // The member, finding the room by its announce alone.
    let mut cmd = Command::new(bridge);
    cmd.args(["lan-join", "openttd", "--tcp", &format!("10.77.0.1:{MESH_PORT}")])
        .arg("--packs")
        .arg(packs_dir())
        .arg("--identity")
        .arg(dir.join("member.identity"));
    logged(&mut cmd, &dir.join("member-bridge.log"));
    let started = Instant::now();
    children.0.push(member.spawn(cmd));

    let logs = || {
        ["host-bridge.log", "member-bridge.log", "server.log", "client.log"]
            .iter()
            .map(|f| {
                format!("--- {f}\n{}", std::fs::read_to_string(dir.join(f)).unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let h_addr = wait_for("the host's adapter", Duration::from_secs(30), || host.adapter_address());
    let m_addr =
        wait_for("the member's adapter", Duration::from_secs(60), || member.adapter_address());
    let seated_after = started.elapsed();
    assert_ne!(h_addr, m_addr);

    // OpenTTD's LAN browser: PACKET_UDP_CLIENT_FIND_SERVER, which is a bare
    // header — two bytes of little-endian size (3) and the type (0) — sent to
    // the subnet broadcast. The answer is PACKET_UDP_SERVER_RESPONSE, type 1.
    let probe: UdpSocket = member.run(|| {
        let s = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        s.set_broadcast(true).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        s
    });
    let broadcast =
        SocketAddr::from((Ipv4Addr::from(u32::from(m_addr) | 0x0000_ffff), OPENTTD_PORT));
    let mut buf = [0u8; 1500];
    let (found_at, discovery_rtt) =
        wait_for("OpenTTD answers a LAN search", Duration::from_secs(60), || {
            let sent = Instant::now();
            probe.send_to(&[3, 0, 0], broadcast).ok()?;
            loop {
                let (n, from) = probe.recv_from(&mut buf).ok()?;
                if from.ip() != std::net::IpAddr::V4(m_addr) && buf[..n] == [3, 0, 1] {
                    return Some((from, sent.elapsed()));
                }
            }
        });
    assert_eq!(
        found_at,
        SocketAddr::from((h_addr, OPENTTD_PORT)),
        "the answer came from the host's room address\n{}",
        logs()
    );

    // The real client joins what the search found.
    std::fs::write(dir.join("client.cfg"), "[network]\nclient_name = room-member\n").unwrap();
    let mut cmd = Command::new(&openttd);
    cmd.args([
        "-v",
        "null:ticks=1000000000",
        "-s",
        "null",
        "-m",
        "null",
        "-x",
        "-X",
        "-d",
        "net=3",
    ])
    .args(["-n", &found_at.to_string(), "-c"])
    .arg(dir.join("client.cfg"))
    .env("HOME", &dir)
    .current_dir(&dir);
    logged(&mut cmd, &dir.join("client.log"));
    let join_started = Instant::now();
    children.0.push(member.spawn(cmd));

    let server_log = || std::fs::read_to_string(dir.join("server.log")).unwrap_or_default();
    wait_for("the client joins the game", Duration::from_secs(90), || {
        server_log().contains("room-member has joined the game").then_some(())
    });
    let joined_after = join_started.elapsed();
    let from_room = format!("Client connected from {m_addr}");
    assert!(
        server_log().contains(&from_room),
        "the server saw the member's room address\n{}",
        logs()
    );
    wait_for("the client starts a company", Duration::from_secs(60), || {
        server_log().contains("room-member has started a new company").then_some(())
    });

    eprintln!(
        "lan_openttd measurements (loopback veth, one hop):\n  \
         member seated and adapter up: {seated_after:?} after lan-join started\n  \
         LAN search answered in:       {discovery_rtt:?}\n  \
         client joined (map included): {joined_after:?} after it started"
    );

    // Leaving takes the adapter with it.
    let member_bridge = children.0.remove(2);
    // SAFETY: signalling a child we started.
    unsafe { libc::kill(member_bridge.id() as i32, libc::SIGINT) };
    let mut member_bridge = Killed(vec![member_bridge]);
    let status = member_bridge.0[0].wait().unwrap();
    assert!(status.success(), "lan-join exited {status} on Ctrl-C\n{}", logs());
    assert!(member.adapter_address().is_none(), "lan-join left its adapter behind");
}
