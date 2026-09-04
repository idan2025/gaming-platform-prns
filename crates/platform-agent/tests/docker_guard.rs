//! Integration tests against a real Docker daemon.
//!
//! These skip themselves when Docker is unavailable rather than failing, so the
//! suite still runs on a machine without it. When Docker *is* available they
//! exercise the property that matters most in this crate: **the agent must
//! never touch a container it did not create.**
//!
//! The machine this was written on was already running unrelated containers, and
//! that is the normal case for a node someone volunteers. It is exactly why the
//! guard reads an inspected label rather than trusting a name prefix.

use std::collections::BTreeMap;

use platform_agent::config::{GameRuntime, MANAGED_LABEL};
use game_bridge::profile::GameTransport;
use platform_agent::docker::{DockerRuntime, PublishedPort};
use platform_agent::instance::{InstanceSpec, InstanceState};
use platform_agent::store::Mount;

const TEST_IMAGE: &str = "gpp-test-sleeper:latest";
/// A stand-in for a dedicated server's console: it reads a line at a time from
/// stdin and says what it heard, which is what a GoldSrc or Source server does
/// with `changelevel`. Enough to prove the plumbing without a 2.7 GB game.
const CONSOLE_IMAGE: &str = "gpp-test-console:latest";

async fn runtime_or_skip() -> Option<DockerRuntime> {
    let rt = DockerRuntime::connect().ok()?;
    rt.ping().await.ok()?;
    Some(rt)
}

/// A tiny local image that stays up, built from the busybox already on the host
/// so the test needs no network.
fn ensure_test_image() -> bool {
    ensure_image(TEST_IMAGE, "FROM busybox\nCMD [\"sleep\", \"3600\"]\n")
}

/// The console stand-in. `read` blocks on stdin, so this also fails the moment
/// a container is created without one — which is the half of `send_console_line`
/// that cannot be tested any other way.
fn ensure_console_image() -> bool {
    ensure_image(
        CONSOLE_IMAGE,
        "FROM busybox\nCMD [\"sh\", \"-c\", \"while read line; do echo CONSOLE:$line; done; sleep 3600\"]\n",
    )
}

fn ensure_image(tag: &str, dockerfile: &str) -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!(
        "gpp-test-image-{}-{}",
        std::process::id(),
        tag.replace([':', '/'], "-")
    ));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    if std::fs::write(dir.join("Dockerfile"), dockerfile).is_err() {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", tag, "."])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(built, Ok(s) if s.success())
}

fn docker_logs(name: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["logs", name])
        .output();
    match out {
        Ok(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => String::new(),
    }
}

fn docker_env(name: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{range .Config.Env}}{{println .}}{{end}}", name])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

fn docker_rm(name: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn test_runtime() -> GameRuntime {
    GameRuntime {
        image: TEST_IMAGE.to_string(),
        content_root: "/game".into(),
        content_version: "test".to_string(),
        memory_limit_bytes: Some(64 * 1024 * 1024),
        cpus: Some(0.5),
        env: BTreeMap::new(),
    }
}

/// One UDP game port, which is what a single-port pack asks for.
fn one_port(host_port: u16) -> Vec<PublishedPort> {
    vec![PublishedPort {
        channel: 0,
        container_port: host_port,
        host_port,
        transport: GameTransport::Udp,
    }]
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".to_string(),
        name: "Test".to_string(),
        max_players: 8,
        port: None,
        extra_ports: BTreeMap::new(),
        map: None,
        owner: None,
    }
}

