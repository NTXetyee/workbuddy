//! Tests against an in-process mock usbmuxd. No hardware required.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use plist::{Dictionary, Integer, Value};
use wb_usbmux::{Device, Usbmux};

#[derive(Clone)]
struct MockDevice {
    id: u32,
    serial: String,
    connection: String,
}

#[derive(Default)]
struct MockState {
    devices: Vec<MockDevice>,
    connect_result: u64,
    connect_ports: Vec<u64>,
    connect_device_ids: Vec<u64>,
}

fn start_mock(state: Arc<Mutex<MockState>>) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let state = Arc::clone(&state);
            thread::spawn(move || {
                let _ = serve(stream, &state);
            });
        }
    });
    addr
}

fn serve(mut s: TcpStream, state: &Mutex<MockState>) -> io::Result<()> {
    let (tag, req) = read_packet(&mut s)?;
    let msg = req
        .get("MessageType")
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_owned();
    match msg.as_str() {
        "ListDevices" => {
            let devices = state.lock().unwrap().devices.clone();
            let list: Vec<Value> = devices
                .iter()
                .map(|d| {
                    let mut props = Dictionary::new();
                    props.insert("SerialNumber".into(), Value::String(d.serial.clone()));
                    props.insert("ConnectionType".into(), Value::String(d.connection.clone()));
                    props.insert("DeviceID".into(), int(d.id as u64));
                    let mut entry = Dictionary::new();
                    entry.insert("MessageType".into(), Value::String("Attached".into()));
                    entry.insert("DeviceID".into(), int(d.id as u64));
                    entry.insert("Properties".into(), Value::Dictionary(props));
                    Value::Dictionary(entry)
                })
                .collect();
            let mut reply = Dictionary::new();
            reply.insert("DeviceList".into(), Value::Array(list));
            write_packet(&mut s, tag, &reply)?;
        }
        "Connect" => {
            let device_id = as_u64(req.get("DeviceID"));
            let port = as_u64(req.get("PortNumber"));
            let number = {
                let mut st = state.lock().unwrap();
                st.connect_device_ids.push(device_id);
                st.connect_ports.push(port);
                st.connect_result
            };
            let mut reply = Dictionary::new();
            reply.insert("MessageType".into(), Value::String("Result".into()));
            reply.insert("Number".into(), int(number));
            write_packet(&mut s, tag, &reply)?;
            if number == 0 {
                // Tunnel: echo bytes until the client closes.
                let mut buf = [0u8; 1024];
                loop {
                    let n = s.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    s.write_all(&buf[..n])?;
                }
            }
        }
        other => panic!("mock got unexpected MessageType {other:?}"),
    }
    Ok(())
}

fn int(v: u64) -> Value {
    Value::Integer(Integer::from(v))
}

fn as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Integer(i)) => i.as_unsigned().expect("integer fits u64"),
        other => panic!("expected integer, got {other:?}"),
    }
}

fn write_packet(w: &mut impl Write, tag: u32, body: &Dictionary) -> io::Result<()> {
    let mut payload = Vec::new();
    plist::to_writer_xml(&mut payload, body).unwrap();
    let total = (16 + payload.len()) as u32;
    let mut header = Vec::with_capacity(16);
    header.extend_from_slice(&total.to_le_bytes());
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&8u32.to_le_bytes());
    header.extend_from_slice(&tag.to_le_bytes());
    w.write_all(&header)?;
    w.write_all(&payload)
}

fn read_packet(r: &mut impl Read) -> io::Result<(u32, Dictionary)> {
    let mut header = [0u8; 16];
    r.read_exact(&mut header)?;
    let total = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    let tag = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let mut payload = vec![0u8; total - 16];
    r.read_exact(&mut payload)?;
    Ok((tag, plist::from_bytes(&payload).unwrap()))
}

fn mock_device(id: u32, serial: &str, connection: &str) -> MockDevice {
    MockDevice {
        id,
        serial: serial.to_owned(),
        connection: connection.to_owned(),
    }
}

