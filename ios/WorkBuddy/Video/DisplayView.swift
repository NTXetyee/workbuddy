import Foundation
import Dispatch
import AVFoundation
import CoreMedia
import UIKit
import SwiftUI

/// Owns the AVSampleBufferDisplayLayer and feeds it decoded frames (SPEC 4.3).
/// `enqueue` and `flush` are thread-safe: all layer calls run on one private serial queue.
final class DisplayRenderer {

    /// Hosted by DisplayView. Frames carry DisplayImmediately, so no control timebase is used.
    let layer = AVSampleBufferDisplayLayer()

    private let queue = DispatchQueue(label: "wb.display.renderer", qos: .userInteractive)

    init() {
        layer.videoGravity = .resizeAspect
        layer.backgroundColor = UIColor.black.cgColor
    }

    /// Queues one decoded frame. Drops it if the layer cannot take more data.
    func enqueue(_ sb: CMSampleBuffer) {
        let layer = self.layer
        queue.async {
            if layer.status == .failed {
                layer.flush()
            }
            if !layer.isReadyForMoreMediaData {
                // Stale content is not worth waiting for: clear it and drop this frame.
                layer.flush()
                return
            }
            layer.enqueue(sb)
        }
    }

    /// Clears any queued frames (used on stream start/restart).
    func flush() {
        let layer = self.layer
        queue.async {
            layer.flush()
        }
    }
}

/// Black full-size view that hosts the renderer's display layer.
struct DisplayView: UIViewRepresentable {
    let renderer: DisplayRenderer

    func makeUIView(context: Context) -> DisplayUIView {
        let view = DisplayUIView()
        view.backgroundColor = .black
        view.attach(renderer.layer)
        return view
    }

    func updateUIView(_ uiView: DisplayUIView, context: Context) {
        uiView.attach(renderer.layer)
    }
}

/// UIView that keeps its hosted layer sized to its bounds without implicit animation.
final class DisplayUIView: UIView {

    private weak var hostedLayer: CALayer?

    func attach(_ hosted: CALayer) {
        if hostedLayer !== hosted {
            hostedLayer?.removeFromSuperlayer()
            layer.addSublayer(hosted)
            hostedLayer = hosted
        }
        setNeedsLayout()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        hostedLayer?.frame = bounds
        CATransaction.commit()
    }
}
