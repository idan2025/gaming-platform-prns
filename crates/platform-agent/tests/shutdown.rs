//! `docker stop` must stop the agent, not wait it out.
//!
//! As PID 1 in a container the agent gets no default action for SIGTERM — the
//! kernel ignores signals PID 1 has no handler for — so before it installed one,
//! every redeploy sat out Docker's 10 s stop timeout and ended in SIGKILL (exit
//! 137). Measured against the v0.2.17 image: 10172 ms and 137 before, 205 ms
//! and 0 after.
//!
//! These run the real binary and send it a real signal, because the defect was
//! never in a function a unit test could call: it was the absence of a handler
//! in `main`. A child process is not PID 1, so this pins that the handler exists
//! and exits cleanly, not the PID 1 behaviour itself.
//!
//! The agent will not start without a Docker daemon, so these skip themselves
//! where there is none — the same bargain `docker_guard.rs` makes.

#![cfg(unix)]

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use platform_agent::docker::DockerRuntime;

const BIN: &str = env!("CARGO_BIN_EXE_platform-agent");

async fn docker_available() -> bool {
    match DockerRuntime::connect() {
        Ok(rt) => rt.ping().await.is_ok(),
        Err(_) => false,
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("a free port").local_addr().unwrap().port()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gpp-shutdown-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).expect("scratch data root");
    std::fs::create_dir_all(dir.join("packs")).expect("scratch pack dir");
    dir
}

/// Start an agent on a scratch config and wait until its API accepts
/// connections, so the signal lands on a running server rather than on startup.
fn start_agent(name: &str) -> Child {
    let dir = scratch(name);
    let port = free_port();
    let config = dir.join("agent.toml");
    std::fs::write(
        &config,
        format!(
            "data_root = {:?}\nmax_instances = 1\napi_bind = \"127.0.0.1:{port}\"\n\
             [port_range]\nstart = 47900\nend = 47901\n",
            dir.join("data")
        ),
    )
    .expect("scratch config");

    let child = Command::new(BIN)
        .arg(&config)
        .arg(dir.join("packs"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the agent binary starts");

    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the agent never started listening on {port}");
        std::thread::sleep(Duration::from_millis(100));
    }
    child
}

fn signal_and_wait(mut child: Child, signal: &str) -> (ExitStatus, Duration) {
    let sent = Instant::now();
    let status =
        Command::new("kill").args([signal, &child.id().to_string()]).status().expect("kill runs");
    assert!(status.success(), "kill {signal} failed");

    let deadline = sent + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().expect("waiting on the agent") {
            return (status, sent.elapsed());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the agent ignored {signal} for 8 s; `docker stop` would SIGKILL it");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn sigterm_stops_the_agent_cleanly() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon, and the agent will not start without one");
        return;
    }
    let (status, took) = signal_and_wait(start_agent("term"), "-TERM");
    assert!(status.success(), "SIGTERM should exit 0, got {status}");
    // Nothing is in flight, so this should not touch the 5 s grace period.
    assert!(took < Duration::from_secs(3), "an idle agent took {took:?} to stop");
}

#[tokio::test]
async fn ctrl_c_stops_the_agent_cleanly() {
    if !docker_available().await {
        eprintln!("skipping: no Docker daemon, and the agent will not start without one");
        return;
    }
    let (status, _) = signal_and_wait(start_agent("int"), "-INT");
    assert!(status.success(), "SIGINT should exit 0, got {status}");
}