/// The guard, tested for real: a container carrying this agent's *name prefix*
/// but not its label must be refused, and left completely alone. A name prefix
/// is a convention anyone can collide with; the label is the only thing this
/// agent actually sets.
#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_touch_a_container_it_did_not_create() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let id = format!("guardtest{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let created = std::process::Command::new("docker")
        .args(["create", "--name", &name, "busybox", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(created, Ok(s) if s.success()) {
        eprintln!("skipping: could not create the fixture container");
        return;
    }

    let stop = rt.stop(&id).await;
    let remove = rt.remove(&id).await;
    let survived = matches!(
        std::process::Command::new("docker")
            .args(["inspect", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    );
    let managed = rt.list_managed().await.unwrap();
    docker_rm(&name);

    assert!(stop.is_err(), "the agent stopped a container it does not manage");
    assert!(remove.is_err(), "the agent removed a container it does not manage");
    assert!(survived, "the unmanaged container should have been left alone entirely");
    assert!(
        !managed.iter().any(|c| c.instance_id == id),
        "an unlabelled container turned up in the managed list"
    );
}

/// Whatever else is running on this host must be invisible to the agent.
#[tokio::test(flavor = "multi_thread")]
async fn other_peoples_containers_are_not_listed() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let Ok(all) = std::process::Command::new("docker").args(["ps", "-aq"]).output() else {
        return;
    };
    let total = String::from_utf8_lossy(&all.stdout).lines().count();
    let managed = rt.list_managed().await.unwrap();
    assert!(managed.len() <= total, "the agent claims more containers than exist");
    for c in &managed {
        assert!(!c.instance_id.is_empty(), "a managed container with no instance label");
    }
}

/// The full lifecycle against a real daemon: create with mounts and a port
/// binding, see it running, stop it, remove it.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_can_be_created_started_stopped_and_removed() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        eprintln!("skipping: could not build the test image");
        return;
    }

    let id = format!("life{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-life-{}", std::process::id()));
    let content = dir.join("content");
    let writable = dir.join("writable");
    // The mountpoint must exist inside the read-only content copy: a writable
    // bind nested in a read-only bind cannot have its mountpoint created by the
    // container runtime. A real game install already has these directories.
    std::fs::create_dir_all(content.join("logs")).unwrap();
    std::fs::create_dir_all(&writable).unwrap();

    let mounts = vec![
        Mount { host_path: content.clone(), container_path: "/game".into(), read_only: true },
        Mount { host_path: writable.clone(), container_path: "/game/logs".into(), read_only: false },
    ];

    let outcome = async {
        rt.create_and_start(&spec(&id), &test_runtime(), &mounts, &one_port(27199)).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Running, "the instance did not come up");

        let managed = rt.list_managed().await?;
        assert!(
            managed.iter().any(|c| c.instance_id == id),
            "the agent's own container is missing from its inventory"
        );

        rt.stop(&id).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Stopped);
        rt.remove(&id).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Missing);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    docker_rm(&name);
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("the instance lifecycle should complete");
}

/// A multi-port game (`GAMES.md` §3) against a real daemon: a UDP game port and
/// a TCP RCON port on one container, published on different host ports, and read
/// back with each channel still attached to the right one.
///
/// The transports are the point. Publishing RCON as UDP would produce a port
/// that answers nothing, and Docker would report it just as happily.
#[tokio::test(flavor = "multi_thread")]
async fn a_multi_port_instance_publishes_each_port_in_its_own_transport() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        eprintln!("skipping: could not build the test image");
        return;
    }

    let id = format!("mport{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    // The game binds 27015 inside the container whichever node it lands on; the
    // host side comes from the node's own range, which is why they differ here.
    let ports = vec![
        PublishedPort {
            channel: 0,
            container_port: 27015,
            host_port: 27191,
            transport: GameTransport::Udp,
        },
        PublishedPort {
            channel: 1,
            container_port: 27015,
            host_port: 27192,
            transport: GameTransport::Tcp,
        },
    ];

    let outcome = async {
        rt.create_and_start(&spec(&id), &test_runtime(), &[], &ports).await?;
        let managed = rt.list_managed().await?;
        let me = managed
            .iter()
            .find(|c| c.instance_id == id)
            .expect("the container the agent just created is missing from its inventory");

        assert_eq!(me.ports.len(), 2, "both published ports should come back");
        let game = me.ports.iter().find(|p| p.channel == 0).expect("no channel 0");
        let rcon = me.ports.iter().find(|p| p.channel == 1).expect("no channel 1");
        assert_eq!(game.host_port, 27191);
        assert_eq!(game.transport, GameTransport::Udp);
        assert_eq!(rcon.host_port, 27192);
        assert_eq!(rcon.transport, GameTransport::Tcp, "RCON published as UDP answers nothing");
        assert_eq!(
            me.port,
            Some(27191),
            "the game port is channel 0, not whichever port Docker happened to list first"
        );

        // And the daemon really holds both bindings, in the right protocol.
        let inspected = std::process::Command::new("docker")
            .args(["inspect", "-f", "{{json .HostConfig.PortBindings}}", &name])
            .output()?;
        let text = String::from_utf8_lossy(&inspected.stdout).to_string();
        assert!(text.contains("27015/udp"), "the game port is not published as UDP: {text}");
        assert!(text.contains("27015/tcp"), "the RCON port is not published as TCP: {text}");

        rt.remove(&id).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    docker_rm(&name);
    outcome.expect("a multi-port instance should run");
}

/// The label is what the guard reads, so it had better be on the container the
/// agent itself creates — otherwise the agent would refuse to manage its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_container_carries_the_managed_label() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("label{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let created = rt.create_and_start(&spec(&id), &test_runtime(), &[], &one_port(27198)).await;
    let labels = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{json .Config.Labels}}", &name])
        .output();
    docker_rm(&name);

    created.expect("the container should have been created");
    let text = String::from_utf8_lossy(&labels.expect("docker inspect should run").stdout).to_string();
    assert!(
        text.contains(MANAGED_LABEL),
        "the managed label is missing, so the guard would refuse to manage our own container: {text}"
    );
}

