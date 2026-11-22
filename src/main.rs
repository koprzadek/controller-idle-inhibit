//! controller-idle-inhibit — inhibit Wayland idle while a gamepad is in use.
//!
//! Main loop: poll gamepads (activity = button or stick events), hold a
//! `zwp_idle_inhibitor_v1` on an invisible layer surface for `--grace`
//! seconds after the last input, and keep the Wayland connection alive
//! across compositor restarts.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use controller_idle_inhibit::gamepad::GamepadWatcher;
use controller_idle_inhibit::journal;
use controller_idle_inhibit::session::Session;

const HELP: &str = "\
controller-idle-inhibit — inhibit Wayland idle while a gamepad is in use

USAGE:
    controller-idle-inhibit [FLAGS]

FLAGS:
    --grace SECS      Keep inhibiting idle this long after the last gamepad
                      input. [default: 120]
    --buttons-only    Count button presses only; ignore stick/trigger movement
                      (useful for pads that stream gyro noise while idle).
    --poll-ms MS      How often to check gamepads and Wayland state.
                      [default: 100]
    --rescan-ms MS    How often to rescan /dev/input for hotplug.
                      [default: 5000]
    --verbose         Trace every Wayland message and state change.
    -h, --help        Show this help.
    -V, --version     Show version.
";

struct Opts {
    grace: Duration,
    poll: Duration,
    rescan: Duration,
    buttons_only: bool,
    verbose: bool,
}

fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts {
        grace: Duration::from_secs(120),
        poll: Duration::from_millis(100),
        rescan: Duration::from_millis(5000),
        buttons_only: false,
        verbose: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |what: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("missing value for {what}"))
        };
        match arg.as_str() {
            "--grace" => {
                let v: u64 = value("--grace")?
                    .parse()
                    .map_err(|_| "grace must be a number of seconds".to_string())?;
                opts.grace = Duration::from_secs(v);
            }
            "--poll-ms" => {
                let v: u64 = value("--poll-ms")?
                    .parse()
                    .map_err(|_| "poll-ms must be a number".to_string())?;
                opts.poll = Duration::from_millis(v);
            }
            "--rescan-ms" => {
                let v: u64 = value("--rescan-ms")?
                    .parse()
                    .map_err(|_| "rescan-ms must be a number".to_string())?;
                opts.rescan = Duration::from_millis(v);
            }
            "--buttons-only" => opts.buttons_only = true,
            "--verbose" | "-v" => opts.verbose = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!(
                    "controller-idle-inhibit {}",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {other:?} (try --help)")),
        }
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    journal::set_verbose(opts.verbose);
    journal::info(&format!(
        "controller-idle-inhibit {} starting (grace {} s, buttons-only: {})",
        env!("CARGO_PKG_VERSION"),
        opts.grace.as_secs(),
        opts.buttons_only
    ));

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if let Err(e) = signal_hook::flag::register(sig, stop.clone()) {
            journal::warn(&format!("cannot register signal {sig}: {e} (Ctrl+C will still stop us)"));
        }
    }

    let mut watcher = GamepadWatcher::new(opts.rescan);
    let mut session: Option<Session> = None;
    // `None` = no activity seen yet → do not inhibit until the user touches
    // the gamepad.
    let mut last_activity: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        if watcher.poll(opts.buttons_only) {
            last_activity = Some(Instant::now());
        }
        let want = last_activity.is_some_and(|t| t.elapsed() <= opts.grace);

        if session.is_none() {
            match Session::connect(opts.verbose) {
                Ok(s) => {
                    journal::info("connected to compositor — ready to inhibit idle");
                    session = Some(s);
                }
                Err(e) => {
                    journal::warn(&format!("compositor not ready ({e}); retrying in 2 s"));
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        }

        let s = session.as_mut().expect("session checked above");
        s.set_desired(want);
        if let Err(e) = s.tick() {
            journal::warn(&format!("Wayland connection lost ({e}); reconnecting in 2 s"));
            s.shutdown();
            session = None;
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        std::thread::sleep(opts.poll);
    }

    if let Some(s) = session.as_mut() {
        s.shutdown();
    }
    journal::info("stopped");
    ExitCode::SUCCESS
}
