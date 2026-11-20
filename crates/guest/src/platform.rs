//! Waydroid's platform service inside Android ("waydroidplatform",
//! lineageos.waydroid.IPlatform): app lists with names, installs and
//! launches, as `waydroid app ...` uses it (tools/interfaces/IPlatform.py).

use crate::binder::{Binder, Parcel};
use std::io;

const SERVICE: &str = "waydroidplatform";
const INTERFACE: &str = "lineageos.waydroid.IPlatform";
const BINDER_DEVICE: &str = "/dev/binderfs/anbox-binder";

const GET_APPS_INFO: u32 = 3;
const INSTALL_APP: u32 = 5;
const REMOVE_APP: u32 = 6;
const LAUNCH_APP: u32 = 7;

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    pub package: String,
    pub component_package: String,
    pub component_class: String,
    pub categories: Vec<String>,
}

pub struct Platform {
    binder: Binder,
    handle: u32,
}

impl Platform {
    pub fn connect() -> io::Result<Platform> {
        let binder = Binder::open(BINDER_DEVICE)?;
        let handle = binder.check_service(SERVICE)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no waydroidplatform service"))?;
        Ok(Platform { binder, handle })
    }

    pub fn apps(&self) -> io::Result<Vec<App>> {
        let mut r = self.binder.transact(self.handle, GET_APPS_INFO, &Parcel::interface(INTERFACE))?;
        r.status()?;
        let n = r.i32()?;
        let mut apps = Vec::new();
        for _ in 0..n.max(0) {
            if r.i32()? != 1 {
                continue; // null entry
            }
            let name = r.string16()?.unwrap_or_default();
            let package = r.string16()?.unwrap_or_default();
            let _action = r.string16()?;
            let _launch_intent = r.string16()?;
            let component_package = r.string16()?.unwrap_or_default();
            let component_class = r.string16()?.unwrap_or_default();
            let nc = r.i32()?;
            let mut categories = Vec::new();
            for _ in 0..nc.max(0) {
                categories.push(r.string16()?.unwrap_or_default());
            }
            apps.push(App { name, package, component_package, component_class, categories });
        }
        Ok(apps)
    }

    fn call_string(&self, code: u32, arg: &str) -> io::Result<()> {
        let mut p = Parcel::interface(INTERFACE);
        p.string16(Some(arg));
        let mut r = self.binder.transact(self.handle, code, &p)?;
        r.status()
    }

    pub fn install(&self, path: &str) -> io::Result<()> {
        self.call_string(INSTALL_APP, path)
    }

    pub fn remove(&self, package: &str) -> io::Result<()> {
        self.call_string(REMOVE_APP, package)
    }

    pub fn launch(&self, package: &str) -> io::Result<()> {
        self.call_string(LAUNCH_APP, package)
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        self.binder.release(self.handle);
    }
}
