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
/// * no on-screen keyboard while there is a hardware one,
/// * in multi-window mode, no crash or "not responding" dialogs: they belong
///   to no app's task, so (as in Waydroid) no window shows them, but they
///   would take the input focus.
pub fn prepare_desktop() {
    let multi = exec::getprop("persist.waydroid.multi_windows") == "true";
    let _ = shell_user(&["/system/bin/settings", "put", "global", "hide_error_dialogs", if multi { "1" } else { "0" }]);
    for argv in [
        &["/system/bin/settings", "put", "system", "screen_off_timeout", "2147483647"][..],
        &["/system/bin/svc", "power", "stayon", "true"],
        &["/system/bin/settings", "put", "secure", "show_ime_with_hard_keyboard", "0"],
        &["/system/bin/locksettings", "set-disabled", "true"],
    ] {
        let _ = shell_user(argv);
    }
    mount_share();
    wake();
}

/// Android follows the host's time zone (`tz`, an Olson ID; empty: leave it).
pub fn follow_timezone(tz: &str) {
    if tz.is_empty() || exec::getprop("persist.sys.timezone") == tz {
        return;
    }
    match shell_user(&["/system/bin/cmd", "alarm", "set-timezone", tz]) {
        Ok(_) => logf!("time zone: {tz}"),
        Err(e) => logf!("time zone {tz}: {e}"),
    }
}

/// Android stays provisioned. Its settings files can come back empty when
/// the host goes down while they are being written (a power cut; Android
/// then starts them afresh), and Waydroid's image provisions the device on
/// its first boot only: without `device_provisioned` Settings exits at
/// once and Home and notifications are off. A marker on the data disk says
/// that this Android was provisioned before.
pub fn stay_provisioned() {
    let marker = format!("{}/provisioned", crate::container::DATA_DISK);
    let get = |ns: &str, key: &str| shell_user(&["/system/bin/settings", "get", ns, key]).map(|v| v.trim().to_string()).unwrap_or_default();
    let global = get("global", "device_provisioned");
    let secure = get("secure", "user_setup_complete");
    if global == "1" && secure == "1" {
        if !std::path::Path::new(&marker).exists() {
            let _ = std::fs::write(&marker, "");
        }
        return;
    }
    if !std::path::Path::new(&marker).exists() || global.is_empty() {
        // the first boot (Android provisions itself), or no answer
        return;
    }
    logf!("Android lost its settings (device_provisioned {global}, user_setup_complete {secure}): provisioning it again");
    let _ = shell_user(&["/system/bin/settings", "put", "global", "device_provisioned", "1"]);
    let _ = shell_user(&["/system/bin/settings", "put", "secure", "user_setup_complete", "1"]);
}

/// The host's shared folder (virtio-9p "airdroid_share"), as /sdcard/airOS:
/// mounted in Android's own mount namespace over /data/media/0/airOS, the
/// storage behind /sdcard, so every app's view of it shows the files.
fn mount_share() {
    let has_share = std::fs::read_dir("/sys/bus/virtio/drivers/9pnet_virtio")
        .map(|d| {
            d.flatten().any(|e| std::fs::read_to_string(e.path().join("mount_tag")).is_ok_and(|t| t.trim_end_matches('\0').trim() == "airdroid_share"))
        })
        .unwrap_or(false);
    if !has_share {
        return;
    }
    // Android's storage (MediaProvider, behind /sdcard) works on these
    // files as group media_rw: the share's files become media_rw's, group
    // writable (on air/OS owners mean nothing; elsewhere QEMU keeps them in
    // extended attributes). Files added on the desktop later stay readable.
    let dir = "/data/media/0/airOS";
    let script = format!(
        "mkdir -p {dir} && \
         (grep -q ' {dir} ' /proc/mounts || mount -t 9p -o trans=virtio,version=9p2000.L,msize=512000,access=client,cache=none airdroid_share {dir}) && \
         chown -R media_rw:media_rw {dir} && chmod -R ug+rwX {dir}"
    );
    match exec::run(&["/system/bin/sh", "-c", &script], true, &[]) {
        Ok((0, _, _)) => logf!("shared folder: /sdcard/airOS"),
        Ok((code, _, err)) => logf!("shared folder: mount failed ({code}): {}", String::from_utf8_lossy(&err).trim()),
        Err(e) => logf!("shared folder: {e}"),
    }
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
            // as the shell user: as root, pm's AppOps check finds no calling
            // package and throws
            let out = shell_user(&["/system/bin/pm", "uninstall", &package])?;
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

/// Re-sends the app list when the installed launcher apps change, or
/// when their icons do (Android writes a new app's icon a little after the
/// app appears).
pub fn watch_packages(link: HostLink) {
    std::thread::spawn(move || {
        let state = || {
            let comps = launcher_components();
            let icons = format!("{}/data/icons", container::DATA_DISK);
            let have: Vec<bool> = comps
                .iter()
                .map(|c| std::path::Path::new(&format!("{icons}/{}.png", c.split('/').next().unwrap_or(c))).exists())
                .collect();
            (comps, have)
        };
        let mut last = state();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if !exec::android_running() {
                return;
            }
            let now = state();
            if now != last && !now.0.is_empty() {
                last = now;
                list(&link);
            }
        }
    });
}

/// Multi-window: gives `package`'s visible freeform task the bounds
/// (left, top, width, height), as the desktop window was resized.
pub fn resize_task(package: String, x: i32, y: i32, w: u32, h: u32) {
    std::thread::spawn(move || {
        let list = match shell_user(&["am", "stack", "list"]) {
            Ok(l) => l,
            Err(e) => return logf!("resize {package}: {e}"),
        };
        // "  taskId=23: com.android.settings/... bounds=[..] userId=0 visible=true ..."
        let task = list.lines().map(str::trim).find_map(|l| {
            let rest = l.strip_prefix("taskId=")?;
            let (id, rest) = rest.split_once(": ")?;
            (rest.starts_with(&format!("{package}/")) && rest.contains("visible=true")).then(|| id.to_string())
        });
        let Some(task) = task else {
            return logf!("resize {package}: no visible task");
        };
        let (r, b) = ((x + w as i32).to_string(), (y + h as i32).to_string());
        if let Err(e) = shell_user(&["am", "task", "resize", &task, &x.to_string(), &y.to_string(), &r, &b]) {
            logf!("resize {package}: {e}");
        }
    });
}
