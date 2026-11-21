//! airDroid for air/OS: Android apps in their own windows.
//!
//!   airDroid                       the main window (apps, status)
//!   airDroid --launch PACKAGE      start an Android app (Deskbar entries)
//!   airDroid --full-ui             the whole Android UI in one window
//!   airDroid FILE.apk              install an app (also from Tracker)
//!   airDroid --stop                stop Android
//!
//! The Be API side runs on the main thread (BApplication); the engine
//! (VM, session, frames, input) on its own thread.

mod engine;
mod host;
mod keys;
mod paths;
mod present;
mod settings;

use std::sync::mpsc;

const SIGNATURE: &str = "application/x-vnd.airos-airDroid";

fn main() {
    if !host::init(SIGNATURE) {
        // B_SINGLE_LAUNCH: the running airDroid got our arguments
        return;
    }
    host::register_types(SIGNATURE);
    let (tx, rx) = mpsc::channel();

    // Be events -> engine
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("airDroid events".into())
            .spawn(move || loop {
                if let Some(ev) = host::next_event(-1) {
                    if tx.send(engine::Msg::Host(ev)).is_err() {
                        return;
                    }
                }
            })
            .expect("event thread");
    }

    // On air/OS the arguments come as B_ARGV_RECEIVED, even for the first
    // instance; elsewhere take them from the command line.
    let args: Vec<String> = if cfg!(target_os = "haiku") { Vec::new() } else { std::env::args().skip(1).collect() };
    host::main_show();
    let engine = engine::Engine::new(tx);
    std::thread::Builder::new()
        .name("airDroid engine".into())
        .stack_size(8 << 20)
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run(rx, args)));
            if r.is_err() {
                host::quit();
            }
        })
        .expect("engine thread");
    host::run();
}
