//! Minimal usbmuxd client speaking the plist protocol over TCP.
//!
//! On Windows the usbmuxd replacement is provided by Apple Mobile Device Service and
//! listens on `127.0.0.1:27015`. Every request opens a fresh TCP connection; a
//! successful `Connect` turns that connection into a raw tunnel to the device port.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use plist::{Dictionary, Integer, Value};

const DEFAULT_PORT: u16 = 27015;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const HEADER_LEN: usize = 16;
const PROTOCOL_VERSION: u32 = 1;
const MESSAGE_TYPE_PLIST: u32 = 8;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const CONNECTION_REFUSED_CODE: u64 = 3;
const SLEEP_SLICE: Duration = Duration::from_millis(20);

static NEXT_TAG: AtomicU32 = AtomicU32::new(1);

/// A USB-attached device known to usbmuxd.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: u32,
    pub serial: String,
}

/// Client handle for a usbmuxd endpoint.
#[derive(Debug, Clone)]
pub struct Usbmux {
    addr: SocketAddr,
}

impl Usbmux {
    /// Client for the default Apple Mobile Device Service endpoint, `127.0.0.1:27015`.
    pub fn new() -> Self {
        Self::with_addr(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_PORT,
        ))
    }

    /// Client for a usbmuxd endpoint at an arbitrary address.
    pub fn with_addr(addr: SocketAddr) -> Self {
        Self { addr }
    }

    /// Lists USB-attached devices. Network-attached devices are filtered out.
    pub fn list_devices(&self) -> io::Result<Vec<Device>> {
        let mut stream = self.open()?;
        let reply = transact(&mut stream, request("ListDevices"))?;
        let list = reply
            .get("DeviceList")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("ListDevices reply has no DeviceList array"))?;

        let mut devices = Vec::new();
        for entry in list {
            let Some(entry) = entry.as_dictionary() else {
                continue;
            };
            let Some(props) = entry.get("Properties").and_then(Value::as_dictionary) else {
                continue;
            };
            if props.get("ConnectionType").and_then(Value::as_string) != Some("USB") {
                continue;
            }
            let id = entry
                .get("DeviceID")
                .and_then(as_u32)
                .or_else(|| props.get("DeviceID").and_then(as_u32))
                .ok_or_else(|| invalid("USB device entry has no DeviceID"))?;
            let serial = props
                .get("SerialNumber")
                .and_then(Value::as_string)
                .unwrap_or_default()
                .to_owned();
            devices.push(Device { id, serial });
        }
        Ok(devices)
    }

    /// Opens a tunnel to `port` on the device. On success the returned stream is a raw
    /// bidirectional connection to the device; timeouts are cleared and `TCP_NODELAY` is set.
    pub fn connect(&self, device_id: u32, port: u16) -> io::Result<TcpStream> {
        let mut stream = self.open()?;
        let mut req = request("Connect");
        req.insert(
            "DeviceID".to_owned(),
            Value::Integer(Integer::from(device_id)),
        );
        req.insert(
            "PortNumber".to_owned(),
            Value::Integer(Integer::from(port.swap_bytes() as u32)),
        );
        let reply = transact(&mut stream, req)?;
        let number = reply
            .get("Number")
            .and_then(as_u64)
            .ok_or_else(|| invalid("Connect reply has no Number"))?;
        if number != 0 {
            let kind = if number == CONNECTION_REFUSED_CODE {
                io::ErrorKind::ConnectionRefused
            } else {
                io::ErrorKind::Other
            };
            return Err(io::Error::new(
                kind,
                format!(
                    "usbmuxd Connect to device {device_id} port {port} failed with code {number}"
                ),
            ));
        }
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// Polls `list_devices` every `poll_ms` until a USB device appears (returns the first one),
    /// `stop` becomes true (returns `Ok(None)`), or a non-transient error occurs.
    /// An unreachable muxd is treated as "no device yet" and polling continues.
    pub fn wait_for_device(&self, poll_ms: u64, stop: &AtomicBool) -> io::Result<Option<Device>> {
        let poll = Duration::from_millis(poll_ms);
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            match self.list_devices() {
                Ok(mut devices) => {
                    if !devices.is_empty() {
                        return Ok(Some(devices.swap_remove(0)));
                    }
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => return Err(e),
            }
            sleep_unless_stopped(poll, stop);
        }
    }

    fn open(&self) -> io::Result<TcpStream> {
        let stream = TcpStream::connect_timeout(&self.addr, IO_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(stream)
    }
}

impl Default for Usbmux {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors that mean "muxd not reachable right now" rather than a protocol failure.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::UnexpectedEof
    )
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + total;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(SLEEP_SLICE));
    }
}

fn request(msg_type: &str) -> Dictionary {
    let mut d = Dictionary::new();
    d.insert("MessageType".to_owned(), Value::String(msg_type.to_owned()));
    d.insert(
        "ClientVersionString".to_owned(),
        Value::String("workbuddy".to_owned()),
    );
    d.insert("ProgName".to_owned(), Value::String("workbuddy".to_owned()));
    d.insert(
        "kLibUSBMuxVersion".to_owned(),
        Value::Integer(Integer::from(3u32)),
    );
    d
}

fn transact(stream: &mut TcpStream, body: Dictionary) -> io::Result<Dictionary> {
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    write_packet(stream, tag, &body)?;
    let (_, reply) = read_packet(stream)?;
    Ok(reply)
}

fn write_packet(w: &mut impl Write, tag: u32, body: &Dictionary) -> io::Result<()> {
    let mut payload = Vec::new();
    plist::to_writer_xml(&mut payload, body).map_err(|e| invalid(&e.to_string()))?;
    let total = u32::try_from(HEADER_LEN + payload.len())
        .map_err(|_| invalid("usbmuxd request too large"))?;

    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&total.to_le_bytes());
    header[4..8].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&MESSAGE_TYPE_PLIST.to_le_bytes());
    header[12..16].copy_from_slice(&tag.to_le_bytes());

    w.write_all(&header)?;
    w.write_all(&payload)?;
    w.flush()
}

fn read_packet(r: &mut impl Read) -> io::Result<(u32, Dictionary)> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header)?;
    let word =
        |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let total = word(0) as usize;
    let version = word(4);
    let msg_type = word(8);
    let tag = word(12);

    if version != PROTOCOL_VERSION || msg_type != MESSAGE_TYPE_PLIST {
        return Err(invalid("unexpected usbmuxd packet version or type"));
    }
    if total < HEADER_LEN || total - HEADER_LEN > MAX_PAYLOAD {
        return Err(invalid("usbmuxd packet length out of range"));
    }
    let mut payload = vec![0u8; total - HEADER_LEN];
    r.read_exact(&mut payload)?;
    let dict: Dictionary =
        plist::from_bytes(&payload).map_err(|e| invalid(&format!("bad usbmuxd plist: {e}")))?;
    Ok((tag, dict))
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Integer(i) => i
            .as_unsigned()
            .or_else(|| i.as_signed().and_then(|s| u64::try_from(s).ok())),
        _ => None,
    }
}

fn as_u32(v: &Value) -> Option<u32> {
    as_u64(v).and_then(|n| u32::try_from(n).ok())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}
