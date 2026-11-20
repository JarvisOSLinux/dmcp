//! Execution backends, end to end: the `Command` dmcp actually builds is
//! spawned, and what comes back proves the launch line ran where the manifest
//! said it would.
//!
//! The unit tests assert the argv. These assert that the argv *works* — that
//! `docker run --env-file … -i --rm --mount … -w … image command args` is a real
//! invocation, that a config value crosses into the container through the env
//! file rather than the command line, and that it never reaches docker itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use dmcp::call::build_stdio_command;
use dmcp::discovery::Scope;
use dmcp::models::Manifest;
use dmcp::paths::Paths;

const IMAGE: &str = "python:3-slim";

fn paths_for(install_dir: &std::path::Path) -> Paths {
    Paths {
        user_sources: install_dir.join("sources.list"),
        user_install_dir: install_dir.to_path_buf(),
        system_sources: PathBuf::from("/etc/mcp/sources.list"),
        system_install_dir: PathBuf::from("/usr/share/mcp/installed"),
        vector_index_dir: install_dir.join("vector_index"),
    }
}

fn manifest_for(install_dir: &std::path::Path, transport: serde_json::Value) -> Manifest {
    serde_json::from_value(serde_json::json!({
        "id": "com.test.execution",
        "installDir": install_dir.to_string_lossy(),
        "transports": [transport],
        "configurableProperties": [{"key": "DMCP_EXEC_TEST_TOKEN", "sensitive": true}],
        "config": {"DMCP_EXEC_TEST_TOKEN": "token-from-config"},
    }))
    .expect("fixture manifest must parse")
}

