import SwiftUI
import UIKit

@main
struct WorkBuddyApp: App {
    @StateObject private var session: Session
    private let renderer: DisplayRenderer

    init() {
        let decoder = VideoDecoder()
        let renderer = DisplayRenderer()
        self.renderer = renderer
        _session = StateObject(wrappedValue: Session(decoder: decoder, renderer: renderer))
    }

    var body: some Scene {
        WindowGroup {
            ContentView(session: session, renderer: renderer)
        }
    }
}

struct ContentView: View {
    @ObservedObject var session: Session
    let renderer: DisplayRenderer

    var body: some View {
        ZStack {
            Color.black
                .ignoresSafeArea()

            DisplayView(renderer: renderer)
                .ignoresSafeArea()

            if !session.streaming {
                Text(session.status)
                    .font(.footnote)
                    .foregroundColor(.gray)
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        .onAppear {
            UIApplication.shared.isIdleTimerDisabled = true
            session.start()
        }
    }
}
