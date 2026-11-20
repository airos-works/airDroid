//! airdroid-dev: boot the airDroid VM headless and drive it.
//!
//!   airdroid-dev --images DIR --guest build/x86_64 --state DIR [--ram MB]
//!                [--cpus N] [--size WxH] [--dpi N] [--multiwin 0|1]
//!                [--cmd FIFO]
//!
//! Commands (one per line, from stdin or the --cmd FIFO):
//!   windows | apps | shot WIN FILE.png | tap WIN X Y | move WIN X Y
//!   click WIN X Y | key WIN CODE | sh CMD... (Android, root) | shsh CMD...
//!   (Android, shell user) | vm CMD... (VM)
//!   install FILE.apk | launch PACKAGE | fullui | prop KEY VALUE | quit

use airdroid_core::compose::{Buffer, Canvas};
use airdroid_core::guestmem::GuestMem;
use airdroid_core::proto::{ToGuest, ToHost, TouchKind};
use airdroid_core::session::Session;
use airdroid_core::vm::{Accel, VmConfig};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Win {
    title: String,
    app_id: String,
    canvas: Canvas,
    frames: u64,
}

/// Plays like a sound card would: buffered audio drains at the stream's
/// rate and comes back as credit; the PCM goes to state/audio-N.raw.
struct AudioOut {
    bps: u64,
    buffered: u64,
    played: u64,
    file: std::fs::File,
}

