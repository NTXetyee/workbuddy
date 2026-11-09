//! Prints the USB devices reported by the local usbmuxd (Apple Mobile Device Service).
//! Usage: cargo run -p wb-usbmux --example list

use wb_usbmux::Usbmux;

fn main() -> std::io::Result<()> {
    let devices = Usbmux::new().list_devices()?;
    if devices.is_empty() {
        println!("no USB devices");
    }
    for d in devices {
        println!("id={} serial={}", d.id, d.serial);
    }
    Ok(())
}
