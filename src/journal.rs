//! Tiny logging helpers. Everything goes to stdout — redirect it to a file
//! or let your session (compositor autostart, service manager, terminal)
//! capture it however you like.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn info(msg: &str) {
    println!("[info] {msg}");
}

pub fn warn(msg: &str) {
    println!("[warn] {msg}");
}

pub fn error(msg: &str) {
    println!("[error] {msg}");
}

pub fn debug(msg: &str) {
    if verbose() {
        println!("[debug] {msg}");
    }
}