#[derive(Default)]
struct Host {
    buffers: HashMap<u32, Buffer>,
    windows: HashMap<u32, Win>,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn save_png(path: &str, c: &Canvas) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), c.width, c.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    let mut data = Vec::with_capacity(c.pixels.len() * 4);
    for p in &c.pixels {
        data.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8, 0xff]);
    }
    w.write_image_data(&data).map_err(std::io::Error::other)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let images = PathBuf::from(arg(&args, "--images").expect("--images DIR"));
    let guest = PathBuf::from(arg(&args, "--guest").unwrap_or_else(|| "build/x86_64".into()));
    let state = PathBuf::from(arg(&args, "--state").unwrap_or_else(|| "build/state".into()));
    let ram_mb: u64 = arg(&args, "--ram").and_then(|v| v.parse().ok()).unwrap_or(4096);
    let cpus: u32 = arg(&args, "--cpus").and_then(|v| v.parse().ok()).unwrap_or(4);
    let (width, height) = arg(&args, "--size")
        .and_then(|s| s.split_once('x').map(|(a, b)| (a.parse().unwrap_or(1280), b.parse().unwrap_or(800))))
        .unwrap_or((1280, 800));
    let dpi: u32 = arg(&args, "--dpi").and_then(|v| v.parse().ok()).unwrap_or(0);
    let multi = arg(&args, "--multiwin").map(|v| v == "1").unwrap_or(false);
    std::fs::create_dir_all(&state).unwrap();
    let userdata = state.join("userdata.qcow2");
    if !userdata.exists() {
        airdroid_core::qcow2::create(&userdata, 32 << 30).expect("create userdata");
    }
    let cfg = VmConfig {
        qemu: PathBuf::from(arg(&args, "--qemu").unwrap_or_else(|| "qemu-system-x86_64".into())),
        firmware: arg(&args, "--firmware").map(PathBuf::from),
        kernel: guest.join("vmlinuz"),
        initrd: guest.join("initramfs.img"),
        system_img: images.join("system.img"),
        vendor_img: images.join("vendor.img"),
        userdata,
        state_dir: state.clone(),
        ram: airdroid_core::vm::RamBackend::for_platform(&state),
        ram_mb,
        cpus,
        accel: Accel::detect(),
        width,
        height,
        dpi,
        multi_windows: multi,
        gpu: arg(&args, "--gpu").unwrap_or_else(|| "none".into()),
        env: Vec::new(),
        share: arg(&args, "--share").map(PathBuf::from),
    };
    eprintln!("dev: qemu {}", cfg.args().join(" "));
    let mut child = cfg.spawn().expect("start qemu");
    let (session, rx) = Session::connect(&cfg.socket(), Duration::from_secs(20)).expect("connect to the VM");
    // guest RAM is mapped once the guest talks (QEMU set it up by then)
    let mut mem: Option<Arc<GuestMem>> = None;
    let host = Arc::new(Mutex::new(Host::default()));
    let start = Instant::now();

    // commands
    {
        let host = host.clone();
        let session = session.clone();
        let fifo = arg(&args, "--cmd");
        std::thread::spawn(move || loop {
            let input: Box<dyn BufRead> = match &fifo {
                Some(p) => Box::new(std::io::BufReader::new(std::fs::File::open(p).expect("open fifo"))),
                None => Box::new(std::io::BufReader::new(std::io::stdin())),
            };
            for line in input.lines() {
                let Ok(line) = line else { break };
                command(&line, &host, &session);
            }
            if fifo.is_none() {
                return;
            }
        });
    }

    let mut pending_done: Vec<(u32, u32)> = Vec::new();
    let mut audio: HashMap<u32, AudioOut> = HashMap::new();
    let mut mics: HashMap<u32, (u32, u32, u64, Instant)> = HashMap::new();
    let mut last_audio = Instant::now();
    let mut next_vsync = Instant::now();
    loop {
        let timeout = next_vsync.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(msg) => match msg {
                ToHost::Hello { version, kernel } => {
                    eprintln!("dev: guest protocol {version}, kernel {kernel}");
                    match cfg.open_guest_mem() {
                        Ok(m) => mem = Some(Arc::new(m)),
                        Err(e) => eprintln!("dev: cannot map guest RAM: {e}"),
                    }
                }
                ToHost::Status { stage, detail } => eprintln!("dev: [{:6.1}s] status {stage:?}: {detail}", start.elapsed().as_secs_f32()),
                ToHost::Log { text } => eprintln!("dev: [{:6.1}s] guest: {text}", start.elapsed().as_secs_f32()),
                ToHost::BufferNew { id, width, height, stride, format, ranges } => {
                    let Some(mem) = mem.as_ref() else { continue };
                    let b = Buffer::new(mem, width, height, stride, format, &ranges);
                    if !b.valid {
                        eprintln!("dev: buffer {id} {width}x{height} fmt {format:#x}: invalid ranges");
                    }
                    host.lock().unwrap().buffers.insert(id, b);
                }
                ToHost::BufferDestroy { id } => {
                    host.lock().unwrap().buffers.remove(&id);
                }
                ToHost::WindowNew { id, title, app_id, width, height } => {
                    eprintln!("dev: window {id} '{title}' ({app_id}) {width}x{height}");
                    host.lock().unwrap().windows.insert(id, Win { title, app_id, canvas: Canvas::new(), frames: 0 });
                }
                ToHost::WindowUpdate { id, title, app_id, .. } => {
                    eprintln!("dev: window {id} now '{title}' ({app_id})");
                    if let Some(w) = host.lock().unwrap().windows.get_mut(&id) {
                        w.title = title;
                        w.app_id = app_id;
                    }
                }
                ToHost::WindowDestroy { id } => {
                    eprintln!("dev: window {id} closed");
                    host.lock().unwrap().windows.remove(&id);
                }
                ToHost::Frame { window, serial, width, height, layers, .. } => {
                    let mut h = host.lock().unwrap();
                    let Host { buffers, windows } = &mut *h;
                    if let Some(w) = windows.get_mut(&window) {
                        // like the air/OS app in multi-window mode: only the
                        // task's bounds (AIRDROID_DEV_CROP=1)
                        let crop = std::env::var("AIRDROID_DEV_CROP").is_ok();
                        match airdroid_core::compose::opaque_bounds(&layers, buffers).or_else(|| airdroid_core::compose::content_bounds(&layers)).filter(|_| crop) {
                            Some(b) => {
                                w.canvas.resize(b.w as u32, b.h as u32);
                                w.canvas.compose_at(&layers, buffers, b.x, b.y);
                            }
                            None => {
                                w.canvas.resize(width, height);
                                w.canvas.compose(&layers, buffers);
                            }
                        }
                        w.frames += 1;
                        if std::env::var("AIRDROID_DEV_LAYERS").is_ok() {
                            eprintln!("dev: frame {window}/{serial} {width}x{height} {:?}", layers.iter().map(|l| (l.buffer, l.dst, buffers.get(&l.buffer).map(|b| b.valid))).collect::<Vec<_>>());
                        }
                    }
                    pending_done.push((window, serial));
                }
                ToHost::ExecOutput { data, .. } => {
                    let _ = std::io::stdout().write_all(&data);
                    let _ = std::io::stdout().flush();
                }
                ToHost::ExecDone { req, code } => eprintln!("dev: exec {req} exited {code}"),
                ToHost::Reply { req, ok, message } => eprintln!("dev: reply {req} ok={ok}: {}", message.trim()),
                ToHost::Apps { apps } => {
                    eprintln!("dev: {} apps", apps.len());
                    for a in apps {
                        eprintln!("dev:   {} '{}' {} icon={}", a.package, a.name, a.activity, a.icon_png.map(|i| i.len()).unwrap_or(0));
                    }
                }
                ToHost::Clipboard { text } => eprintln!("dev: clipboard: {text}"),
                ToHost::Notify { id, app_name, package, title, body, image } => eprintln!(
                    "dev: notification {id} from {app_name} ({package}): '{title}' '{body}' image={:?}",
                    image.map(|i| (i.width, i.height, i.has_alpha))
                ),
                ToHost::NotifyClose { id } => eprintln!("dev: notification {id} closed"),
                ToHost::AudioOpen { stream, rate, channels } => {
                    eprintln!("dev: audio {stream} open: {rate} Hz x{channels}");
                    let file = std::fs::File::create(state.join(format!("audio-{stream}.raw"))).unwrap();
                    audio.insert(stream, AudioOut { bps: rate as u64 * channels as u64 * 2, buffered: 0, played: 0, file });
                }
                ToHost::AudioData { stream, data } => {
                    if let Some(a) = audio.get_mut(&stream) {
                        a.buffered += data.len() as u64;
                        let _ = a.file.write_all(&data);
                    }
                }
                ToHost::AudioClose { stream } => {
                    if let Some(a) = audio.remove(&stream) {
                        eprintln!("dev: audio {stream} closed after {} bytes", a.played + a.buffered);
                    }
                }
                ToHost::AudioInOpen { stream, rate, channels } => {
                    eprintln!("dev: microphone {stream} open: {rate} Hz x{channels} (a 440 Hz tone)");
                    mics.insert(stream, (rate, channels, 0u64, Instant::now()));
                }
                ToHost::AudioInClose { stream } => {
                    if let Some((_, _, sent, _)) = mics.remove(&stream) {
                        eprintln!("dev: microphone {stream} closed after {sent} frames");
                    }
                }
                ToHost::Cursor { .. } => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("dev: VM connection closed");
                break;
            }
        }
        // the microphone: a 440 Hz tone in real time, 10 ms at a time
        for (stream, (rate, channels, sent, opened)) in mics.iter_mut() {
            let due = opened.elapsed().as_micros() as u64 * *rate as u64 / 1_000_000;
            if due >= *sent + *rate as u64 / 100 {
                let mut data = Vec::with_capacity(((due - *sent) * *channels as u64 * 2) as usize);
                for i in *sent..due {
                    let v = ((i as f64 * 440.0 * std::f64::consts::TAU / *rate as f64).sin() * 8000.0) as i16;
                    for _ in 0..*channels {
                        data.extend_from_slice(&v.to_le_bytes());
                    }
                }
                *sent = due;
                let _ = session.send(&ToGuest::AudioIn { stream: *stream, data });
            }
        }
        let dt = last_audio.elapsed();
        if dt >= Duration::from_millis(10) {
            last_audio = Instant::now();
            for (stream, a) in audio.iter_mut() {
                let n = (a.bps * dt.as_micros() as u64 / 1_000_000).min(a.buffered) / 4 * 4;
                if n > 0 {
                    a.buffered -= n;
                    a.played += n;
                    let _ = session.send(&ToGuest::AudioCredit { stream: *stream, bytes: n });
                }
            }
        }
        if Instant::now() >= next_vsync {
            let t = start.elapsed().as_nanos() as u64;
            for (window, serial) in pending_done.drain(..) {
                let _ = session.send(&ToGuest::FrameDone { window, serial, time_ns: t });
            }
            next_vsync += Duration::from_micros(16_667);
            if next_vsync < Instant::now() {
                next_vsync = Instant::now() + Duration::from_micros(16_667);
            }
        }
    }
    let _ = child.wait();
}