fn new_state() -> Arc<Mutex<MockState>> {
    Arc::new(Mutex::new(MockState::default()))
}

#[test]
fn list_devices_keeps_only_usb() {
    let state = new_state();
    state.lock().unwrap().devices = vec![
        mock_device(3, "UDID-USB", "USB"),
        mock_device(4, "UDID-NET", "Network"),
    ];
    let mux = Usbmux::with_addr(start_mock(state));
    let devices = mux.list_devices().unwrap();
    assert_eq!(
        devices,
        vec![Device {
            id: 3,
            serial: "UDID-USB".to_owned()
        }]
    );
}

#[test]
fn list_devices_empty() {
    let state = new_state();
    let mux = Usbmux::with_addr(start_mock(state));
    assert!(mux.list_devices().unwrap().is_empty());
}

#[test]
fn connect_success_tunnels_bytes() {
    let state = new_state();
    let mux = Usbmux::with_addr(start_mock(Arc::clone(&state)));
    let mut stream = mux.connect(3, 7878).unwrap();

    assert!(stream.read_timeout().unwrap().is_none());
    assert!(stream.write_timeout().unwrap().is_none());
    assert!(stream.nodelay().unwrap());

    stream.write_all(b"hello tunnel").unwrap();
    let mut echoed = [0u8; 12];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"hello tunnel");

    let st = state.lock().unwrap();
    assert_eq!(st.connect_device_ids, vec![3]);
    assert_eq!(st.connect_ports, vec![7878u16.swap_bytes() as u64]);
    assert_eq!(st.connect_ports[0], 50718);
}

#[test]
fn connect_refused_returns_error() {
    let state = new_state();
    state.lock().unwrap().connect_result = 3;
    let mux = Usbmux::with_addr(start_mock(state));
    let err = mux.connect(3, 7878).expect_err("connect must fail");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    assert!(err.to_string().contains('3'), "message: {err}");
}

#[test]
fn connect_other_error_returns_error() {
    let state = new_state();
    state.lock().unwrap().connect_result = 2;
    let mux = Usbmux::with_addr(start_mock(state));
    let err = mux.connect(9, 7878).expect_err("connect must fail");
    assert!(err.to_string().contains('2'), "message: {err}");
}

#[test]
fn wait_for_device_returns_when_device_appears() {
    let state = new_state();
    let mux = Usbmux::with_addr(start_mock(Arc::clone(&state)));
    let stop = AtomicBool::new(false);

    let adder = {
        let state = Arc::clone(&state);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            state
                .lock()
                .unwrap()
                .devices
                .push(mock_device(7, "ABC123", "USB"));
        })
    };

    let found = mux.wait_for_device(20, &stop).unwrap();
    adder.join().unwrap();
    assert_eq!(
        found,
        Some(Device {
            id: 7,
            serial: "ABC123".to_owned()
        })
    );
}

#[test]
fn wait_for_device_returns_none_when_stop_set() {
    let state = new_state();
    let mux = Usbmux::with_addr(start_mock(state));
    let stop = AtomicBool::new(true);
    assert_eq!(mux.wait_for_device(20, &stop).unwrap(), None);
}

#[test]
fn wait_for_device_returns_none_when_stopped_while_polling() {
    let state = new_state();
    let mux = Usbmux::with_addr(start_mock(state));
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(100));
            stop.store(true, Ordering::SeqCst);
        });
        assert_eq!(mux.wait_for_device(20, &stop).unwrap(), None);
    });
}

#[test]
fn unreachable_muxd_is_not_an_error_for_wait_for_device() {
    // Bind then drop to get a port nobody listens on.
    let addr = {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap()
    };
    let mux = Usbmux::with_addr(addr);
    assert!(mux.list_devices().is_err());

    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::SeqCst);
        });
        assert_eq!(mux.wait_for_device(20, &stop).unwrap(), None);
    });
}
