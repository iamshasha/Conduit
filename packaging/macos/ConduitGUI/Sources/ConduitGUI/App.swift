import AppKit
import SwiftUI
import Combine

/// Shared UI state. The snapshot and live stats are replaced wholesale by the
/// streamed messages; SwiftUI re-renders what changed.
final class AppModel: ObservableObject {
    @Published var snapshot: JSON = .object([:])
    @Published var stats: JSON = .object([:])
    @Published var toast: String?

    private let bus: Bus
    init(bus: Bus) { self.bus = bus }

    func cmd(_ c: String, _ extra: [String: JSON] = [:]) { bus.cmd(c, extra) }

    func showToast(_ text: String) {
        toast = text
        let mine = text
        DispatchQueue.main.asyncAfter(deadline: .now() + 4) { [weak self] in
            if self?.toast == mine { self?.toast = nil }
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private let pipe: String
    private let key: String
    private let dark: Bool
    private let bus = Bus()
    private var model: AppModel!
    private var dashboard: NSWindow?
    private var consent: ConsentController?

    init(pipe: String, key: String, dark: Bool) {
        self.pipe = pipe; self.key = key; self.dark = dark
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        if dark { NSApp.appearance = NSAppearance(named: .darkAqua) }
        model = AppModel(bus: bus)
        bus.onMessage = { [weak self] msg in self?.dispatch(msg) }
        bus.onClose = { NSApp.terminate(nil) } // core went away
        if !bus.connect(path: pipe, key: key) {
            FileHandle.standardError.write(Data("conduit-gui: cannot connect to \(pipe)\n".utf8))
            NSApp.terminate(nil)
        }
    }

    private func dispatch(_ msg: JSON) {
        let data = msg["data"]
        switch msg["type"].string ?? "" {
        case "open_main":
            model.snapshot = data
            showDashboard()
        case "snapshot":
            model.snapshot = data
        case "stats":
            model.stats = data
        case "toast":
            model.showToast(Loc.t(data.string ?? ""))
        case "settings":
            if let theme = data["theme"].string {
                NSApp.appearance = NSAppearance(named: theme == "dark" ? .darkAqua : .aqua)
            }
        case "consent_add":
            consentController().add(data)
        case "consents":
            for r in data.array { consentController().add(r) }
        case "consent_gone":
            consent?.remove(UInt64(data.int64 ?? 0))
        default:
            break
        }
    }

    private func showDashboard() {
        if let w = dashboard {
            w.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        let host = NSHostingController(rootView: DashboardView().environmentObject(model))
        let w = NSWindow(contentViewController: host)
        w.title = Loc.t("app")
        w.setContentSize(NSSize(width: 900, height: 640))
        w.styleMask = [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.minSize = NSSize(width: 640, height: 460)
        w.center()
        w.delegate = self
        dashboard = w
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func consentController() -> ConsentController {
        if let c = consent { return c }
        let c = ConsentController(model: model)
        consent = c
        return c
    }

    // Closing the dashboard tells the core, but the process stays alive to serve
    // future prompts (it exits when the socket closes).
    func windowWillClose(_ notification: Notification) {
        if (notification.object as? NSWindow) === dashboard {
            model.cmd("main_closed")
            dashboard = nil
        }
    }
}
