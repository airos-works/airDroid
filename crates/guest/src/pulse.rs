//! A small PulseAudio server for Waydroid's audio HAL.
//!
//! `audio.primary.waydroid` opens ALSA's "pulse" device, i.e. libpulse
//! talking the PulseAudio native protocol to $PULSE_RUNTIME_PATH/native.
//! Waydroid hands it the host's PulseAudio socket; airDroid serves it here
//! with just what libpulse's playback path uses (protocol 35, no shared
//! memory: audio comes as memblock frames on the socket) and forwards the
//! PCM to the host, which plays it with the Media Kit. Pacing: the host
//! grants credits as it plays; data is pulled from the clients (REQUEST)
//! only as fast as that. Recording is refused for now.
//!
//! Wire format: frames of a 20-byte descriptor (length, channel, offset
//! hi/lo, flags; big endian) and a payload. Channel 0xffffffff carries a
//! command tagstruct, any other channel audio for that stream index.
//! (pulsecore/pstream.c, tagstruct.c, protocol-native.c in PulseAudio 17.)

use crate::hostlink::HostLink;
use crate::logf;
use airdroid_proto::ToHost;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: u32 = 35;
const VERSION_MASK: u32 = 0x0000_ffff;
const COMMAND_CHANNEL: u32 = 0xffff_ffff;
const MAX_FRAME: usize = 16 << 20;

// pulsecore/native-common.h
mod cmd {
    pub const ERROR: u32 = 0;
    pub const REPLY: u32 = 2;
    pub const CREATE_PLAYBACK_STREAM: u32 = 3;
    pub const DELETE_PLAYBACK_STREAM: u32 = 4;
    pub const CREATE_RECORD_STREAM: u32 = 5;
    pub const AUTH: u32 = 8;
    pub const SET_CLIENT_NAME: u32 = 9;
    pub const LOOKUP_SINK: u32 = 10;
    pub const DRAIN_PLAYBACK_STREAM: u32 = 12;
    pub const GET_PLAYBACK_LATENCY: u32 = 14;
    pub const GET_SERVER_INFO: u32 = 20;
    pub const SUBSCRIBE: u32 = 35;
    pub const SET_SINK_INPUT_VOLUME: u32 = 37;
    pub const CORK_PLAYBACK_STREAM: u32 = 41;
    pub const FLUSH_PLAYBACK_STREAM: u32 = 42;
    pub const TRIGGER_PLAYBACK_STREAM: u32 = 43;
    pub const SET_PLAYBACK_STREAM_NAME: u32 = 46;
    pub const PREBUF_PLAYBACK_STREAM: u32 = 60;
    pub const REQUEST: u32 = 61;
    pub const SET_SINK_INPUT_MUTE: u32 = 69;
    pub const SET_PLAYBACK_STREAM_BUFFER_ATTR: u32 = 72;
    pub const UPDATE_PLAYBACK_STREAM_SAMPLE_RATE: u32 = 74;
    pub const UPDATE_PLAYBACK_STREAM_PROPLIST: u32 = 81;
    pub const UPDATE_CLIENT_PROPLIST: u32 = 82;
    pub const REMOVE_PLAYBACK_STREAM_PROPLIST: u32 = 84;
    pub const STARTED: u32 = 86;
}

// pulse/def.h error codes
const ERR_NOENTITY: u32 = 5;
const ERR_PROTOCOL: u32 = 7;
const ERR_NOTSUPPORTED: u32 = 19;

// pulse/sample.h
const SAMPLE_S16LE: u8 = 3;
const SAMPLE_FLOAT32LE: u8 = 5;
const SAMPLE_S32LE: u8 = 7;

// ---- tagstruct ---------------------------------------------------------------

struct TagReader<'a> {
    d: &'a [u8],
    i: usize,
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "bad tagstruct")
}