fn temp_dir(name: &str) -> PathBuf {
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!(
        "dmcp-execution-{}-{}",
        name,
        std::process::id()
    )));
    let dir = std::env::temp_dir().join(format!("dmcp-execution-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build the Command the production path would spawn, then run it to completion.
/// Also returns where its env file was, which is gone once this returns.
fn run_planned(
    manifest: &Manifest,
    dir: &std::path::Path,
) -> (std::process::Output, Vec<String>, Option<PathBuf>) {
    let paths = paths_for(dir);
    let transport = dmcp::transport::select(manifest.transports.as_deref()).unwrap();
    let (mut cmd, env_file) = build_stdio_command(
        &paths,
        manifest,
        "com.test.execution",
        Scope::User,
        transport,
    )
    .expect("the spawn plan must build");
    let argv: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let output = cmd
        .as_std_mut()
        .output()
        .expect("the planned command must be spawnable");
    let env_file_path = env_file.as_ref().map(|f| f.path().to_path_buf());
    drop(env_file);
    (output, argv, env_file_path)
}

/// `docker ps` succeeds and the image is available (pulling it if need be).
/// Anything else is a clean skip: a machine without a working docker, or
/// without the network to fetch the image, is not a failing machine.
fn docker_ready() -> bool {
    let daemon = Command::new("docker")
        .args(["ps"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !daemon {
        eprintln!("[skip] docker ps failed: no usable docker daemon here");
        return false;
    }
    let present = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return true;
    }
    let pulled = Command::new("docker")
        .args(["pull", IMAGE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !pulled {
        eprintln!("[skip] {IMAGE} is not present and could not be pulled");
    }
    pulled
}

/// The whole feature in one run: the launch line executes inside the container,
/// the install dir is mounted at the same path and is the working directory, and
/// the configured value arrives in the container's environment while never
/// appearing in the command line that any local account can read from /proc.
///
/// The install dir holds a `:` and a `,`, which `-v` could not mount. A setting
/// named `DOCKER_HOST` points at nothing: had it been set on the docker client,
/// docker would have failed to reach its daemon. It reaches the container only.
#[test]
fn a_docker_execution_block_runs_the_launch_line_in_a_container() {
    if !docker_ready() {
        return;
    }

    let dir = temp_dir("docker:a,b");
    std::fs::write(dir.join("marker.txt"), "mounted-payload").unwrap();

    let script = "import os,pathlib;\
                  print('TOKEN=' + os.environ.get('DMCP_EXEC_TEST_TOKEN', 'MISSING'));\
                  print('DOCKER_HOST=' + os.environ.get('DOCKER_HOST', 'MISSING'));\
                  print('MOUNT=' + pathlib.Path('marker.txt').read_text().strip());\
                  print('CONTAINER=' + str(os.path.exists('/.dockerenv')))";
    let mut manifest = manifest_for(
        &dir,
        serde_json::json!({
            "type": "stdio",
            "command": "python3",
            "args": ["-c", script],
            "execution": {"type": "docker", "image": IMAGE},
        }),
    );
    manifest
        .configurable_properties
        .push(dmcp::models::ConfigurableProperty {
            key: "DOCKER_HOST".into(),
            ..Default::default()
        });
    manifest.config.insert(
        "DOCKER_HOST".into(),
        serde_json::json!("unix:///nonexistent/dmcp-test.sock"),
    );

    let (output, argv, env_file) = run_planned(&manifest, &dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "container exited {:?}\nstdout: {stdout}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("CONTAINER=True"),
        "the launch line ran inside the container: {stdout}"
    );
    assert!(
        stdout.contains("TOKEN=token-from-config"),
        "the configured value reached the container: {stdout}"
    );
    assert!(
        stdout.contains("DOCKER_HOST=unix:///nonexistent/dmcp-test.sock"),
        "a setting docker itself reads reached the container, not docker: {stdout}"
    );
    assert!(
        stdout.contains("MOUNT=mounted-payload"),
        "the install dir is mounted and is the working directory: {stdout}"
    );
    assert!(
        !argv.iter().any(|a| a.contains("token-from-config")),
        "the value must travel by environment, not on the command line: {argv:?}"
    );
    let env_file = env_file.expect("a container with settings gets an env file");
    assert!(
        !env_file.exists(),
        "the env file is removed: {}",
        env_file.display()
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The wrapper form needs nothing installed: `env` is a program that execs its
/// argument, which is exactly the contract a wrapper has to satisfy.
#[cfg(unix)]
#[test]
fn a_wrapper_execution_block_prefixes_the_real_launch_line() {
    let dir = temp_dir("wrapper");
    let manifest = manifest_for(
        &dir,
        serde_json::json!({
            "type": "stdio",
            "command": "sh",
            "args": ["-c", "printf 'ran:%s\\n' \"$DMCP_EXEC_TEST_TOKEN\""],
            "execution": {"wrapper": ["env"]},
        }),
    );

    let (output, argv, env_file) = run_planned(&manifest, &dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "wrapper spawn failed: {stdout}");
    assert!(env_file.is_none());
    assert_eq!(
        stdout.trim(),
        "ran:token-from-config",
        "the wrapper ran the manifest's own launch line, with the config env set on it"
    );
    assert_eq!(
        argv.first().map(String::as_str),
        Some("sh"),
        "the launch line follows the prefix: {argv:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A manifest whose execution block does not parse must not fall back to an
/// unwrapped host spawn — the whole manifest is unreadable instead, which every
/// caller already treats as "this server does not run".
#[test]
fn a_typo_in_the_execution_block_is_never_read_as_no_backend() {
    let raw = serde_json::json!({
        "id": "com.test.execution",
        "transports": [{
            "type": "stdio",
            "command": "python3",
            "execution": {"type": "docker", "imgae": IMAGE},
        }],
    });
    let parsed = serde_json::from_value::<Manifest>(raw);
    let err = parsed.expect_err("a typo must not parse into a manifest");
    assert!(err.to_string().contains("imgae"), "{err}");
}

/// `plan_spawn` is the one decision every spawn site builds from, so this is
/// also the guarantee that `call`, `tools`, `run` and the broker agree.
#[test]
fn the_plan_is_shared_by_every_spawn_site() {
    let dir = temp_dir("shared");
    let manifest = manifest_for(
        &dir,
        serde_json::json!({
            "type": "stdio",
            "command": "python3",
            "args": ["server.py"],
            "execution": {"type": "docker", "image": IMAGE, "mountInstallDir": "rw"},
        }),
    );
    let transport = dmcp::transport::select(manifest.transports.as_deref()).unwrap();
    let plan = dmcp::call::stdio_spawn_plan(
        &paths_for(&dir),
        &manifest,
        "com.test.execution",
        Scope::User,
        transport,
    )
    .unwrap();
    let (built, env_file) = build_stdio_command(
        &paths_for(&dir),
        &manifest,
        "com.test.execution",
        Scope::User,
        transport,
    )
    .unwrap();
    let env_file = env_file.expect("a container with settings gets an env file");

    let std = built.as_std();
    // The resolved path: build_stdio_command resolves the program it starts.
    assert!(
        std.get_program().to_string_lossy().ends_with(&plan.program),
        "{:?}",
        std.get_program()
    );
    let mut args: Vec<String> = std
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args.drain(1..3).collect::<Vec<_>>(),
        vec![
            "--env-file".to_string(),
            env_file.path().to_string_lossy().into_owned()
        ],
        "the env file is named right after `run`"
    );
    assert_eq!(args, plan.args);
    assert_eq!(std.get_current_dir(), Some(plan.current_dir.as_path()));
    let env: HashMap<String, String> = std
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.unwrap_or_default().to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert!(
        !env.contains_key("DMCP_EXEC_TEST_TOKEN"),
        "settings are not set on the docker client: {env:?}"
    );
    assert_eq!(
        std::fs::read_to_string(env_file.path()).unwrap(),
        "DMCP_EXEC_TEST_TOKEN=token-from-config\n"
    );

    std::fs::remove_dir_all(&dir).ok();
}
