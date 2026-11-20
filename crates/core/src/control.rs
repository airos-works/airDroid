//! The running airDroid app's control socket, for the `airdroid` command
//! (waydroid's command line: shell, logcat, app install/launch/list, ...).
//!
//! A request is one JSON line; the answer a stream of frames:
//! `[kind: u8][len: u32 LE][data]`, kind 1 stdout, 2 stderr, 3 exit code
//! (i32 LE, last frame).

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// A command in Android (or the VM): `waydroid shell`.
    Shell { argv: Vec<String>, uid: Option<u32>, vm: bool },
    Install { path: String },
    Remove { package: String },
    Launch { package: String },
    Intent { action: String, uri: String },
    List,
    ShowFullUi,
    Status,
    Stop,
    SetProp { key: String, value: String },
}

pub const OUT: u8 = 1;
pub const ERR: u8 = 2;
pub const EXIT: u8 = 3;

pub fn write_frame<W: Write>(w: &mut W, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(5 + data.len());
    f.push(kind);
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    w.write_all(&f)
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h)?;
    let len = u32::from_le_bytes([h[1], h[2], h[3], h[4]]) as usize;
    let mut d = vec![0u8; len];
    r.read_exact(&mut d)?;
    Ok((h[0], d))
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    crate::vm::socket_dir(state_dir).join("control.sock")
}

/// Sends a request to the running app; copies its output to stdout/stderr
/// and returns the exit code.
pub fn call(state_dir: &Path, req: &Request) -> io::Result<i32> {
    let path = socket_path(state_dir);
    let mut s = UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("airDroid is not running ({}: {e})", path.display())))?;
    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');
    s.write_all(&line)?;
    loop {
        let (kind, data) = read_frame(&mut s)?;
        match kind {
            OUT => {
                io::stdout().write_all(&data)?;
                io::stdout().flush()?;
            }
            ERR => {
                io::stderr().write_all(&data)?;
            }
            EXIT => return Ok(i32::from_le_bytes(data.get(..4).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]))),
            _ => {}
        }
    }
}
