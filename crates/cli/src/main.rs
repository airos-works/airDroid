//! airdroid: the command line of the running airDroid app, after waydroid's.
//!
//!   airdroid status
//!   airdroid shell [-u UID] [--vm] COMMAND...   run in Android (as root) or the VM
//!   airdroid logcat [ARGS...]
//!   airdroid app install FILE.apk | remove PACKAGE | launch PACKAGE | list
//!   airdroid app intent ACTION [URI]
//!   airdroid show-full-ui
//!   airdroid prop get KEY | prop set KEY VALUE
//!   airdroid upgrade                             newer Android images, then a restart
//!   airdroid stop

use airdroid_core::control::{call, Request};
use std::path::PathBuf;
// stderr may be gone (a closed ssh session): eprintln! would panic, which
// aborts here
use std::io::Write as _;

fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/boot/home".into());
    PathBuf::from(home).join("config/cache/airDroid")
}

fn usage() -> ! {
    let _ = writeln!(std::io::stderr(), 
        "usage: airdroid status | shell [-u UID] [--vm] COMMAND... | logcat [ARGS...]\n\
         \x20      | app install FILE.apk | app remove PACKAGE | app launch PACKAGE | app list\n\
         \x20      | app intent ACTION [URI] | show-full-ui | prop get KEY | prop set KEY VALUE | upgrade | stop"
    );
    std::process::exit(2)
}

fn shell(argv: Vec<String>, uid: Option<u32>, vm: bool) -> Request {
    // one string: run it with sh -c, as `waydroid shell -- CMD` would
    let sh = if vm { "/bin/sh" } else { "/system/bin/sh" };
    Request::Shell { argv: vec![sh.into(), "-c".into(), argv.join(" ")], uid, vm }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let req = match a.as_slice() {
        ["status"] => Request::Status,
        ["stop"] | ["session", "stop"] => Request::Stop,
        ["upgrade"] => Request::Upgrade,
        ["show-full-ui"] => Request::ShowFullUi,
        ["shell", rest @ ..] => {
            let mut uid = None;
            let mut vm = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "-u" | "--uid" => {
                        uid = rest.get(i + 1).and_then(|u| u.parse().ok());
                        i += 2;
                    }
                    "--vm" => {
                        vm = true;
                        i += 1;
                    }
                    "--" => {
                        i += 1;
                        break;
                    }
                    _ => break,
                }
            }
            if i >= rest.len() {
                usage();
            }
            shell(rest[i..].iter().map(|s| s.to_string()).collect(), uid, vm)
        }
        ["logcat", rest @ ..] => {
            let mut argv = vec!["logcat".to_string()];
            argv.extend(rest.iter().map(|s| s.to_string()));
            shell(argv, None, false)
        }
        ["app", "install", file] => {
            let p = std::fs::canonicalize(file).unwrap_or_else(|e| {
                let _ = writeln!(std::io::stderr(), "airdroid: {file}: {e}");
                std::process::exit(1)
            });
            Request::Install { path: p.to_string_lossy().into_owned() }
        }
        ["app", "remove", pkg] => Request::Remove { package: pkg.to_string() },
        ["app", "launch", pkg] => Request::Launch { package: pkg.to_string() },
        ["app", "list"] => Request::List,
        ["app", "intent", action] => Request::Intent { action: action.to_string(), uri: String::new() },
        ["app", "intent", action, uri] => Request::Intent { action: action.to_string(), uri: uri.to_string() },
        ["prop", "get", key] => shell(vec!["getprop".into(), key.to_string()], None, false),
        ["prop", "set", key, value] => Request::SetProp { key: key.to_string(), value: value.to_string() },
        _ => usage(),
    };
    match call(&state_dir(), &req) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "airdroid: {e}");
            std::process::exit(1)
        }
    }
}
