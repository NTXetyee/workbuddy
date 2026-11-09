use std::time::{Duration, Instant};

use wb_capture::{list_outputs, Capturer, ID3D11Texture2D, OutputSel};
use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

#[test]
fn list_outputs_nonempty() {
    let outputs = list_outputs().expect("list_outputs failed");
    assert!(!outputs.is_empty(), "no outputs enumerated");
}

#[test]
fn capture_primary_frame() {
    let mut cap = Capturer::new(OutputSel::Primary).expect("Capturer::new failed");
    let deadline = Instant::now() + Duration::from_secs(3);
    let frame = loop {
        if let Some(f) = cap.next_frame(100).expect("next_frame failed") {
            break f;
        }
        assert!(Instant::now() < deadline, "no frame within 3 s");
    };
    let (w, h) = cap.size();
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!((frame.width, frame.height), (w, h));

    let tex: &ID3D11Texture2D = &frame.texture;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    assert_eq!(desc.Width, w);
    assert_eq!(desc.Height, h);
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
}