impl<'a> TagReader<'a> {
    fn tag(&mut self) -> io::Result<u8> {
        let t = *self.d.get(self.i).ok_or_else(bad)?;
        self.i += 1;
        Ok(t)
    }
    fn raw_u32(&mut self) -> io::Result<u32> {
        let b = self.d.get(self.i..self.i + 4).ok_or_else(bad)?;
        self.i += 4;
        Ok(u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn raw_u8(&mut self) -> io::Result<u8> {
        self.tag()
    }
    fn expect(&mut self, t: u8) -> io::Result<()> {
        if self.tag()? != t {
            return Err(bad());
        }
        Ok(())
    }
    fn u32(&mut self) -> io::Result<u32> {
        self.expect(b'L')?;
        self.raw_u32()
    }
    fn u8(&mut self) -> io::Result<u8> {
        self.expect(b'B')?;
        self.raw_u8()
    }
    fn boolean(&mut self) -> io::Result<bool> {
        match self.tag()? {
            b'1' => Ok(true),
            b'0' => Ok(false),
            _ => Err(bad()),
        }
    }
    fn string(&mut self) -> io::Result<Option<String>> {
        match self.tag()? {
            b'N' => Ok(None),
            b't' => {
                let rest = &self.d[self.i..];
                let end = rest.iter().position(|&b| b == 0).ok_or_else(bad)?;
                let s = String::from_utf8_lossy(&rest[..end]).into_owned();
                self.i += end + 1;
                Ok(Some(s))
            }
            _ => Err(bad()),
        }
    }
    fn arbitrary(&mut self) -> io::Result<&'a [u8]> {
        self.expect(b'x')?;
        let n = self.raw_u32()? as usize;
        let b = self.d.get(self.i..self.i + n).ok_or_else(bad)?;
        self.i += n;
        Ok(b)
    }
    fn sample_spec(&mut self) -> io::Result<(u8, u8, u32)> {
        self.expect(b'a')?;
        let f = self.raw_u8()?;
        let c = self.raw_u8()?;
        let r = self.raw_u32()?;
        Ok((f, c, r))
    }
    fn channel_map(&mut self) -> io::Result<Vec<u8>> {
        self.expect(b'm')?;
        let n = self.raw_u8()? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.raw_u8()?);
        }
        Ok(v)
    }
    fn cvolume(&mut self) -> io::Result<()> {
        self.expect(b'v')?;
        let n = self.raw_u8()? as usize;
        for _ in 0..n {
            self.raw_u32()?;
        }
        Ok(())
    }
    fn timeval(&mut self) -> io::Result<(u32, u32)> {
        self.expect(b'T')?;
        Ok((self.raw_u32()?, self.raw_u32()?))
    }
    fn proplist(&mut self) -> io::Result<Vec<(String, Vec<u8>)>> {
        self.expect(b'P')?;
        let mut v = Vec::new();
        loop {
            let Some(k) = self.string()? else { break };
            let _len = self.u32()?;
            v.push((k, self.arbitrary()?.to_vec()));
        }
        Ok(v)
    }
    fn format_info(&mut self) -> io::Result<()> {
        self.expect(b'f')?;
        self.u8()?;
        self.proplist()?;
        Ok(())
    }
}

#[derive(Default)]
struct Tags(Vec<u8>);

