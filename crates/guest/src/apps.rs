//! App management: what `waydroid app ...` does, done with Android's own
//! shell tools inside the container.

use crate::container;
use crate::exec;
use crate::hostlink::HostLink;
use crate::logf;
use airdroid_proto::{AppInfo, ToHost};
use std::time::Duration;

pub enum Action {
    Install(String),
    Remove(String),
    Launch(String),
    FullUi,
    Intent(String, String),
    SetProp(String, String),
    List,
}

pub fn spawn(link: HostLink, req: u32, action: Action) {
    std::thread::spawn(move || {
        let result = run(&link, action);
        if req != 0 {
            match result {
                Ok(message) => link.send(&ToHost::Reply { req, ok: true, message }),
                Err(message) => link.send(&ToHost::Reply { req, ok: false, message }),
            }
        } else if let Err(e) = result {
            logf!("{e}");
        }
    });
}

fn sh(argv: &[&str]) -> Result<String, String> {
    if !exec::android_running() {
        return Err("Android is not running".into());
    }
    exec::android_output(argv).map_err(|e| e.to_string())
}

/// As Android's shell user: `am` answers root with a silent -1.
fn shell_user(argv: &[&str]) -> Result<String, String> {
    if !exec::android_running() {
        return Err("Android is not running".into());
    }
    exec::android_output_as(argv, Some(exec::AID_SHELL)).map_err(|e| e.to_string())
}

/// `package/class` of the app's launcher activity.
fn launcher_activity(package: &str) -> Result<String, String> {
    let out = sh(&["/system/bin/cmd", "package", "resolve-activity", "--brief", "-a", "android.intent.action.MAIN", "-c", "android.intent.category.LAUNCHER", package])?;
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains('/'))
        .map(str::to_string)
        .ok_or_else(|| format!("{package} has no launcher activity"))
}

/// Android set up for a desktop, after each boot:
/// * the display stays on (air/OS does screen saving; Android's own timeout
///   would make its windows go black),
/// * no lock screen, and the notification shade collapsed: in multi-window
///   mode neither is shown, but either takes the input focus,
/// * no on-screen keyboard while there is a hardware one.
pub fn prepare_desktop() {
    for argv in [
        &["/system/bin/settings", "put", "system", "screen_off_timeout", "2147483647"][..],
        &["/system/bin/svc", "power", "stayon", "true"],
        &["/system/bin/settings", "put", "secure", "show_ime_with_hard_keyboard", "0"],
        &["/system/bin/locksettings", "set-disabled", "true"],
    ] {
        let _ = shell_user(argv);
    }
    wake();
}

/// Brings Android's focus to the apps: wakes the display, dismisses the
/// keyguard and collapses the (invisible) notification shade.
fn wake() {
    let _ = shell_user(&["/system/bin/input", "keyevent", "KEYCODE_WAKEUP"]);
    let _ = shell_user(&["/system/bin/wm", "dismiss-keyguard"]);
    let _ = shell_user(&["/system/bin/cmd", "statusbar", "collapse"]);
}

fn run(link: &HostLink, action: Action) -> Result<String, String> {
    if matches!(action, Action::Launch(_) | Action::FullUi | Action::Intent(..)) {
        wake();
    }
    match action {
        Action::Install(path) => {
            let out = sh(&["/system/bin/pm", "install", "-r", "-g", &path])?;
            let _ = std::fs::remove_file(format!("{}/{}", container::DATA_DISK, path.trim_start_matches('/')));
            list(link);
            Ok(out)
        }
        Action::Remove(package) => {
            let out = sh(&["/system/bin/pm", "uninstall", &package])?;
            list(link);
            Ok(out)
        }
        Action::Launch(package) => {
            let activity = launcher_activity(&package)?;
            let _ = exec::setprop("waydroid.active_apps", &package);
            let r = shell_user(&["/system/bin/am", "start", "-n", &activity, "-f", "0x10000000"]);
            // SystemUI's shade takes the focus again as the app comes up
            std::thread::sleep(Duration::from_millis(500));
            let _ = shell_user(&["/system/bin/cmd", "statusbar", "collapse"]);
            r
        }
        Action::FullUi => {
            let _ = exec::setprop("waydroid.active_apps", "Waydroid");
            shell_user(&["/system/bin/am", "start", "-a", "android.intent.action.MAIN", "-c", "android.intent.category.HOME"])
        }
        Action::Intent(action, uri) => {
            let mut argv = vec!["/system/bin/am", "start", "-a", action.as_str(), "-f", "0x10000000"];
            if !uri.is_empty() {
                argv.push("-d");
                argv.push(uri.as_str());
            }
            shell_user(&argv)
        }
        Action::SetProp(k, v) => exec::setprop(&k, &v).map(|_| String::new()).map_err(|e| e.to_string()),
        Action::List => {
            list(link);
            Ok(String::new())
        }
    }
}

fn launcher_components() -> Vec<String> {
    let Ok(out) = sh(&["/system/bin/cmd", "package", "query-activities", "--brief", "-a", "android.intent.action.MAIN", "-c", "android.intent.category.LAUNCHER"]) else {
        return Vec::new();
    };
    let mut v: Vec<String> = out.lines().map(str::trim).filter(|l| l.contains('/') && !l.contains(' ')).map(str::to_string).collect();
    v.sort();
    v.dedup();
    v
}

/// Names of launchable apps from Waydroid's platform service. The call
/// runs on its own thread with a deadline: a slow or wedged service must
/// not hold up the app list (it then falls back to package names).
fn app_names() -> std::collections::HashMap<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::platform::Platform::connect().and_then(|p| p.apps()));
    });
    let mut names = std::collections::HashMap::new();
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(apps)) => {
            for a in apps {
                if !a.name.is_empty() {
                    names.insert(a.package, a.name);
                }
            }
        }
        Ok(Err(e)) => logf!("waydroidplatform: {e}"),
        Err(_) => logf!("waydroidplatform: no answer in 10 s"),
    }
    names
}

fn list(link: &HostLink) {
    let names = app_names();
    let comps = launcher_components();
    logf!("{} launcher apps, {} names", comps.len(), names.len());
    let icons = format!("{}/data/icons", container::DATA_DISK);
    let apps: Vec<AppInfo> = comps
        .iter()
        .map(|c| {
            let package = c.split('/').next().unwrap_or(c).to_string();
            AppInfo {
                name: names.get(&package).cloned().unwrap_or_else(|| package.clone()),
                icon_png: std::fs::read(format!("{icons}/{package}.png")).ok(),
                package,
                activity: c.clone(),
            }
        })
        .collect();
    link.send(&ToHost::Apps { apps });
}

/// Re-sends the app list when the installed launcher apps change.
pub fn watch_packages(link: HostLink) {
    std::thread::spawn(move || {
        let mut last = launcher_components();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if !exec::android_running() {
                return;
            }
            let now = launcher_components();
            if now != last && !now.is_empty() {
                last = now;
                list(&link);
            }
        }
    });
}
