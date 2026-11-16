import SwiftUI

@main
struct LunchTrayApp: App {
    @State private var model = TrayModel()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(model)
                .preferredColorScheme(model.theme.scheme)
                .tint(Palette.accent)
        }
        .onChange(of: phase, initial: true) { _, phase in
            // The socket only lives while the app is in front; coming back
            // catches up over HTTP first.
            switch phase {
            case .active: model.start()
            case .background: model.stop()
            default: break
            }
        }
    }
}