fn command(line: &str, host: &Arc<Mutex<Host>>, s: &Session) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| parts.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
    let win = parts.get(1).and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
    let r = match parts.first().copied() {
        Some("windows") => {
            for (id, w) in &host.lock().unwrap().windows {
                eprintln!("dev: window {id} '{}' ({}) {}x{} frames={}", w.title, w.app_id, w.canvas.width, w.canvas.height, w.frames);
            }
            Ok(())
        }
        Some("shot") => {
            let h = host.lock().unwrap();
            match h.windows.get(&win) {
                Some(w) => save_png(parts.get(2).unwrap_or(&"shot.png"), &w.canvas).map(|_| eprintln!("dev: saved")),
                None => Ok(eprintln!("dev: no window {win}")),
            }
        }
        Some("tap") => {
            let (x, y) = (num(2), num(3));
            let _ = s.send(&ToGuest::Touch { window: win, id: 0, kind: TouchKind::Down, x, y });
            std::thread::sleep(Duration::from_millis(60));
            s.send(&ToGuest::Touch { window: win, id: 0, kind: TouchKind::Up, x, y })
        }
        Some("swipe") => {
            let (x0, y0, x1, y1) = (num(2), num(3), num(4), num(5));
            let _ = s.send(&ToGuest::Touch { window: win, id: 0, kind: TouchKind::Down, x: x0, y: y0 });
            for i in 1..=10 {
                let f = i as f32 / 10.0;
                std::thread::sleep(Duration::from_millis(20));
                let _ = s.send(&ToGuest::Touch { window: win, id: 0, kind: TouchKind::Motion, x: x0 + (x1 - x0) * f, y: y0 + (y1 - y0) * f });
            }
            s.send(&ToGuest::Touch { window: win, id: 0, kind: TouchKind::Up, x: x1, y: y1 })
        }
        Some("move") => s.send(&ToGuest::PointerMotion { window: win, x: num(2), y: num(3) }),
        Some("click") => {
            let _ = s.send(&ToGuest::PointerMotion { window: win, x: num(2), y: num(3) });
            let _ = s.send(&ToGuest::PointerButton { window: win, button: 0x110, pressed: true });
            std::thread::sleep(Duration::from_millis(50));
            s.send(&ToGuest::PointerButton { window: win, button: 0x110, pressed: false })
        }
        Some("key") => {
            let code = num(2) as u32;
            let _ = s.send(&ToGuest::Key { window: win, code, pressed: true });
            s.send(&ToGuest::Key { window: win, code, pressed: false })
        }
        Some("close") => s.send(&ToGuest::WindowClose { id: win }),
        Some("bounds") => s.send(&ToGuest::TaskBounds { id: win, x: num(2) as i32, y: num(3) as i32, width: num(4) as u32, height: num(5) as u32 }),
        Some("sh") | Some("vm") | Some("shsh") => {
            let android = parts[0] != "vm";
            let uid = (parts[0] == "shsh").then_some(2000);
            let cmd = line.splitn(2, ' ').nth(1).unwrap_or("").to_string();
            let argv = if android { vec!["/system/bin/sh".into(), "-c".into(), cmd] } else { vec!["/bin/sh".into(), "-c".into(), cmd] };
            s.send(&ToGuest::Exec { req: s.next_req(), argv, android, uid, stdin: Vec::new() })
        }
        Some("install") => {
            let path = PathBuf::from(parts.get(1).copied().unwrap_or(""));
            let guest = format!("/data/waydroid_tmp/{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            match s.put_file(&path, &guest) {
                Ok(_) => s.send(&ToGuest::InstallApk { req: s.next_req(), path: guest }),
                Err(e) => Err(e),
            }
        }
        Some("launch") => s.send(&ToGuest::LaunchApp { req: s.next_req(), package: parts.get(1).unwrap_or(&"").to_string() }),
        Some("fullui") => s.send(&ToGuest::ShowFullUi { req: s.next_req() }),
        Some("apps") => s.send(&ToGuest::ListApps),
        Some("prop") => s.send(&ToGuest::SetProp { req: s.next_req(), key: parts.get(1).unwrap_or(&"").to_string(), value: parts.get(2).unwrap_or(&"").to_string() }),
        Some("clip") => s.send(&ToGuest::Clipboard { text: line.splitn(2, ' ').nth(1).unwrap_or("").to_string() }),
        Some("quit") => s.send(&ToGuest::Shutdown),
        Some(other) => Ok(eprintln!("dev: unknown command {other}")),
        None => Ok(()),
    };
    if let Err(e) = r {
        eprintln!("dev: {e}");
    }
}