/// The read-only content mount must actually be read-only in the container. If
/// it were writable, one instance could corrupt the copy every other instance
/// on the node is running from.
#[tokio::test(flavor = "multi_thread")]
async fn the_shared_content_mount_is_read_only_in_the_container() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("ro{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-ro-{}", std::process::id()));
    let content = dir.join("content");
    std::fs::create_dir_all(&content).unwrap();

    let mounts = vec![Mount {
        host_path: content.clone(),
        container_path: "/game".into(),
        read_only: true,
    }];

    let created = rt.create_and_start(&spec(&id), &test_runtime(), &mounts, &one_port(27197)).await;
    let write_attempt = std::process::Command::new("docker")
        .args(["exec", &name, "sh", "-c", "touch /game/scribble"])
        .output();
    docker_rm(&name);
    let host_file_appeared = content.join("scribble").exists();
    let _ = std::fs::remove_dir_all(&dir);

    created.expect("the container should have been created");
    let out = write_attempt.expect("docker exec should run");
    assert!(
        !out.status.success(),
        "a write to the shared content mount succeeded; every instance on this node shares it"
    );
    assert!(!host_file_appeared, "the write reached the host's shared content directory");
}

/// The bug an integration test found and a unit test could not: a writable bind
/// nested inside a read-only bind fails to start unless its mountpoint already
/// exists in the read-only source. runc reports it as
/// `mkdirat ... read-only file system`, which says nothing about game content —
/// so the agent has to check for it and say something that does.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_mountpoint_in_shared_content_fails_to_start() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("nomount{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-nomount-{}", std::process::id()));
    let content = dir.join("content");
    let writable = dir.join("writable");
    // Deliberately do NOT create content/logs.
    std::fs::create_dir_all(&content).unwrap();
    std::fs::create_dir_all(&writable).unwrap();

    let mounts = vec![
        Mount { host_path: content.clone(), container_path: "/game".into(), read_only: true },
        Mount { host_path: writable.clone(), container_path: "/game/logs".into(), read_only: false },
    ];
    let result = rt.create_and_start(&spec(&id), &test_runtime(), &mounts, &one_port(27196)).await;
    docker_rm(&name);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        result.is_err(),
        "a writable mount nested in a read-only one started despite a missing mountpoint; \
         if the runtime has changed, `missing_content_dirs` may no longer be needed"
    );
}

/// The check that turns that opaque runtime failure into a sentence naming the
/// path a game install is missing.
#[test]
fn missing_content_dirs_names_what_is_absent() {
    use platform_agent::store::{ContentRef, StoreLayout};

    let dir = std::env::temp_dir().join(format!("gpp-missing-{}", std::process::id()));
    let content = dir.join("content").join("sven-coop").join("1.0");
    std::fs::create_dir_all(content.join("svencoop/maps")).unwrap();

    let layout = StoreLayout::new(&dir);
    let plan = layout
        .plan_instance(
            "inst1",
            &ContentRef { game_id: "sven-coop".into(), version: "1.0".into() },
            std::path::Path::new("/game"),
            &["svencoop/maps".to_string(), "svencoop/logs".to_string()],
        )
        .unwrap();

    let missing = StoreLayout::missing_content_dirs(&plan);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(missing.len(), 1, "only the absent one should be reported");
    assert!(missing[0].ends_with("svencoop/logs"));
}

