use std::net::TcpListener;
use std::sync::atomic::{AtomicU16, Ordering};

/// Ports below every OS's ephemeral range (Linux starts at 32768, Windows and
/// macOS at 49152), and below the 27xxx the node's Docker tests use.
const PORTS: std::ops::Range<u16> = 20000..27000;
static NEXT: AtomicU16 = AtomicU16::new(0);

/// A port that is free now, for a test to bind a moment later.
///
/// **Never from the OS's ephemeral range.** Binding port 0 hands out a port
/// from the same pool outgoing connections take their source ports from, and
/// these tests make many: in the gap between this returning and the test
/// binding it, another test's connection took it and the bind failed (CI,
/// `server_chat.rs`, "binding TCP listener"). Each call in a process also
/// takes the next number, so two tests never get the same one.
pub fn free_tcp_port() -> u16 {
    let span = PORTS.end - PORTS.start;
    let start = (std::process::id() % u32::from(span)) as u16;
    for _ in 0..span {
        let port = PORTS.start + (start + NEXT.fetch_add(1, Ordering::Relaxed) % span) % span;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() && TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in {PORTS:?}");
}

pub fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("game-bridge-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
