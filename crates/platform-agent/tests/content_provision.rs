//! The `steamcmd` content driver, against a real Docker daemon.
//!
//! Skips itself without Docker, like the other integration tests here, and
//! builds its stand-in images from the busybox already on the host rather than
//! pulling the real steamcmd image — this exercises the agent's half (build the
//! command line, mount the staging directory, judge the exit code, keep or
//! discard the result), which is the half that can be wrong.

use platform_agent::content::{ProvisionError, Provisioned, Provisioner};
use platform_agent::docker::DockerRuntime;
use platform_agent::store::{ContentRef, StoreLayout};

use game_bridge::content::PackContent;

const OK_IMAGE: &str = "gpp-test-steamcmd-ok:latest";
const FAIL_IMAGE: &str = "gpp-test-steamcmd-fail:latest";

async fn docker_or_skip() -> Option<DockerRuntime> {
    let rt = DockerRuntime::connect().ok()?;
    rt.ping().await.ok()?;
    Some(rt)
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
    let dir = std::env::temp_dir().join(format!("gpp-content-image-{}-{}", std::process::id(), tag.replace(':', "-")));
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

/// Stands in for steamcmd: writes into whatever `+force_install_dir` named, so
/// the test proves the agent passed a directory the tool could actually write.
const OK_DOCKERFILE: &str = r#"FROM busybox
RUN printf '#!/bin/sh\nwhile [ "$1" != "+force_install_dir" ]; do shift || exit 3; done\nshift\nmkdir -p "$1/game"\necho installed > "$1/game/data.txt"\n' > /fake && chmod +x /fake
ENTRYPOINT ["/fake"]
"#;

const FAIL_DOCKERFILE: &str = r#"FROM busybox
ENTRYPOINT ["/bin/sh", "-c", "echo 'ERROR! app_update failed: No subscription' >&2; exit 8"]
"#;

fn sven() -> ContentRef {
    ContentRef { game_id: "sven-coop".to_string(), version: "5.26".to_string() }
}

#[tokio::test(flavor = "multi_thread")]
async fn steamcmd_installs_what_the_tool_wrote() {
    let Some(docker) = docker_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_image(OK_IMAGE, OK_DOCKERFILE) {
        eprintln!("skipping: could not build the stand-in image");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let layout = StoreLayout::new(tmp.path().to_path_buf());
    let provisioner = Provisioner::new(layout.clone(), true, Some(OK_IMAGE.to_string()));
    let spec = PackContent::Steamcmd { app_id: 276060 };

    let out = provisioner.ensure(&sven(), &spec, Some(&docker)).await.unwrap();
    let dir = layout.content_dir(&sven()).unwrap();
    match out {
        Provisioned::Installed { dir: installed, .. } => assert_eq!(installed, dir),
        other => panic!("expected an install, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("game/data.txt")).unwrap().trim(),
        "installed"
    );

    // Second call installs nothing: content that exists is never re-fetched.
    assert_eq!(
        provisioner.ensure(&sven(), &spec, Some(&docker)).await.unwrap(),
        Provisioned::AlreadyInstalled(dir)
    );
}

/// A failed run is not a partial install. The same rule as a failed digest:
/// nothing half-done becomes content, and what the tool said comes back with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_steamcmd_run_installs_nothing_and_reports_what_it_said() {
    let Some(docker) = docker_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_image(FAIL_IMAGE, FAIL_DOCKERFILE) {
        eprintln!("skipping: could not build the stand-in image");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let layout = StoreLayout::new(tmp.path().to_path_buf());
    let provisioner = Provisioner::new(layout.clone(), true, Some(FAIL_IMAGE.to_string()));

    match provisioner
        .ensure(&sven(), &PackContent::Steamcmd { app_id: 276060 }, Some(&docker))
        .await
    {
        Err(ProvisionError::ToolFailed { tool, exit_code, output }) => {
            assert_eq!(tool, "steamcmd");
            assert_eq!(exit_code, 8);
            assert!(output.contains("No subscription"), "{output}");
        }
        other => panic!("expected the tool's failure to surface, got {other:?}"),
    }
    assert!(!layout.content_dir(&sven()).unwrap().exists());
}

/// A provisioning container is short-lived and must not linger as something the
/// agent then reports as an instance.
#[tokio::test(flavor = "multi_thread")]
async fn a_provisioning_run_leaves_no_container_behind() {
    let Some(docker) = docker_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_image(OK_IMAGE, OK_DOCKERFILE) {
        eprintln!("skipping: could not build the stand-in image");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let layout = StoreLayout::new(tmp.path().to_path_buf());
    let before = docker.list_managed().await.unwrap().len();
    Provisioner::new(layout, true, Some(OK_IMAGE.to_string()))
        .ensure(&sven(), &PackContent::Steamcmd { app_id: 276060 }, Some(&docker))
        .await
        .unwrap();
    assert_eq!(docker.list_managed().await.unwrap().len(), before);
}