/// The map an operator picked reaches the game as `GPP_MAP`, and nothing else
/// carries it.
///
/// `images/sven-coop/entrypoint.sh` has read `GPP_MAP` since it was written;
/// the agent simply never set it, so every server started on the image's
/// default. This is the test that fails if the wiring is removed again.
#[tokio::test]
async fn a_chosen_map_reaches_the_container_as_an_environment_variable() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("withmap{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let mut s = spec(&id);
    s.map = Some("svencoop1".to_string());
    rt.create_and_start(&s, &test_runtime(), &[], &one_port(27201)).await.unwrap();
    let env = docker_env(&name);
    docker_rm(&name);

    assert!(env.contains("GPP_MAP=svencoop1"), "the chosen map never reached the game: {env}");
}

/// An instance created without a map says nothing about one, so the image's own
/// default stands. An empty `GPP_MAP` would override it with nothing.
#[tokio::test]
async fn no_chosen_map_sets_no_map_variable_at_all() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("nomap{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    rt.create_and_start(&spec(&id), &test_runtime(), &[], &one_port(27202)).await.unwrap();
    let env = docker_env(&name);
    docker_rm(&name);

    assert!(!env.contains("GPP_MAP"), "an unset map should leave the image's default alone: {env}");
}

/// A line written to a running server's console actually arrives.
///
/// This is the live map change end to end, minus the game: the container is
/// created by the agent (so it is created with stdin open, which is decided at
/// create and cannot be added later), the agent writes one line, and the
/// process on the other side reads it.
#[tokio::test]
async fn a_console_line_reaches_a_running_container() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_console_image() {
        return;
    }
    let id = format!("console{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let mut runtime = test_runtime();
    runtime.image = CONSOLE_IMAGE.to_string();
    rt.create_and_start(&spec(&id), &runtime, &[], &one_port(27203)).await.unwrap();

    let outcome = rt.send_console_line(&id, "changelevel svencoop1").await;

    // The line crosses a socket and then a pipe, so give the shell a moment to
    // read it before reading the log back.
    let mut heard = String::new();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        heard = docker_logs(&name);
        if heard.contains("CONSOLE:changelevel") {
            break;
        }
    }
    docker_rm(&name);

    outcome.unwrap();
    assert!(
        heard.contains("CONSOLE:changelevel svencoop1"),
        "the console never heard the command: {heard:?}"
    );
}

/// One call is one command. A newline in the line would be a second command
/// nobody asked for, and the map name that produced it is already refused by
/// `console.rs` — this is the backstop at the last point before the bytes leave.
#[tokio::test]
async fn a_console_line_carrying_a_second_command_is_refused_before_docker() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    // No container needed: this must be refused before anything is inspected.
    let err = rt
        .send_console_line("no-such-instance", "changelevel a\nquit")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("one line"), "{err}");
}

/// The guard applies here too: a console is a way to make a container do
/// something, so it obeys the same label rule as stop, restart and remove.
#[tokio::test]
async fn the_console_refuses_a_container_this_agent_did_not_create() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("unmanagedconsole{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);
    // Created outside the agent, so it carries no MANAGED_LABEL — and named
    // with the agent's own prefix, because a prefix is exactly what must not be
    // enough.
    let started = std::process::Command::new("docker")
        .args(["run", "-d", "--name", &name, TEST_IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(started, Ok(s) if s.success()) {
        return;
    }

    let err = rt.send_console_line(&id, "changelevel svencoop1").await.unwrap_err().to_string();
    docker_rm(&name);

    assert!(err.contains("not managed by this agent"), "{err}");
}

/// A container created before the agent learned to talk to consoles has no
/// stdin, and Docker does not say so: the attach returns `200 OK` and every
/// byte is discarded. So a map change on one of those has to be refused here,
/// or it reports success and changes nothing.
///
/// The container is created out of band with the managed label and no `-i`,
/// because that is exactly the shape of every instance already running on a
/// node that upgrades.
#[tokio::test]
async fn a_server_started_without_a_console_is_refused_rather_than_silently_ignored() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("nostdin{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let started = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            &format!("{MANAGED_LABEL}=true"),
            TEST_IMAGE,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(started, Ok(s) if s.success()) {
        return;
    }

    let err = rt.send_console_line(&id, "changelevel svencoop1").await.unwrap_err().to_string();
    docker_rm(&name);

    assert!(err.contains("no console attached"), "{err}");
    assert!(
        err.contains("restart will not help"),
        "the message has to say a restart is not the fix, because it is the obvious guess: {err}"
    );
}
