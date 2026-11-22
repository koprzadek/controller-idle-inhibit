//! Integration tests that run the real client code against a fake Wayland
//! compositor (tests/mock_compositor.py, over a real unix socket).
//!
//! Scenario 1 — normal handshake:
//!   connect -> registry round-trip -> binds -> invisible layer surface
//!   mapped (initial commit, configure, ack, attach, commit) -> inhibitor
//!   created on demand -> inhibitor destroyed when the desire drops.
//!
//! Scenario 2 — output disappears:
//!   after the first map the mock closes the layer surface, removes the
//!   wl_output global and advertises a fresh one; the session must tear
//!   down and rebuild the surface on the new output, after which inhibition
//!   works again.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use controller_idle_inhibit::session::Session;

struct MockGuard(Child, PathBuf, PathBuf);

impl Drop for MockGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
        let _ = std::fs::remove_file(&self.2);
    }
}

fn spawn_mock(dir: &Path, name: &str, closed_test: bool) -> MockGuard {
    let sock = dir.join(format!("{name}.sock"));
    let log = dir.join(format!("{name}.log"));
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&log);

    let mut cmd = Command::new("python3");
    cmd.arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_compositor.py"))
        .arg(&sock)
        .arg(&log);
    if closed_test {
        cmd.arg("--closed-test");
    }
    let child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mock compositor (is python3 available?)");
    let guard = MockGuard(child, sock, log);

    // Wait for the unix socket to appear.
    let start = Instant::now();
    while !guard.1.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "mock compositor socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Give the mock a moment to reach accept().
    std::thread::sleep(Duration::from_millis(100));
    guard
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn count(log: &str, needle: &str) -> usize {
    log.matches(needle).count()
}

fn wait_for(path: &Path, needle: &str, min_occurrences: usize, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if count(&read_log(path), needle) >= min_occurrences {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    count(&read_log(path), needle) >= min_occurrences
}

#[test]
fn session_against_mock_compositor() {
    let dir = std::env::temp_dir().join(format!("cii-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // ---- Scenario 1: normal handshake and inhibitor toggling ----
    let _mock = spawn_mock(&dir, "mock1", false);

    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    std::env::set_var("WAYLAND_DISPLAY", "mock1.sock");

    let mut session = Session::connect(false).expect("Session::connect against mock");

    // Pump repeatedly, like the daemon's 100 ms main loop does, until the
    // mock confirms the surface is mapped.
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&read_log(&_mock.2), "MAPPED") < 1 {
        assert!(Instant::now() < deadline, "surface never mapped; mock log:\n{}", read_log(&_mock.2));
        session.tick().expect("tick during mapping");
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = read_log(&_mock.2);
    assert!(log.contains("GET-LAYER-SURFACE"), "log:\n{log}");
    assert!(log.contains("namespace=controller-idle-inhibit"), "log:\n{log}");
    assert!(
        !log.contains("NULL-OUTPUT"),
        "an explicit output must be passed (river panics on null)"
    );
    assert!(
        !log.contains("CREATE-INHIBITOR"),
        "no inhibitor before activity"
    );
    assert!(log.contains("CREATE-BUFFER id=") && log.contains(" 1x1 stride=4 fmt=0"));

    // Inhibit on demand.
    session.set_desired(true);
    session.tick().expect("tick (inhibit)");
    assert!(
        wait_for(&_mock.2, "CREATE-INHIBITOR", 1, Duration::from_secs(5)),
        "inhibitor not created; log:\n{}",
        read_log(&_mock.2)
    );

    // Release again.
    session.set_desired(false);
    session.tick().expect("tick (release)");
    assert!(
        wait_for(&_mock.2, "DESTROY-INHIBITOR", 1, Duration::from_secs(5)),
        "inhibitor not destroyed; log:\n{}",
        read_log(&_mock.2)
    );

    // Toggling again re-uses the still-mapped surface (no new layer surface).
    session.set_desired(true);
    session.tick().expect("tick (re-inhibit)");
    assert!(wait_for(&_mock.2, "CREATE-INHIBITOR", 2, Duration::from_secs(5)));
    assert_eq!(count(&read_log(&_mock.2), "GET-LAYER-SURFACE"), 1);

    drop(session); // disconnect; mock sees CLIENT-DISCONNECT
    drop(_mock);

    // ---- Scenario 2: output removed -> rebuild on the new output ----
    let _mock2 = spawn_mock(&dir, "mock2", true);
    std::env::set_var("WAYLAND_DISPLAY", "mock2.sock");

    let mut session = Session::connect(false).expect("connect (closed-test)");

    // Pump like the daemon's main loop until the first surface is mapped.
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&read_log(&_mock2.2), "MAPPED") < 1 {
        assert!(
            Instant::now() < deadline,
            "surface never mapped; mock log:\n{}",
            read_log(&_mock2.2)
        );
        session.tick().expect("tick during first mapping");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Drain the closed + global_remove + global-add burst and let the
    // session rebuild.
    for _ in 0..10 {
        if let Err(e) = session.tick() {
            panic!("tick failed after closed: {e}");
        }
        if count(&read_log(&_mock2.2), "MAPPED") >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let log2 = read_log(&_mock2.2);
    assert!(
        count(&log2, "MAPPED") >= 2 && count(&log2, "GET-LAYER-SURFACE") >= 2,
        "surface not rebuilt after output removal; log:\n{log2}"
    );
    assert!(log2.contains("CLOSED-SENT"), "log:\n{log2}");

    // Inhibition works again on the fresh surface.
    session.set_desired(true);
    session.tick().expect("tick (inhibit after rebuild)");
    assert!(
        wait_for(&_mock2.2, "CREATE-INHIBITOR", 1, Duration::from_secs(5)),
        "inhibitor not created after rebuild; log:\n{}",
        read_log(&_mock2.2)
    );

    drop(session);
    drop(_mock2);
    let _ = std::fs::remove_dir_all(&dir);
}
