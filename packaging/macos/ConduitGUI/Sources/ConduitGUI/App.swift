import AppKit
import SwiftUI
import Combine

/// Shared UI state. The snapshot and live stats are replaced wholesale by the
/// streamed messages; SwiftUI re-renders what changed.
final class AppModel: ObservableObject {
    @Published var snapshot: JSON = .object([:])
    @Published var stats: JSON = .object([:])
    @Published var toast: String?

    // Local AI (Ollama) page state.
    @Published var aiStatus: JSON = .object([:])
    @Published var aiProbe: JSON = .object([:])
    @Published var aiStorage: JSON = .null
    @Published var aiOutput = ""
    @Published var aiRunning = false
    // One-key setup progress.
    @Published var aiBusy = false
    @Published var aiPaused = false
    @Published var aiPct: Double = -1 // < 0 = indeterminate
    @Published var aiStage = ""
    @Published var aiLog = ""

    var aiRecModel: String { aiProbe["model"].string ?? "" }

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

    // ---- AI setup control (mirrors the WinUI/GTK flow) --------------------

    func startAiSetup() {
        guard !aiBusy else { return } // never fire a second concurrent run
        aiBusy = true
        aiPaused = false
        aiPct = -1
        aiStage = Loc.t("ai_setup_checking")
        aiLog = ""
        var extra: [String: JSON] = [:]
        if !aiRecModel.isEmpty { extra["model"] = .string(aiRecModel) }
        cmd("ai_setup", extra)
    }

    func pauseAiSetup() {
        guard aiBusy else { return }
        aiPaused.toggle()
        cmd(aiPaused ? "ai_setup_pause" : "ai_setup_resume")
    }

    func stopAiSetup() {
        guard aiBusy else { return }
        cmd("ai_setup_cancel")
    }

    func runAi(model: String, prompt: String) {
        guard !model.isEmpty, !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        aiRunning = true
        aiOutput = ""
        cmd("ai_generate", ["model": .string(model), "prompt": .string(prompt)])
    }

    /// Fold a streamed `ai_setup` event into the published state.
    func applyAiSetup(_ d: JSON) {
        let pctStr = { (p: Double) in String(Int(p)) }
        switch d["stage"].string ?? "" {
        case "log": pushAiLog(d["line"].string ?? "")
        case "paused": aiPaused = true; aiStage = Loc.t("ai_setup_paused")
        case "resumed": aiPaused = false
        case "download":
            aiPaused = false
            aiPct = d["pct"].double ?? 0
            let total = d["total"].double ?? 0, done = d["done"].double ?? 0
            aiStage = total > 0
                ? Loc.f("ai_setup_downloading_size", ["pct": pctStr(aiPct), "done": Fmt.bytes(done), "total": Fmt.bytes(total)])
                : Loc.f("ai_setup_downloading", ["pct": pctStr(aiPct)])
        case "install": aiPct = -1; aiStage = Loc.t("ai_setup_installing")
        case "starting": aiPct = -1; aiStage = Loc.t("ai_setup_starting")
        case "pull":
            aiPaused = false
            aiPct = d["pct"].double ?? 0
            aiStage = Loc.f("ai_setup_pulling", ["pct": pctStr(aiPct)])
        case "done":
            aiBusy = false; aiPaused = false; aiPct = -1
            aiStage = Loc.t("ai_setup_done"); pushAiLog(aiStage)
        case "cancelled":
            aiBusy = false; aiPaused = false; aiPct = -1
            aiStage = Loc.t("ai_setup_stopped"); pushAiLog(aiStage)
        case "error":
            aiBusy = false; aiPaused = false; aiPct = -1
            aiStage = Loc.f("ai_setup_failed", ["reason": d["error"].string ?? ""]); pushAiLog(aiStage)
        default: break
        }
    }

    private func pushAiLog(_ line: String) {
        guard !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        if !aiLog.isEmpty { aiLog += "\n" }
        aiLog += line
        // Bound to the last 500 lines.
        let lines = aiLog.split(separator: "\n", omittingEmptySubsequences: false)
        if lines.count > 500 { aiLog = lines.suffix(500).joined(separator: "\n") }
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
        case "ai_status":
            model.aiStatus = data
        case "ai_probe":
            model.aiProbe = data
        case "ai_setup":
            model.applyAiSetup(data)
        case "ai_result":
            model.aiRunning = false
            model.aiOutput = data["ok"].boolValue ? (data["text"].string ?? "") : (data["error"].string ?? "error")
        case "ai_storage":
            model.aiStorage = data
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
