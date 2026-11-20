//! The engine thread: first-run setup, the VM and its session, Android's
//! windows (composed from guest memory), input, apps and launchers.

use crate::host::{self, action, kind, Event};
use crate::keys;
use crate::paths::{Paths, Resources};
use crate::settings::Settings;
use airdroid_core::compose::{content_bounds, opaque_bounds, Buffer, Canvas};
use airdroid_core::proto::Rect;
use airdroid_core::guestmem::GuestMem;
use airdroid_core::images;
use airdroid_core::proto::{AppInfo, BootStage, ToGuest, ToHost, TouchKind};
use airdroid_core::control;
use airdroid_core::session::Session;
use std::os::unix::net::{UnixListener, UnixStream};
use airdroid_core::vm::{Accel, RamBackend, VmConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Msg {
    Host(Event),
    /// A request from the `airdroid` command and the connection to answer on.
    Control(control::Request, UnixStream),
    Guest(ToHost),
    GuestGone,
    SetupProgress(String, f32),
    SetupDone(Result<(), String>),
    /// `airdroid upgrade` finished: (message, images changed)
    UpgradeDone(Result<bool, String>, Option<UnixStream>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    NotInstalled,
    Downloading,
    Stopped,
    Starting,
    Booted,
    Stopping,
}

/// Things asked for before Android was up.
enum Pending {
    Launch(String),
    Install(PathBuf),
    FullUi,
}

struct Win {
    canvas: Canvas,
    /// opaque bounds, recomputed when the layer geometry changes
    crop: Option<(Vec<(Rect, Rect)>, Option<Rect>)>,
    title: String,
    /// The part of the Android window shown (multi-window: the app's task
    /// inside a display-sized surface); None until something is visible,
    /// and the BWindow opens then.
    shown: Option<Rect>,
    /// bounds seen before the window opened (it opens once they hold for
    /// two frames: an app's first frame can sit at the display's corner)
    candidate: Option<Rect>,
    buttons: i32,
    touching: bool,
}

impl Win {
    /// Host window coordinates to Android window coordinates.
    fn to_android(&self, x: f32, y: f32) -> (f32, f32) {
        let (ox, oy) = self.shown.map_or((0, 0), |r| (r.x, r.y));
        (x + ox as f32, y + oy as f32)
    }
}

pub struct Engine {
    paths: Paths,
    settings: Settings,
    tx: Sender<Msg>,
    phase: Phase,
    vm: Option<VmConfig>,
    child: Option<Child>,
    session: Option<Session>,
    mem: Option<Arc<GuestMem>>,
    buffers: HashMap<u32, Buffer>,
    windows: HashMap<u32, Win>,
    pending: Vec<Pending>,
    frames_done: Vec<(u32, u32)>,
    next_vsync: Instant,
    modifiers: u32,
    apps: Vec<AppInfo>,
    installs: HashMap<u32, String>,
    app_path: String,
    quitting: bool,
    epoch: Instant,
    cancel_setup: Arc<AtomicBool>,
    /// open audio streams (the host plays them; credits go back)
    audio: Vec<u32>,
    mics: HashMap<u32, Mic>,
    /// `airdroid` commands waiting for a guest answer, by request id
    waiters: HashMap<u32, UnixStream>,
    restart_after_stop: bool,
}

const FRAME: Duration = Duration::from_micros(16_667);

impl Engine {
    pub fn new(tx: Sender<Msg>) -> Engine {
        let paths = Paths::new();
        let settings = Settings::load(&paths.settings);
        Engine {
            app_path: host::app_path().unwrap_or_else(|| "/boot/system/apps/airDroid".into()),
            paths,
            settings,
            tx,
            phase: Phase::Stopped,
            vm: None,
            child: None,
            session: None,
            mem: None,
            buffers: HashMap::new(),
            windows: HashMap::new(),
            pending: Vec::new(),
            frames_done: Vec::new(),
            next_vsync: Instant::now(),
            modifiers: 0,
            apps: Vec::new(),
            installs: HashMap::new(),
            quitting: false,
            epoch: Instant::now(),
            cancel_setup: Arc::new(AtomicBool::new(false)),
            audio: Vec::new(),
            mics: HashMap::new(),
            waiters: HashMap::new(),
            restart_after_stop: false,
        }
    }

    fn status(&self, text: &str) {
        host::main_set_status(text, -1.0);
    }

    // ---- lifecycle -----------------------------------------------------

    pub fn run(mut self, rx: Receiver<Msg>, args: Vec<String>) {
        host::main_set_multiwindow(self.settings.multi_windows);
        self.start_control();
        self.handle_args(&args);
        if images::installed(&self.paths.images()).is_some() {
            self.start_vm();
        } else {
            self.phase = Phase::NotInstalled;
            self.start_setup();
        }
        loop {
            let timeout = self.next_vsync.saturating_duration_since(Instant::now());
            // audio credits need a short tick while streams are open
            let idle = if self.audio.is_empty() && self.mics.is_empty() { Duration::from_millis(250) } else { Duration::from_millis(10) };
            let msg = if self.frames_done.is_empty() { rx.recv_timeout(idle) } else { rx.recv_timeout(timeout.min(idle)) };
            match msg {
                Ok(Msg::Host(ev)) => self.host_event(ev),
                Ok(Msg::Control(req, conn)) => self.control(req, conn),
                Ok(Msg::Guest(m)) => self.guest_message(m),
                Ok(Msg::GuestGone) => self.vm_gone(),
                Ok(Msg::SetupProgress(text, p)) => host::main_set_status(&text, p),
                Ok(Msg::SetupDone(r)) => self.setup_done(r),
                Ok(Msg::UpgradeDone(r, conn)) => self.upgrade_done(r, conn),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.vsync();
            self.audio_credits();
            self.microphones();
            if self.quitting && self.child.is_none() {
                break;
            }
        }
        host::quit();
    }

    /// The control socket for the `airdroid` command.
    fn start_control(&self) {
        let _ = std::fs::create_dir_all(&self.paths.state);
        let path = control::socket_path(&self.paths.state);
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("airDroid: control socket {}: {e}", path.display());
                return;
            }
        };
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("airDroid control".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    let mut c = conn;
                    use std::io::Read;
                    while c.read_exact(&mut byte).is_ok() && byte[0] != b'\n' && line.len() < 1 << 20 {
                        line.push(byte[0]);
                    }
                    match serde_json::from_slice::<control::Request>(&line) {
                        Ok(req) => {
                            if tx.send(Msg::Control(req, c)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = control::write_frame(&mut c, control::ERR, format!("airDroid: bad request: {e}\n").as_bytes());
                            let _ = control::write_frame(&mut c, control::EXIT, &2i32.to_le_bytes());
                        }
                    }
                }
            })
            .ok();
    }

    fn control(&mut self, req: control::Request, mut conn: UnixStream) {
        use control::Request as R;
        let finish = |conn: &mut UnixStream, text: &str, code: i32| {
            let kind = if code == 0 { control::OUT } else { control::ERR };
            if !text.is_empty() {
                let _ = control::write_frame(conn, kind, format!("{}\n", text.trim_end()).as_bytes());
            }
            let _ = control::write_frame(conn, control::EXIT, &code.to_le_bytes());
        };
        match req {
            R::Status => {
                let text = match self.phase {
                    Phase::NotInstalled => "Android is not installed".to_string(),
                    Phase::Downloading => "Downloading Android".to_string(),
                    Phase::Stopped => "Android is stopped".to_string(),
                    Phase::Starting => "Android is starting".to_string(),
                    Phase::Booted => format!("Android is running ({} apps, {} windows)", self.apps.len(), self.windows.len()),
                    Phase::Stopping => "Android is stopping".to_string(),
                };
                finish(&mut conn, &text, 0);
            }
            R::List => {
                let mut out = String::new();
                for a in &self.apps {
                    out.push_str(&format!("{}\t{}\n", a.package, display_name(a)));
                }
                finish(&mut conn, &out, 0);
            }
            R::Stop => {
                self.stop_vm();
                finish(&mut conn, "", 0);
            }
            R::Upgrade => {
                if matches!(self.phase, Phase::Downloading | Phase::NotInstalled) {
                    return finish(&mut conn, "Android is not installed yet", 1);
                }
                let _ = control::write_frame(&mut conn, control::OUT, b"Checking for newer Android images...\n");
                let tx = self.tx.clone();
                let dir = self.paths.images();
                let flavour = self.settings.flavour.clone();
                let cancel = self.cancel_setup.clone();
                std::thread::spawn(move || {
                    let mut last = Instant::now();
                    let tx2 = tx.clone();
                    let mut progress = |what: &str, done: u64, total: u64| {
                        if last.elapsed() > Duration::from_millis(500) {
                            last = Instant::now();
                            let p = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                            let _ = tx2.send(Msg::SetupProgress(format!("Updating Android: {what}"), p));
                        }
                    };
                    let r = images::upgrade(&dir, images::Arch::host(), &flavour, &mut progress, &cancel)
                        .map(|n| n.is_some())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Msg::UpgradeDone(r, Some(conn)));
                });
            }
            R::Launch { package } if self.phase != Phase::Booted => {
                self.request(Pending::Launch(package));
                finish(&mut conn, "Android is starting; the app opens when it is up.", 0);
            }
            R::Install { path } if self.phase != Phase::Booted => {
                self.request(Pending::Install(PathBuf::from(path)));
                finish(&mut conn, "Android is starting; the app is installed when it is up.", 0);
            }
            _ if self.phase != Phase::Booted => finish(&mut conn, "Android is not running", 1),
            R::Install { path } => {
                if let Some(req) = self.request(Pending::Install(PathBuf::from(path))) {
                    self.waiters.insert(req, conn);
                }
            }
            req => {
                let Some(s) = self.session.clone() else { return finish(&mut conn, "Android is not running", 1) };
                let id = s.next_req();
                let msg = match req {
                    R::Shell { argv, uid, vm } => {
                        let argv = if argv.is_empty() { vec!["/system/bin/sh".into(), "-c".into(), "echo use: airdroid shell COMMAND".into()] } else { argv };
                        ToGuest::Exec { req: id, argv, android: !vm, uid, stdin: Vec::new() }
                    }
                    R::Remove { package } => ToGuest::RemoveApp { req: id, package },
                    R::Launch { package } => ToGuest::LaunchApp { req: id, package },
                    R::Intent { action, uri } => ToGuest::LaunchIntent { req: id, action, uri },
                    R::ShowFullUi => ToGuest::ShowFullUi { req: id },
                    R::SetProp { key, value } => ToGuest::SetProp { req: id, key, value },
                    _ => return,
                };
                if s.send(&msg).is_ok() {
                    self.waiters.insert(id, conn);
                } else {
                    finish(&mut conn, "the VM went away", 1);
                }
            }
        }
    }

    fn start_setup(&mut self) {
        if self.phase == Phase::Downloading {
            return;
        }
        self.phase = Phase::Downloading;
        host::main_set_status("Downloading Android (LineageOS 20, about 1 GB)…", 0.0);
        let tx = self.tx.clone();
        let dir = self.paths.images();
        let flavour = self.settings.flavour.clone();
        let cancel = self.cancel_setup.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut progress = |what: &str, done: u64, total: u64| {
                if last.elapsed() < Duration::from_millis(200) && done < total {
                    return;
                }
                last = Instant::now();
                let p = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                let mb = |b: u64| b as f64 / 1_048_576.0;
                let _ = tx.send(Msg::SetupProgress(format!("{what}: {:.0} of {:.0} MB", mb(done), mb(total)), p));
            };
            let r = images::setup(&dir, images::Arch::host(), &flavour, &mut progress, &cancel).map(|_| ()).map_err(|e| e.to_string());
            let _ = tx.send(Msg::SetupDone(r));
        });
    }

    fn setup_done(&mut self, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.phase = Phase::Stopped;
                self.start_vm();
            }
            Err(e) => {
                self.phase = Phase::NotInstalled;
                host::main_set_status(&format!("Could not download Android: {e}"), -1.0);
            }
        }
    }

    fn upgrade_done(&mut self, r: Result<bool, String>, conn: Option<UnixStream>) {
        let (text, code) = match r {
            Ok(true) => ("Android was updated; it restarts now.".to_string(), 0),
            Ok(false) => ("Android is up to date.".to_string(), 0),
            Err(e) => (format!("Updating Android failed: {e}"), 1),
        };
        host::main_set_status(&text, -1.0);
        if let Some(mut c) = conn {
            let _ = control::write_frame(&mut c, if code == 0 { control::OUT } else { control::ERR }, format!("{text}\n").as_bytes());
            let _ = control::write_frame(&mut c, control::EXIT, &(code as i32).to_le_bytes());
        }
        if text.starts_with("Android was updated") && self.session.is_some() {
            // restart on the new images once the old VM is gone
            self.restart_after_stop = true;
            self.stop_vm();
        }
    }

    fn start_vm(&mut self) {
        if matches!(self.phase, Phase::Starting | Phase::Booted | Phase::Downloading) {
            return;
        }
        let res = match Resources::find(Some(std::path::Path::new(&self.app_path))) {
            Ok(r) => r,
            Err(e) => {
                self.status(&format!("airDroid is incomplete: {e}"));
                return;
            }
        };
        if !res.qemu_shares_ram {
            self.status("Warning: using linuxvm's QEMU; Android's screen cannot be shown.");
        }
        let _ = std::fs::create_dir_all(&self.paths.state);
        let _ = std::fs::create_dir_all(&self.paths.data);
        let userdata = self.paths.userdata();
        if !userdata.exists() {
            if let Err(e) = airdroid_core::qcow2::create(&userdata, 64 << 30) {
                self.status(&format!("Cannot create Android's data disk: {e}"));
                return;
            }
        }
        let (sw, sh) = host::screen_size();
        let (width, height) = if self.settings.multi_windows {
            // Android's display is the whole screen; each app a window in it
            (sw, sh)
        } else {
            (self.settings.width.min(sw), self.settings.height.min(sh))
        };
        // GPU: virgl when QEMU has it and a Mesa with EGL can drive a GPU
        let qemu_gl = res.qemu.parent().map(|d| d.join("lib/libvirglrenderer.so.1").exists()).unwrap_or(false);
        let gl = if qemu_gl { crate::paths::gl_environment() } else { None };
        let (gpu, env) = match (self.settings.gpu.as_str(), gl) {
            ("none", _) | (_, None) => ("none".to_string(), Vec::new()),
            ("virgl", Some((env, _))) => ("virgl".to_string(), env),
            (_, Some((env, true))) => ("virgl".to_string(), env),
            (_, Some(_)) => ("none".to_string(), Vec::new()), // llvmpipe only: SwiftShader is faster
        };
        let images = self.paths.images();
        let cfg = VmConfig {
            qemu: res.qemu,
            firmware: res.firmware,
            kernel: res.kernel,
            initrd: res.initrd,
            system_img: images.join("system.img"),
            vendor_img: images.join("vendor.img"),
            userdata,
            state_dir: self.paths.state.clone(),
            ram: RamBackend::for_platform(&self.paths.state),
            ram_mb: self.settings.ram_mb,
            cpus: self.settings.cpus,
            accel: Accel::detect(),
            width,
            height,
            dpi: self.settings.dpi,
            multi_windows: self.settings.multi_windows,
            gpu,
            env,
        };
        if cfg.accel == Accel::Tcg {
            self.status("Starting Android (no hardware virtualization: this will be slow)…");
        } else {
            self.status("Starting Android…");
        }
        let child = match cfg.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.status(&format!("Cannot start the Android VM: {e}"));
                return;
            }
        };
        let (session, rx) = match Session::connect(&cfg.socket(), Duration::from_secs(30)) {
            Ok(v) => v,
            Err(e) => {
                self.status(&format!("The Android VM did not come up: {e} (see {})", cfg.qemu_log().display()));
                return;
            }
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for m in rx {
                if tx.send(Msg::Guest(m)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Msg::GuestGone);
        });
        self.child = Some(child);
        self.session = Some(session);
        self.vm = Some(cfg);
        self.phase = Phase::Starting;
        self.epoch = Instant::now();
    }

    fn stop_vm(&mut self) {
        if let Some(s) = &self.session {
            self.phase = Phase::Stopping;
            self.status("Stopping Android…");
            let _ = s.send(&ToGuest::Shutdown);
            // QEMU exits when the guest powers off; don't wait forever
            let child = self.child.take();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                if let Some(mut c) = child {
                    for _ in 0..150 {
                        if let Ok(Some(_)) = c.try_wait() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = tx.send(Msg::GuestGone);
            });
        } else if self.quitting {
            self.child = None;
        }
    }

    fn vm_gone(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.session = None;
        self.mem = None;
        self.buffers.clear();
        for id in self.windows.keys().copied().collect::<Vec<_>>() {
            host::window_close(id);
        }
        self.windows.clear();
        self.frames_done.clear();
        for (stream, m) in self.mics.drain() {
            if m.native {
                host::mic_close(stream);
            }
        }
        for stream in self.audio.drain(..) {
            host::audio_close(stream);
        }
        if self.phase != Phase::NotInstalled {
            self.phase = Phase::Stopped;
        }
        self.status("Android is stopped.");
        if std::mem::take(&mut self.restart_after_stop) && !self.quitting {
            self.start_vm();
        }
    }

    // ---- requests --------------------------------------------------------

    fn handle_args(&mut self, args: &[String]) {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--launch" | "-l" => {
                    if let Some(p) = it.next() {
                        self.request(Pending::Launch(p.clone()));
                    }
                }
                "--full-ui" => {
                    self.request(Pending::FullUi);
                }
                "--install" | "-i" => {
                    if let Some(p) = it.next() {
                        self.request(Pending::Install(PathBuf::from(p)));
                    }
                }
                "--stop" => {
                    self.stop_vm();
                }
                p if p.ends_with(".apk") => {
                    self.request(Pending::Install(PathBuf::from(p)));
                }
                _ => {}
            }
        }
    }

    /// Sends a request now, or queues it until Android is up; the id of an
    /// install (its Reply comes when it is done).
    fn request(&mut self, p: Pending) -> Option<u32> {
        if self.phase != Phase::Booted {
            self.pending.push(p);
            if self.phase == Phase::Stopped {
                self.start_vm();
            }
            return None;
        }
        let Some(s) = self.session.clone() else { return None };
        match p {
            Pending::Launch(package) => {
                let _ = s.send(&ToGuest::LaunchApp { req: s.next_req(), package });
                None
            }
            Pending::FullUi => {
                let _ = s.send(&ToGuest::ShowFullUi { req: s.next_req() });
                None
            }
            Pending::Install(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "app.apk".into());
                let guest = format!("/data/waydroid_tmp/{}", name.replace(' ', "_"));
                self.status(&format!("Installing {name}…"));
                // the copy runs on its own thread: APKs can be large
                let tx = self.tx.clone();
                let req = s.next_req();
                self.installs.insert(req, name.clone());
                std::thread::spawn(move || match s.put_file(&path, &guest) {
                    Ok(_) => {
                        let _ = s.send(&ToGuest::InstallApk { req, path: guest });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Guest(ToHost::Reply { req, ok: false, message: e.to_string() }));
                    }
                });
                Some(req)
            }
        }
    }

    // ---- guest -------------------------------------------------------------

    fn guest_message(&mut self, m: ToHost) {
        match m {
            ToHost::Hello { .. } => {
                if let Some(cfg) = &self.vm {
                    match cfg.open_guest_mem() {
                        Ok(mem) => self.mem = Some(Arc::new(mem)),
                        Err(e) => {
                            eprintln!("airDroid: guest memory: {e}");
                            self.status(&format!("Cannot see Android's memory: {e}"))
                        }
                    }
                }
            }
            ToHost::Log { text } if debug() => eprintln!("airDroid: guest: {text}"),
            ToHost::Status { stage, detail } => match stage {
                BootStage::Init | BootStage::StartingAndroid => self.status("Starting Android…"),
                BootStage::FormattingData => self.status("Preparing Android's storage (first start)…"),
                BootStage::Booted => {
                    self.phase = Phase::Booted;
                    self.status(&format!("Android is running ({:.0} s to start).", self.epoch.elapsed().as_secs_f32()));
                    for p in std::mem::take(&mut self.pending) {
                        self.request(p);
                    }
                }
                BootStage::Stopped => self.status("Android stopped."),
                BootStage::Failed => self.status(&format!("Android failed to start: {detail}")),
            },
            ToHost::Log { .. } => {}
            ToHost::BufferNew { id, width, height, stride, format, ranges } => {
                if let Some(mem) = &self.mem {
                    let b = Buffer::new(mem, width, height, stride, format, &ranges);
                    if !b.valid && debug() {
                        eprintln!("airDroid: buffer {id} {width}x{height} not in guest RAM: {:?}", &ranges[..ranges.len().min(4)]);
                    }
                    self.buffers.insert(id, b);
                } else if debug() {
                    eprintln!("airDroid: buffer {id} before guest RAM was mapped");
                }
            }
            ToHost::BufferDestroy { id } => {
                self.buffers.remove(&id);
            }
            ToHost::WindowNew { id, title, app_id, .. } if debug() && { eprintln!("airDroid: window {id} new '{title}' ({app_id})"); false } => {}
            ToHost::WindowDestroy { id } if debug() && { eprintln!("airDroid: window {id} destroyed"); false } => {}
            ToHost::WindowNew { id, title, app_id, .. } => {
                let title = window_title(&title, &app_id, &self.apps);
                self.windows.insert(id, Win { canvas: Canvas::new(), crop: None, title, shown: None, candidate: None, buttons: 0, touching: false });
            }
            ToHost::WindowUpdate { id, title, app_id, .. } => {
                let t = window_title(&title, &app_id, &self.apps);
                if let Some(w) = self.windows.get_mut(&id) {
                    if w.title != t {
                        host::window_set_title(id, &t);
                        w.title = t;
                    }
                }
            }
            ToHost::WindowDestroy { id } => {
                host::window_close(id);
                self.windows.remove(&id);
            }
            ToHost::Frame { window, serial, width, height, layers, .. } => {
                if debug() {
                    eprintln!(
                        "airDroid: frame {window}/{serial} {width}x{height} {:?}",
                        layers.iter().map(|l| (l.buffer, l.dst, self.buffers.get(&l.buffer).map(|b| b.valid))).collect::<Vec<_>>()
                    );
                }
                let multi = self.settings.multi_windows;
                if let Some(w) = self.windows.get_mut(&window) {
                    let full = Rect::new(0, 0, width as i32, height as i32);
                    // multi-window: just the app's task, where Android put it
                    // (its display is the screen, so that is screen coordinates)
                    let area = if multi {
                        // the task without its shadow (no per-pixel alpha
                        // windows here); recomputed when the layers move
                        let geometry: Vec<(Rect, Rect)> = layers.iter().map(|l| (l.src, l.dst)).collect();
                        let bounds = match &w.crop {
                            Some((g, b)) if *g == geometry => *b,
                            _ => {
                                let b = opaque_bounds(&layers, &self.buffers).or_else(|| content_bounds(&layers));
                                w.crop = Some((geometry, b));
                                b
                            }
                        };
                        bounds.map(|b| b.intersect(&full))
                    } else {
                        Some(full)
                    };
                    let settled = match (w.shown, area) {
                        (None, Some(a)) if multi => {
                            let ok = w.candidate == Some(a);
                            w.candidate = Some(a);
                            ok
                        }
                        _ => true,
                    };
                    if let Some(area) = area.filter(|a| !a.is_empty() && settled) {
                        if debug() && w.shown != Some(area) {
                            eprintln!("airDroid: window {window} shows {area:?}");
                        }
                        match w.shown {
                            None => host::window_open(window, &w.title, multi.then_some((area.x, area.y)), area.w as u32, area.h as u32, multi),
                            Some(old) => {
                                if (old.w, old.h) != (area.w, area.h) {
                                    host::window_resize(window, area.w as u32, area.h as u32);
                                }
                                if multi && (old.x, old.y) != (area.x, area.y) {
                                    host::window_move(window, area.x, area.y);
                                }
                            }
                        }
                        w.shown = Some(area);
                        w.canvas.resize(area.w as u32, area.h as u32);
                        w.canvas.compose_at(&layers, &self.buffers, area.x, area.y);
                        host::window_present(window, &w.canvas.pixels, area.w as u32, area.h as u32);
                    }
                }
                self.frames_done.push((window, serial));
            }
            ToHost::Cursor { window, buffer, .. } => host::window_hide_cursor(window, buffer.is_some()),
            ToHost::Apps { apps } => self.set_apps(apps),
            ToHost::Clipboard { text } => host::clipboard_set(&text),
            ToHost::Notify { id, app_name, package, title, body, image } => {
                let icon = self.apps.iter().find(|a| a.package == package).and_then(|a| a.icon_png.clone());
                host::notify(id, &app_name, &package, &title, &body, image.as_ref(), icon.as_deref());
            }
            ToHost::NotifyClose { .. } => {} // air/OS notifications expire by themselves
            ToHost::Reply { req, ok, message } if self.waiters.contains_key(&req) => {
                let mut conn = self.waiters.remove(&req).unwrap();
                if let Some(name) = self.installs.remove(&req) {
                    self.status(&format!("{name} {}.", if ok { "is installed" } else { "could not be installed" }));
                }
                let kind = if ok { control::OUT } else { control::ERR };
                if !message.trim().is_empty() {
                    let _ = control::write_frame(&mut conn, kind, format!("{}\n", message.trim_end()).as_bytes());
                }
                let _ = control::write_frame(&mut conn, control::EXIT, &(if ok { 0i32 } else { 1 }).to_le_bytes());
            }
            ToHost::Reply { req, ok, message } => {
                if let Some(name) = self.installs.remove(&req) {
                    if ok {
                        self.status(&format!("{name} is installed."));
                    } else {
                        self.status(&format!("Installing {name} failed."));
                        host::alert("airDroid", &format!("Installing {name} failed:\n\n{}", message.trim()));
                    }
                } else if !ok {
                    self.status(message.trim());
                }
            }
            ToHost::ExecOutput { req, stderr, data } => {
                if let Some(conn) = self.waiters.get_mut(&req) {
                    if control::write_frame(conn, if stderr { control::ERR } else { control::OUT }, &data).is_err() {
                        self.waiters.remove(&req);
                    }
                }
            }
            ToHost::ExecDone { req, code } => {
                if let Some(mut conn) = self.waiters.remove(&req) {
                    let _ = control::write_frame(&mut conn, control::EXIT, &code.to_le_bytes());
                }
            }
            ToHost::AudioOpen { stream, rate, channels } => {
                host::audio_open(stream, rate, channels);
                self.audio.push(stream);
            }
            ToHost::AudioData { stream, data } => host::audio_write(stream, &data),
            ToHost::AudioClose { stream } => {
                host::audio_close(stream);
                self.audio.retain(|s| *s != stream);
            }
            ToHost::AudioInOpen { stream, rate, channels } => {
                let native = host::mic_open(stream, rate, channels);
                debug_log(&format!("microphone {stream}: {rate} Hz x{channels}, {}", if native { "audio input" } else { "no input: silence" }));
                self.mics.insert(stream, Mic { rate, channels, native, opened: Instant::now(), sent_frames: 0, got_data: false });
            }
            ToHost::AudioInClose { stream } => {
                if let Some(m) = self.mics.remove(&stream) {
                    if m.native {
                        host::mic_close(stream);
                    }
                }
            }
        }
    }

    /// Answers frames at the display's pace, so Android renders no faster.
    fn vsync(&mut self) {
        let now = Instant::now();
        if now < self.next_vsync {
            return;
        }
        self.next_vsync += FRAME;
        if self.next_vsync < now {
            self.next_vsync = now + FRAME;
        }
        if self.frames_done.is_empty() {
            return;
        }
        let t = self.epoch.elapsed().as_nanos() as u64;
        if let Some(s) = &self.session {
            for (window, serial) in self.frames_done.drain(..) {
                let _ = s.send(&ToGuest::FrameDone { window, serial, time_ns: t });
            }
        } else {
            self.frames_done.clear();
        }
    }

    /// Captured audio to Android's record streams: the audio input's, or
    /// silence in real time when there is none (or it stays quiet: an
    /// input node that delivers nothing), so recording apps keep running.
    fn microphones(&mut self) {
        let Some(s) = &self.session else { return };
        for (&stream, m) in self.mics.iter_mut() {
            let due = m.opened.elapsed().as_micros() as u64 * m.rate as u64 / 1_000_000;
            let data = if m.native {
                let data = host::mic_take(stream);
                if !data.is_empty() {
                    m.got_data = true;
                } else if !m.got_data && m.opened.elapsed() > Duration::from_secs(1) {
                    debug_log(&format!("microphone {stream}: the audio input sends nothing; silence instead"));
                    host::mic_close(stream);
                    m.native = false;
                }
                data
            } else if due >= m.sent_frames + m.rate as u64 / 100 {
                vec![0u8; ((due - m.sent_frames) * m.channels as u64 * 2) as usize]
            } else {
                Vec::new()
            };
            if !data.is_empty() {
                m.sent_frames += data.len() as u64 / (m.channels as u64 * 2);
                let _ = s.send(&ToGuest::AudioIn { stream, data });
            }
        }
    }

    fn audio_credits(&mut self) {
        let Some(s) = &self.session else { return };
        for &stream in &self.audio {
            let bytes = host::audio_take_consumed(stream);
            if bytes > 0 {
                let _ = s.send(&ToGuest::AudioCredit { stream, bytes });
            }
        }
    }

    fn set_apps(&mut self, apps: Vec<AppInfo>) {
        // Android's own plumbing apps stay out of the list (as Waydroid hides them)
        const HIDDEN: [&str; 2] = ["com.android.inputmethod.latin", "com.android.documentsui"];
        let shown: Vec<AppInfo> = apps.into_iter().filter(|a| !HIDDEN.contains(&a.package.as_str())).collect();
        let list: Vec<(String, String, Option<Vec<u8>>)> =
            shown.iter().map(|a| (a.package.clone(), display_name(a), a.icon_png.clone())).collect();
        host::main_set_apps(&list);
        self.sync_launchers(&shown);
        self.apps = shown;
    }

    /// One Deskbar entry per launchable app (Waydroid writes .desktop files).
    fn sync_launchers(&self, apps: &[AppInfo]) {
        let dir = &self.paths.launchers;
        let _ = std::fs::create_dir_all(dir);
        let mut keep = Vec::new();
        for a in apps {
            let file = dir.join(sanitize(&display_name(a)));
            host::make_launcher(&file, &self.app_path, &a.package, a.icon_png.as_deref());
            keep.push(file);
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if !keep.contains(&e.path()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    // ---- host events --------------------------------------------------------

    fn send(&self, m: ToGuest) {
        if let Some(s) = &self.session {
            let _ = s.send(&m);
        }
    }

    fn host_event(&mut self, e: Event) {
        let ev = e.ev;
        let win = ev.window;
        match ev.kind {
            kind::MOUSE_MOVED => {
                let touch = self.settings.mouse_as_touch;
                if let Some(w) = self.windows.get(&win) {
                    let (x, y) = w.to_android(ev.x, ev.y);
                    if touch {
                        if w.touching {
                            self.send(ToGuest::Touch { window: win, id: 0, kind: TouchKind::Motion, x, y });
                        }
                    } else {
                        self.send(ToGuest::PointerMotion { window: win, x, y });
                    }
                }
            }
            kind::MOUSE_DOWN | kind::MOUSE_UP => {
                let touch = self.settings.mouse_as_touch;
                let Some(w) = self.windows.get_mut(&win) else { return };
                let old = w.buttons;
                let new = ev.buttons;
                w.buttons = new;
                let (x, y) = w.to_android(ev.x, ev.y);
                if touch {
                    let down = new & 1 != 0;
                    if down != w.touching {
                        w.touching = down;
                        let k = if down { TouchKind::Down } else { TouchKind::Up };
                        self.send(ToGuest::Touch { window: win, id: 0, kind: k, x, y });
                    }
                    return;
                }
                self.send(ToGuest::PointerMotion { window: win, x, y });
                // B_PRIMARY/SECONDARY/TERTIARY_MOUSE_BUTTON -> BTN_LEFT/RIGHT/MIDDLE
                for (bit, btn) in [(1, 0x110), (2, 0x111), (4, 0x112)] {
                    if (old ^ new) & bit != 0 {
                        self.send(ToGuest::PointerButton { window: win, button: btn, pressed: new & bit != 0 });
                    }
                }
            }
            kind::MOUSE_EXITED => self.send(ToGuest::PointerLeave { window: win }),
            kind::WHEEL => {
                // wl_pointer axis units: about 10 per wheel click, like Weston
                self.send(ToGuest::PointerAxis {
                    window: win,
                    dx: ev.dx * 10.0,
                    dy: ev.dy * 10.0,
                    discrete_x: ev.dx.round() as i32,
                    discrete_y: ev.dy.round() as i32,
                });
            }
            kind::KEY_DOWN | kind::KEY_UP if debug() && { eprintln!("airDroid: key {} raw {:#x} -> {:?} (window {win})", if ev.kind == kind::KEY_DOWN { "down" } else { "up" }, ev.key, keys::linux_key(ev.key)); false } => {}
            kind::KEY_DOWN | kind::KEY_UP => {
                if let Some(code) = keys::linux_key(ev.key) {
                    // Android repeats keys itself
                    if ev.kind == kind::KEY_DOWN && ev.repeat > 1 {
                        return;
                    }
                    self.send(ToGuest::Key { window: win, code, pressed: ev.kind == kind::KEY_DOWN });
                }
            }
            kind::MODIFIERS => {
                for (code, pressed) in keys::modifier_changes(ev.key as u32, ev.modifiers) {
                    self.send(ToGuest::Key { window: win, code, pressed });
                }
                self.modifiers = ev.modifiers;
            }
            kind::WINDOW_CLOSE => self.send(ToGuest::WindowClose { id: win }),
            kind::WINDOW_RESIZED => {
                let (w, h) = (ev.x as u32, ev.y as u32);
                if let Some(win_state) = self.windows.get(&win) {
                    let shown = win_state.shown.map(|r| (r.w as u32, r.h as u32));
                    if shown.is_some() && shown != Some((w, h)) && !self.settings.multi_windows {
                        self.send(ToGuest::WindowResize { id: win, width: w, height: h });
                    }
                }
            }
            kind::TASK_BOUNDS => {
                self.send(ToGuest::TaskBounds { id: win, x: ev.x as i32, y: ev.y as i32, width: ev.dx as u32, height: ev.dy as u32 })
            }
            kind::WINDOW_ACTIVATED => self.send(ToGuest::Focus { window: Some(win) }),
            kind::WINDOW_DEACTIVATED => {}
            kind::REFS => {
                if e.text.ends_with(".apk") || e.text.ends_with(".APK") {
                    self.request(Pending::Install(PathBuf::from(&e.text)));
                } else {
                    host::alert("airDroid", &format!("{} is not an Android app (.apk).", e.text));
                }
            }
            kind::ARGV => {
                let args: Vec<String> = e.text.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();
                self.handle_args(&args);
            }
            kind::APP_CLICKED => {
                self.request(Pending::Launch(e.text.clone()));
            }
            kind::CLIPBOARD => self.send(ToGuest::Clipboard { text: e.text.clone() }),
            kind::ACTION => match ev.key {
                action::SHOW_FULL_UI => {
                    self.request(Pending::FullUi);
                }
                action::INSTALL_APK => host::open_file_panel(),
                action::STOP => self.stop_vm(),
                action::START => {
                    if self.phase == Phase::Stopped {
                        self.start_vm()
                    }
                }
                action::SETUP => {
                    if self.session.is_none() {
                        let _ = std::fs::remove_file(self.paths.images().join("installed.json"));
                        let _ = std::fs::remove_file(self.paths.images().join("system.img"));
                        self.start_setup();
                    } else {
                        host::alert("airDroid", "Stop Android before downloading it again.");
                    }
                }
                action::TOGGLE_MULTIWINDOW => {
                    self.settings.multi_windows = !self.settings.multi_windows;
                    self.settings.save(&self.paths.settings);
                    host::main_set_multiwindow(self.settings.multi_windows);
                    host::alert("airDroid", "The change takes effect when Android starts next time.");
                }
                action::REMOVE_APP => {
                    if let Some(s) = &self.session {
                        let _ = s.send(&ToGuest::RemoveApp { req: s.next_req(), package: e.text.clone() });
                    }
                }
                _ => {}
            },
            kind::QUIT => {
                self.quitting = true;
                self.cancel_setup.store(true, std::sync::atomic::Ordering::Relaxed);
                if self.session.is_some() {
                    self.stop_vm();
                } else {
                    self.child = None;
                }
            }
            _ => {}
        }
    }
}

fn debug_log(text: &str) {
    if debug() {
        eprintln!("airDroid: {text}");
    }
}

/// An Android record stream being fed (see Engine::microphones).
struct Mic {
    rate: u32,
    channels: u32,
    /// the system's audio input (else silence)
    native: bool,
    opened: Instant,
    sent_frames: u64,
    got_data: bool,
}

fn debug() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("AIRDROID_DEBUG").is_ok())
}

fn display_name(a: &AppInfo) -> String {
    if a.name != a.package && !a.name.is_empty() {
        return a.name.clone();
    }
    // com.android.settings -> Settings
    let last = a.package.rsplit('.').next().unwrap_or(&a.package);
    let mut c = last.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => a.package.clone(),
    }
}

fn window_title(title: &str, app_id: &str, apps: &[AppInfo]) -> String {
    if !title.is_empty() && title != "Waydroid" {
        return title.to_string();
    }
    if app_id == "Waydroid" || app_id.is_empty() {
        return "Android".into();
    }
    apps.iter().find(|a| a.package == app_id).map(display_name).unwrap_or_else(|| app_id.to_string())
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c == '/' || c == ':' { '-' } else { c }).collect()
}