impl Tags {
    fn command(c: u32, tag: u32) -> Tags {
        let mut t = Tags::default();
        t.u32(c);
        t.u32(tag);
        t
    }
    fn u32(&mut self, v: u32) {
        self.0.push(b'L');
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.0.push(b'B');
        self.0.push(v);
    }
    fn u64(&mut self, v: u64) {
        self.0.push(b'R');
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn s64(&mut self, v: i64) {
        self.0.push(b'r');
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn usec(&mut self, v: u64) {
        self.0.push(b'U');
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn boolean(&mut self, b: bool) {
        self.0.push(if b { b'1' } else { b'0' });
    }
    fn string(&mut self, s: Option<&str>) {
        match s {
            None => self.0.push(b'N'),
            Some(s) => {
                self.0.push(b't');
                self.0.extend_from_slice(s.as_bytes());
                self.0.push(0);
            }
        }
    }
    fn timeval(&mut self, sec: u32, usec: u32) {
        self.0.push(b'T');
        self.0.extend_from_slice(&sec.to_be_bytes());
        self.0.extend_from_slice(&usec.to_be_bytes());
    }
    fn sample_spec(&mut self, f: u8, c: u8, r: u32) {
        self.0.push(b'a');
        self.0.push(f);
        self.0.push(c);
        self.0.extend_from_slice(&r.to_be_bytes());
    }
    fn channel_map(&mut self, m: &[u8]) {
        self.0.push(b'm');
        self.0.push(m.len() as u8);
        self.0.extend_from_slice(m);
    }
    fn empty_proplist(&mut self) {
        self.0.push(b'P');
        self.string(None);
    }
    fn format_info_pcm(&mut self) {
        self.0.push(b'f');
        self.u8(1); // PA_ENCODING_PCM
        self.empty_proplist();
    }
}

fn frame(channel: u32, offset: u64, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(20 + payload.len());
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(&channel.to_be_bytes());
    f.extend_from_slice(&((offset >> 32) as u32).to_be_bytes());
    f.extend_from_slice(&(offset as u32).to_be_bytes());
    f.extend_from_slice(&flags.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

// ---- server state ---------------------------------------------------------------

struct Stream {
    client: u32,
    /// What the host stream is called (global, so streams of all clients
    /// stay apart).
    host_id: u32,
    bytes_per_second: u64,
    tlength: u32,
    queue: VecDeque<u8>,
    corked: bool,
    /// bytes received from the client / handed to the host in total
    write_index: u64,
    read_index: u64,
    /// bytes requested from the client and not yet received
    requested: u64,
    started: bool,
    drain_tags: Vec<u32>,
}

struct Client {
    out: Arc<Mutex<UnixStream>>,
    version: u32,
}

#[derive(Default)]
struct State {
    clients: HashMap<u32, Client>,
    streams: HashMap<u32, Stream>,
    /// host credit (bytes it can take) per host stream id
    credit: HashMap<u32, u64>,
    next_client: u32,
    next_stream: u32,
}

pub struct Pulse {
    state: Mutex<State>,
    wake: Condvar,
    link: HostLink,
}

/// Host buffering granted when a stream opens: about this much audio.
const INITIAL_CREDIT_MS: u64 = 120;

impl Pulse {
    fn send(&self, client: &Client, payload: Tags) {
        let f = frame(COMMAND_CHANNEL, 0, 0, &payload.0);
        let _ = client.out.lock().unwrap().write_all(&f);
    }

    fn reply_ok(&self, client: &Client, tag: u32) {
        self.send(client, Tags::command(cmd::REPLY, tag));
    }

    fn error(&self, client: &Client, tag: u32, code: u32) {
        let mut t = Tags::command(cmd::ERROR, tag);
        t.u32(code);
        self.send(client, t);
    }

    /// The host played `bytes` of stream `host_id`.
    pub fn credit(&self, host_id: u32, bytes: u64) {
        let mut st = self.state.lock().unwrap();
        *st.credit.entry(host_id).or_insert(0) += bytes;
        drop(st);
        self.wake.notify_all();
    }

    fn handle_command(&self, cid: u32, payload: &[u8]) -> io::Result<()> {
        let mut r = TagReader { d: payload, i: 0 };
        let command = r.u32()?;
        let tag = r.u32()?;
        let mut st = self.state.lock().unwrap();
        let Some(client) = st.clients.get(&cid) else { return Ok(()) };
        let version = client.version;
        let out = Client { out: client.out.clone(), version };
        match command {
            cmd::AUTH => {
                let v = r.u32()?;
                let _cookie = r.arbitrary()?;
                let v = (v & VERSION_MASK).min(PROTOCOL_VERSION);
                st.clients.get_mut(&cid).unwrap().version = v;
                let mut t = Tags::command(cmd::REPLY, tag);
                t.u32(PROTOCOL_VERSION); // no SHM, no memfd
                self.send(&out, t);
            }
            cmd::SET_CLIENT_NAME => {
                let mut t = Tags::command(cmd::REPLY, tag);
                if version >= 13 {
                    t.u32(cid);
                }
                self.send(&out, t);
            }
            cmd::GET_SERVER_INFO => {
                let mut t = Tags::command(cmd::REPLY, tag);
                t.string(Some("pulseaudio"));
                t.string(Some("17.0"));
                t.string(Some("airdroid"));
                t.string(Some("airdroid"));
                t.sample_spec(SAMPLE_S16LE, 2, 48000);
                t.string(Some("airos"));
                t.string(None);
                t.u32(0x41495244);
                if version >= 15 {
                    t.channel_map(&[1, 2]);
                }
                self.send(&out, t);
            }
            cmd::LOOKUP_SINK => {
                let mut t = Tags::command(cmd::REPLY, tag);
                t.u32(0);
                self.send(&out, t);
            }
            cmd::CREATE_PLAYBACK_STREAM => {
                drop(st);
                return self.create_playback(cid, &out, tag, &mut r);
            }
            cmd::CREATE_RECORD_STREAM => self.error(&out, tag, ERR_NOENTITY),
            cmd::DELETE_PLAYBACK_STREAM => {
                let idx = r.u32()?;
                if let Some(s) = st.streams.remove(&idx) {
                    st.credit.remove(&s.host_id);
                    self.link.send(&ToHost::AudioClose { stream: s.host_id });
                }
                self.reply_ok(&out, tag);
            }
            cmd::GET_PLAYBACK_LATENCY => {
                let idx = r.u32()?;
                let (sec, usec) = r.timeval()?;
                let Some(s) = st.streams.get(&idx) else {
                    self.error(&out, tag, ERR_NOENTITY);
                    return Ok(());
                };
                let credit = st.credit.get(&s.host_id).copied().unwrap_or(0);
                let initial = s.bytes_per_second * INITIAL_CREDIT_MS / 1000;
                let in_host = initial.saturating_sub(credit);
                let usec_of = |b: u64| if s.bytes_per_second > 0 { b * 1_000_000 / s.bytes_per_second } else { 0 };
                let mut t = Tags::command(cmd::REPLY, tag);
                t.usec(usec_of(in_host)); // sink latency
                t.usec(0); // source latency
                t.boolean(!s.corked && s.started);
                t.timeval(sec, usec);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                t.timeval(now.as_secs() as u32, now.subsec_micros());
                t.s64(s.write_index as i64);
                t.s64(s.read_index.saturating_sub(in_host) as i64);
                if version >= 13 {
                    t.u64(0); // underrun_for
                    t.u64(usec_of(s.read_index)); // playing_for
                }
                self.send(&out, t);
            }
            cmd::CORK_PLAYBACK_STREAM => {
                let idx = r.u32()?;
                let b = r.boolean()?;
                if let Some(s) = st.streams.get_mut(&idx) {
                    s.corked = b;
                }
                self.reply_ok(&out, tag);
                drop(st);
                self.wake.notify_all();
            }
            cmd::FLUSH_PLAYBACK_STREAM => {
                let idx = r.u32()?;
                if let Some(s) = st.streams.get_mut(&idx) {
                    let n = s.queue.len() as u64;
                    s.queue.clear();
                    s.read_index += n;
                }
                self.reply_ok(&out, tag);
            }
            cmd::DRAIN_PLAYBACK_STREAM => {
                let idx = r.u32()?;
                match st.streams.get_mut(&idx) {
                    Some(s) if !s.queue.is_empty() => s.drain_tags.push(tag),
                    _ => self.reply_ok(&out, tag),
                }
                drop(st);
                self.wake.notify_all();
            }
            cmd::TRIGGER_PLAYBACK_STREAM | cmd::PREBUF_PLAYBACK_STREAM => {
                let idx = r.u32()?;
                if let Some(s) = st.streams.get_mut(&idx) {
                    s.started = command == cmd::TRIGGER_PLAYBACK_STREAM || s.started;
                }
                self.reply_ok(&out, tag);
                drop(st);
                self.wake.notify_all();
            }
            cmd::SET_PLAYBACK_STREAM_BUFFER_ATTR => {
                let idx = r.u32()?;
                let maxlength = r.u32()?;
                let tlength = r.u32()?;
                let prebuf = r.u32()?;
                let minreq = r.u32()?;
                if let Some(s) = st.streams.get_mut(&idx) {
                    if tlength != u32::MAX {
                        s.tlength = tlength;
                    }
                }
                let tl = st.streams.get(&idx).map(|s| s.tlength).unwrap_or(tlength);
                let mut t = Tags::command(cmd::REPLY, tag);
                t.u32(maxlength);
                t.u32(tl);
                t.u32(prebuf);
                t.u32(minreq);
                if version >= 13 {
                    t.usec(0);
                }
                self.send(&out, t);
            }
            cmd::SUBSCRIBE
            | cmd::SET_PLAYBACK_STREAM_NAME
            | cmd::UPDATE_PLAYBACK_STREAM_PROPLIST
            | cmd::UPDATE_CLIENT_PROPLIST
            | cmd::REMOVE_PLAYBACK_STREAM_PROPLIST
            | cmd::SET_SINK_INPUT_VOLUME
            | cmd::SET_SINK_INPUT_MUTE
            | cmd::UPDATE_PLAYBACK_STREAM_SAMPLE_RATE => self.reply_ok(&out, tag),
            other => {
                logf!("pulse: unsupported command {other}");
                self.error(&out, tag, ERR_NOTSUPPORTED);
            }
        }
        Ok(())
    }

    fn create_playback(&self, cid: u32, out: &Client, tag: u32, r: &mut TagReader) -> io::Result<()> {
        let version = out.version;
        if version < 13 {
            r.string()?;
        }
        let (format, channels, rate) = r.sample_spec()?;
        let map = r.channel_map()?;
        let _sink_index = r.u32()?;
        let _sink_name = r.string()?;
        let maxlength = r.u32()?;
        let _corked = r.boolean()?;
        let tlength = r.u32()?;
        let prebuf = r.u32()?;
        let minreq = r.u32()?;
        let _syncid = r.u32()?;
        r.cvolume()?;
        if version >= 12 {
            for _ in 0..7 {
                r.boolean()?;
            }
        }
        if version >= 13 {
            r.boolean()?;
            r.boolean()?;
            r.proplist()?;
        }
        if version >= 14 {
            r.boolean()?;
            r.boolean()?;
        }
        if version >= 15 {
            for _ in 0..3 {
                r.boolean()?;
            }
        }
        if version >= 17 {
            r.boolean()?;
        }
        if version >= 18 {
            r.boolean()?;
        }
        if version >= 21 {
            let n = r.u8()?;
            for _ in 0..n {
                r.format_info()?;
            }
        }
        if format != SAMPLE_S16LE && format != SAMPLE_FLOAT32LE && format != SAMPLE_S32LE || channels == 0 || rate == 0 {
            self.error(out, tag, ERR_NOTSUPPORTED);
            return Ok(());
        }
        // the host plays s16le: Android's mixer outputs it anyway
        if format != SAMPLE_S16LE {
            self.error(out, tag, ERR_NOTSUPPORTED);
            return Ok(());
        }
        let bps = rate as u64 * channels as u64 * 2;
        let frame_size = channels as u32 * 2;
        let def_tlength = (bps / 10) as u32; // 100 ms
        let tlength = if tlength == u32::MAX || tlength == 0 { def_tlength } else { tlength } / frame_size * frame_size;
        let minreq = if minreq == u32::MAX || minreq == 0 { tlength / 4 } else { minreq };
        let prebuf = if prebuf == u32::MAX { tlength } else { prebuf };
        let maxlength = if maxlength == u32::MAX { tlength * 4 } else { maxlength };

        let mut st = self.state.lock().unwrap();
        st.next_stream += 1;
        let idx = st.next_stream;
        let host_id = idx;
        st.streams.insert(
            idx,
            Stream {
                client: cid,
                host_id,
                bytes_per_second: bps,
                tlength,
                queue: VecDeque::new(),
                corked: false,
                write_index: 0,
                read_index: 0,
                requested: tlength as u64,
                started: false,
                drain_tags: Vec::new(),
            },
        );
        st.credit.insert(host_id, bps * INITIAL_CREDIT_MS / 1000);
        drop(st);
        self.link.send(&ToHost::AudioOpen { stream: host_id, rate, channels: channels as u32 });

        let mut t = Tags::command(cmd::REPLY, tag);
        t.u32(idx); // stream index (the channel its data comes on)
        t.u32(idx); // sink input index
        t.u32(tlength); // missing: what the client may send now
        if version >= 9 {
            t.u32(maxlength);
            t.u32(tlength);
            t.u32(prebuf);
            t.u32(minreq);
        }
        if version >= 12 {
            t.sample_spec(format, channels, rate);
            t.channel_map(&map);
            t.u32(0); // sink index
            t.string(Some("airos"));
            t.boolean(false); // suspended
        }
        if version >= 13 {
            t.usec(INITIAL_CREDIT_MS * 1000);
        }
        if version >= 21 {
            t.format_info_pcm();
        }
        self.send(out, t);
        logf!("pulse: playback stream {idx}: {rate} Hz, {channels} channels");
        Ok(())
    }

    fn audio(&self, channel: u32, data: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if let Some(s) = st.streams.get_mut(&channel) {
            s.queue.extend(data);
            s.write_index += data.len() as u64;
            s.requested = s.requested.saturating_sub(data.len() as u64);
            if !s.started && s.queue.len() as u64 >= s.tlength as u64 / 2 {
                s.started = true;
            }
        }
        drop(st);
        self.wake.notify_all();
    }

    /// Moves queued audio to the host as credit allows and asks clients for
    /// what was moved.
    fn pump(self: Arc<Self>) {
        const CHUNK: usize = 8192;
        loop {
            let mut st = self.state.lock().unwrap();
            let mut work: Vec<(u32, Vec<u8>)> = Vec::new();
            let mut requests: Vec<(u32, u32, u32)> = Vec::new(); // client, stream, bytes
            let mut started: Vec<(u32, u32)> = Vec::new();
            let mut drained: Vec<(u32, u32)> = Vec::new();
            let ids: Vec<u32> = st.streams.keys().copied().collect();
            for id in ids {
                let credit = {
                    let s = &st.streams[&id];
                    st.credit.get(&s.host_id).copied().unwrap_or(0)
                };
                let s = st.streams.get_mut(&id).unwrap();
                if s.corked || (!s.started && s.drain_tags.is_empty()) {
                    continue;
                }
                let n = (credit as usize).min(s.queue.len()).min(CHUNK) / 4 * 4;
                if n > 0 {
                    let was_unplayed = s.read_index == 0;
                    let chunk: Vec<u8> = s.queue.drain(..n).collect();
                    s.read_index += n as u64;
                    let host_id = s.host_id;
                    work.push((host_id, chunk));
                    // keep the client's buffer at tlength
                    let want = (s.tlength as u64).saturating_sub(s.queue.len() as u64 + s.requested);
                    if want >= (s.tlength / 4) as u64 {
                        s.requested += want;
                        requests.push((s.client, id, want as u32));
                    }
                    if was_unplayed {
                        started.push((s.client, id));
                    }
                    *st.credit.get_mut(&host_id).unwrap() -= n as u64;
                }
                let s = st.streams.get_mut(&id).unwrap();
                if s.queue.is_empty() && !s.drain_tags.is_empty() {
                    for t in s.drain_tags.drain(..) {
                        drained.push((s.client, t));
                    }
                }
            }
            let outs: HashMap<u32, Arc<Mutex<UnixStream>>> = st.clients.iter().map(|(k, c)| (*k, c.out.clone())).collect();
            let versions: HashMap<u32, u32> = st.clients.iter().map(|(k, c)| (*k, c.version)).collect();
            if work.is_empty() && requests.is_empty() && drained.is_empty() {
                let _ = self.wake.wait_timeout(st, Duration::from_millis(20));
                continue;
            }
            drop(st);
            for (host_id, data) in work {
                self.link.send(&ToHost::AudioData { stream: host_id, data });
            }
            for (cid, stream, bytes) in requests {
                if let Some(o) = outs.get(&cid) {
                    let c = Client { out: o.clone(), version: versions[&cid] };
                    let mut t = Tags::command(cmd::REQUEST, u32::MAX);
                    t.u32(stream);
                    t.u32(bytes);
                    self.send(&c, t);
                }
            }
            for (cid, stream) in started {
                if let Some(o) = outs.get(&cid) {
                    if versions[&cid] >= 13 {
                        let c = Client { out: o.clone(), version: versions[&cid] };
                        let mut t = Tags::command(cmd::STARTED, u32::MAX);
                        t.u32(stream);
                        self.send(&c, t);
                    }
                }
            }
            for (cid, tag) in drained {
                if let Some(o) = outs.get(&cid) {
                    let c = Client { out: o.clone(), version: versions[&cid] };
                    self.reply_ok(&c, tag);
                }
            }
        }
    }

    fn serve_client(self: Arc<Self>, mut sock: UnixStream) {
        let out = match sock.try_clone() {
            Ok(o) => Arc::new(Mutex::new(o)),
            Err(_) => return,
        };
        let cid = {
            let mut st = self.state.lock().unwrap();
            st.next_client += 1;
            let id = st.next_client;
            st.clients.insert(id, Client { out, version: PROTOCOL_VERSION });
            id
        };
        let mut hdr = [0u8; 20];
        loop {
            if sock.read_exact(&mut hdr).is_err() {
                break;
            }
            let be = |i: usize| u32::from_be_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
            let (len, channel, flags) = (be(0) as usize, be(1), be(4));
            if len > MAX_FRAME {
                break;
            }
            let mut payload = vec![0u8; len];
            if sock.read_exact(&mut payload).is_err() {
                break;
            }
            if channel == COMMAND_CHANNEL {
                if let Err(e) = self.handle_command(cid, &payload) {
                    logf!("pulse: client {cid}: {e}");
                    let st = self.state.lock().unwrap();
                    if let Some(c) = st.clients.get(&cid) {
                        let c = Client { out: c.out.clone(), version: c.version };
                        drop(st);
                        self.error(&c, 0, ERR_PROTOCOL);
                    }
                }
            } else if flags & 0xff00_0000 == 0 {
                self.audio(channel, &payload);
            }
        }
        // client gone: its streams end
        let mut st = self.state.lock().unwrap();
        st.clients.remove(&cid);
        let gone: Vec<u32> = st.streams.iter().filter(|(_, s)| s.client == cid).map(|(k, _)| *k).collect();
        for k in gone {
            if let Some(s) = st.streams.remove(&k) {
                st.credit.remove(&s.host_id);
                self.link.send(&ToHost::AudioClose { stream: s.host_id });
            }
        }
    }
}

/// Starts the server on `socket`; returns it so host credits can reach it.
pub fn start(link: HostLink, socket: &str) -> io::Result<Arc<Pulse>> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let _ = crate::sys::chmod(socket, 0o777);
    let pulse = Arc::new(Pulse { state: Mutex::new(State::default()), wake: Condvar::new(), link });
    {
        let p = pulse.clone();
        std::thread::Builder::new().name("pulse-pump".into()).spawn(move || p.pump())?;
    }
    {
        let p = pulse.clone();
        std::thread::Builder::new().name("pulse".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let p = p.clone();
                let _ = std::thread::Builder::new().name("pulse-client".into()).spawn(move || p.serve_client(conn));
            }
        })?;
    }
    logf!("pulse server listening on {socket}");
    Ok(pulse)
}
