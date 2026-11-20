//! Waydroid's notification service ("waydroidnotifications",
//! lineageos.waydroid.INotifications), served from the VM: Android posts
//! its notifications to it and airDroid shows them on the air/OS desktop
//! (tools/services/notification_manager.py in Waydroid).

use crate::binder::{Binder, Parcel, Reply};
use crate::hostlink::HostLink;
use crate::logf;
use airdroid_proto::{NotifyImage, ToHost};
use std::io;

const SERVICE: &str = "waydroidnotifications";
const REGISTER_LISTENER: u32 = 1;
const NOTIFY: u32 = 2;
const CLOSE_NOTIFICATION: u32 = 3;
/// Parcelable null marker.
const NULL_PARCELABLE: i32 = 0;

fn notify(r: &mut Reply, next_id: &mut u32, link: &HostLink) -> io::Result<u32> {
    r.rpc_header()?;
    let replaces_id = r.i32()? as u32;
    let app_name = r.string16()?.unwrap_or_default();
    let package = r.string16()?.unwrap_or_default();
    let summary = r.string16()?.unwrap_or_default();
    let body = r.string16()?.unwrap_or_default();
    let n_actions = r.i32()?;
    for _ in 0..n_actions.max(0) {
        if r.i32()? != NULL_PARCELABLE {
            let _size = r.i32()?;
            r.string16()?;
            r.string16()?;
        }
    }
    let mut image = None;
    if r.i32()? != NULL_PARCELABLE {
        let _size = r.i32()?;
        let width = r.i32()? as u32;
        let height = r.i32()? as u32;
        let rowstride = r.i32()? as u32;
        let has_alpha = r.bool()?;
        let data = r.bytes()?;
        image = Some(NotifyImage { width, height, rowstride, has_alpha, data });
    }
    let id = if replaces_id != 0 {
        replaces_id
    } else {
        *next_id += 1;
        *next_id
    };
    link.send(&ToHost::Notify { id, app_name, package, title: summary, body, image });
    Ok(id)
}

pub fn start(link: HostLink) {
    std::thread::Builder::new()
        .name("notifications".into())
        .spawn(move || {
            let binder = match Binder::open("/dev/binderfs/anbox-binder") {
                Ok(b) => b,
                Err(e) => return logf!("notifications: {e}"),
            };
            // Android looks the service up once, early in system_server's
            // start: register as soon as its servicemanager answers
            let start = std::time::Instant::now();
            loop {
                match binder.add_service(SERVICE, 1) {
                    Ok(()) => break,
                    Err(e) if start.elapsed() > std::time::Duration::from_secs(120) => {
                        return logf!("notifications: addService: {e}");
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            }
            logf!("notifications: {SERVICE} registered");
            let mut next_id = 0u32;
            let r = binder.serve(|_cookie, code, req| {
                let mut reply = Parcel::default();
                match code {
                    REGISTER_LISTENER => reply.i32(0),
                    NOTIFY => match notify(req, &mut next_id, &link) {
                        Ok(id) => {
                            reply.i32(0);
                            reply.i32(id as i32);
                        }
                        Err(e) => {
                            logf!("notifications: bad notify: {e}");
                            return None;
                        }
                    },
                    CLOSE_NOTIFICATION => {
                        let _ = req.rpc_header();
                        if let Ok(id) = req.i32() {
                            link.send(&ToHost::NotifyClose { id: id as u32 });
                        }
                        reply.i32(0);
                    }
                    _ => return None,
                }
                Some(reply)
            });
            if let Err(e) = r {
                logf!("notifications: {e}");
            }
        })
        .ok();
}
