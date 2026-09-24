//! Mode 3: a room leaves nothing behind (`PLAN.md` §14, step 5b).
//!
//! The rule is that nothing a room creates can outlive the process that
//! opened it, however that process ends. Two ways in, both driven through the
//! real `game-bridge lan-host` and the real `lan-helper`:
//!
//! - **`kill -9`.** No handler runs, no cleanup code gets a turn. The adapter
//!   must still go, and so must the helper: it sees its relay drop and exits,
//!   and the non-persistent TUN device goes with its last descriptor.
//! - **The per-room path** a portable or AppImage launcher takes: nothing is
//!   granted, so the helper runs as a copy staged in the runtime directory,
//!   through an elevator. `pkexec` needs a person at a prompt, so a debug build
//!   takes a stand-in that simply runs the command (`ELEVATOR_ENV`). The copy
//!   must be gone once the helper is running, and nothing may remain after.
//!
//! Runs inside `unshare -rn` like the other adapter tests, and skips where
//! unprivileged user namespaces are off.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use game_bridge::lan_adapter::ELEVATOR_ENV;

// The helpers here use fixed ports inside a private namespace.
#[allow(dead_code)]
mod common;

const INSIDE: &str = "GAME_BRIDGE_LAN_NO_GARBAGE_TEST_INSIDE";

#[test]
fn a_room_leaves_nothing_behind_on_kill_or_on_the_per_room_path() {
    if std::env::var_os(INSIDE).is_some() {
        return;
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
    assert!(
        out.status.success(),
        "the inner run failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(stdout.contains("1 passed"), "the inner run did not run\n{stdout}");
}

/// A thread in its own network namespace; what it spawns lives there too.
struct Namespace {
    jobs: mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

impl Namespace {
    fn new() -> Self {
        let (jobs, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: moves only this thread into a new network namespace.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNET) }, 0);
            ready_tx.send(()).unwrap();
            for job in rx {
                job();
            }
        });
        ready_rx.recv().unwrap();
        let ns = Self { jobs };
        ns.run(|| Command::new("ip").args(["link", "set", "lo", "up"]).status().unwrap());
        ns
    }

    fn run<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        self.jobs.send(Box::new(move || tx.send(f()).unwrap())).unwrap();
        rx.recv().unwrap()
    }

    fn has_adapter(&self) -> bool {
        self.run(|| {
            Command::new("ip").args(["link", "show", "gbl0"]).output().unwrap().status.success()
        })
    }
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Every running `lan-helper serve` started by `parent` — this test's
/// `lan-host`, and so this test's helper and nobody else's.
fn helpers(parent: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(cmd) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
        // Field 4 of /proc/<pid>/stat is the parent; the name before it is
        // parenthesized and may hold spaces, so split after the last ')'.
        let ppid = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|st| st.rsplit_once(')').map(|(_, rest)| rest.to_string()))
            .and_then(|rest| rest.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()));
        if cmd.contains(" serve gbl0 ") && ppid == Some(parent) {
            let exe = std::fs::read_link(entry.path().join("exe"))
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            out.push((pid, exe));
        }
    }
    out
}

fn packs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs")
}

fn lan_host(ns: &Namespace, dir: &Path, port: u16, env: Vec<(&'static str, PathBuf)>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_game-bridge"));
    cmd.args(["lan-host", "openttd", "--tcp", &format!("0.0.0.0:{port}")])
        .arg("--packs")
        .arg(packs_dir())
        .arg("--identity")
        .arg(dir.join(format!("host-{port}.identity")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join(format!("host-{port}.log"))).unwrap());
    for (k, v) in env {
        cmd.env(k, v);
    }
    ns.run(move || cmd.spawn().expect("lan-host starts"))
}

#[test]
#[ignore = "runs inside `unshare -rn`, started by a_room_leaves_nothing_behind_on_kill_or_on_the_per_room_path"]
fn inside_a_user_namespace() {
    if std::env::var_os(INSIDE).is_none() {
        return;
    }
    let dir = common::scratch_dir("lan-no-garbage");

    // 1. kill -9, with a helper that holds its capability (as root in this
    //    namespace, it does).
    let ns = Namespace::new();
    let mut host = lan_host(&ns, &dir, 4301, Vec::new());
    wait_for("the adapter comes up", Duration::from_secs(30), || ns.has_adapter());
    let running = helpers(host.id());
    assert_eq!(running.len(), 1, "exactly one helper holds the adapter: {running:?}");
    let helper_pid = running[0].0;
    // SAFETY: signalling a child we started.
    unsafe { libc::kill(host.id() as i32, libc::SIGKILL) };
    let _ = host.wait();
    wait_for("the adapter is gone after kill -9", Duration::from_secs(15), || !ns.has_adapter());
    wait_for("the helper exits after kill -9", Duration::from_secs(15), || {
        !Path::new(&format!("/proc/{helper_pid}")).exists()
    });

    // 2. The per-room path: nothing granted, so the helper runs as a staged
    //    copy through an elevator — here a stand-in for pkexec.
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let elevator = dir.join("elevator.sh");
    std::fs::write(&elevator, "#!/bin/sh\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&elevator, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();

    let ns = Namespace::new();
    let mut host = lan_host(
        &ns,
        &dir,
        4302,
        vec![(ELEVATOR_ENV, elevator.clone()), ("XDG_RUNTIME_DIR", run_dir.clone())],
    );
    wait_for("the adapter comes up through the elevator", Duration::from_secs(30), || {
        ns.has_adapter()
    });
    let running = helpers(host.id());
    assert_eq!(running.len(), 1, "{running:?}");
    let (helper_pid, exe) = running[0].clone();
    assert!(
        exe.contains(&run_dir.display().to_string()) && exe.ends_with("(deleted)"),
        "the helper runs from a staged copy that is already deleted: {exe}"
    );
    let left: Vec<_> =
        std::fs::read_dir(&run_dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "the staged copy was not removed once the helper ran: {left:?}");

    // SAFETY: signalling a child we started.
    unsafe { libc::kill(host.id() as i32, libc::SIGINT) };
    let status = host.wait().unwrap();
    assert!(status.success(), "lan-host exited {status} on Ctrl-C");
    wait_for("the adapter is gone after leaving", Duration::from_secs(15), || !ns.has_adapter());
    wait_for("the helper exits after leaving", Duration::from_secs(15), || {
        !Path::new(&format!("/proc/{helper_pid}")).exists()
    });
    let left: Vec<_> =
        std::fs::read_dir(&run_dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "something was left in the runtime directory: {left:?}");
}
