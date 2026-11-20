//! The test guard as another crate's tests meet it. This file is its own
//! executable, linking agent-termd built without `cfg(test)`, as agent-service's
//! and agent's tests do: the guard must still keep it off the user's real
//! termd sockets, and the same code run as an ordinary binary must not be
//! guarded at all.

use agent_termd::testing::{EnvGuard, create_socket_dir};
use agent_termd::{
    Client, Options, TEST_SERVER_ENV, check_test_server, is_test_binary, stop_socket,
};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

const PROBE_ENV: &str = "AGENT_TERMD_GUARD_PROBE";

#[test]
fn test_guard_applies_in_another_crates_test_binary() {
    let mut env = EnvGuard::lock();
    assert!(
        is_test_binary(),
        "{:?} wasn't taken for a test binary",
        std::env::current_exe()
    );

    // A private dir set by hand, without agent_termd::testing.
    let dir = create_socket_dir("gmxt-guard-").unwrap();
    env.remove(TEST_SERVER_ENV);
    env.remove("G3D_TERMD_SOCKET");
    env.set("G3D_TERMD_DIR", &dir);

    let want = "a test tried to use termd without a private socket directory";
    let err = check_test_server().unwrap_err();
    assert!(format!("{err:#}").contains(want), "{err:#}");
    let err = Client::new(Options::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("agent_termd::testing"),
        "{err:#}"
    );

    // Something listens there; stop_socket must not even connect.
    let sock = dir.join("termd-guard.sock");
    let ln = UnixListener::bind(&sock).unwrap();
    ln.set_nonblocking(true).unwrap();
    let err = stop_socket(&sock).unwrap_err();
    assert!(format!("{err:#}").contains(want), "{err:#}");
    assert!(
        ln.accept().is_err(),
        "stop_socket connected without a private dir"
    );

    drop(ln);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run by `test_guard_decision_follows_where_the_binary_lives`, never on its
/// own. It only decides; it dials nothing, so it is safe unguarded.
#[test]
#[ignore = "a probe run in a subprocess by test_guard_decision_follows_where_the_binary_lives"]
fn guard_probe() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let text = |r: anyhow::Result<()>| match r {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("{e:#}"),
    };
    println!("PROBE is_test_binary={}", is_test_binary());
    println!("PROBE check_test_server={}", text(check_test_server()));
    println!(
        "PROBE client_new={}",
        text(Client::new(Options::default()).map(|_| ()))
    );
}

fn run_probe(bin: &Path) -> String {
    let mut last_err = None;
    for _ in 0..20 {
        let out = Command::new(bin)
            .args([
                "--exact",
                "guard_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, "1")
            .env_remove(TEST_SERVER_ENV)
            .env_remove("G3D_TERMD_DIR")
            .env_remove("G3D_TERMD_SOCKET")
            .output();
        match out {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                return stdout
                    .lines()
                    // libtest's "test guard_probe ... " shares the first line.
                    .filter_map(|l| l.split_once("PROBE ").map(|(_, rest)| rest))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            // A just-copied executable can be briefly busy (ETXTBSY) while
            // another thread's fork still holds the copy's write descriptor.
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("running {bin:?}: {e}"),
        }
    }
    panic!("running {bin:?}: {last_err:?}")
}

/// The same code is guarded where cargo runs tests from, and not guarded as
/// an installed binary under a plain name.
#[test]
fn test_guard_decision_follows_where_the_binary_lives() {
    let _env = EnvGuard::lock();
    let exe = std::env::current_exe().unwrap();

    let in_deps = run_probe(&exe);
    assert!(in_deps.contains("is_test_binary=true"), "{in_deps}");
    assert!(
        in_deps.contains(
            "check_test_server=a test tried to use termd without a private socket directory"
        ),
        "{in_deps}"
    );
    assert!(
        in_deps.contains("client_new=a test tried to use termd"),
        "{in_deps}"
    );

    let bin_dir = tempfile::Builder::new()
        .prefix("gmxt-inst-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let installed = bin_dir.path().join("agent");
    std::fs::copy(&exe, &installed).unwrap();
    let out = run_probe(&installed);
    assert_eq!(
        out, "is_test_binary=false\ncheck_test_server=ok\nclient_new=ok",
        "an installed binary was guarded"
    );
}
