import AppKit
import SwiftUI

/// One floating window shows queued consent requests one at a time. Deny is the
/// default; Allow arms only after a short delay so a stray click or key-repeat
/// can't approve anything. Mirrors gui-gtk/src/consent.rs.
final class ConsentState: ObservableObject {
    @Published var current: JSON = .null
    @Published var allowArmed = false
    var onAnswer: ((_ allow: Bool, _ perms: [String], _ remember: Bool) -> Void)?
}

final class ConsentController: NSObject, NSWindowDelegate {
    private let model: AppModel
    private let state = ConsentState()
    private var window: NSWindow?
    private var queue: [JSON] = []
    private var currentId: UInt64?
    private var armTimer: Timer?

    init(model: AppModel) {
        self.model = model
        super.init()
        state.onAnswer = { [weak self] allow, perms, remember in
            self?.answer(allow: allow, perms: perms, remember: remember)
        }
    }

    private func id(of req: JSON) -> UInt64 { UInt64(req["id"].int64 ?? 0) }

    func add(_ req: JSON) {
        let rid = id(of: req)
        if currentId == rid || queue.contains(where: { id(of: $0) == rid }) { return } // dedup
        queue.append(req)
        if currentId == nil { showNext() }
    }

    func remove(_ rid: UInt64) {
        queue.removeAll { id(of: $0) == rid }
        if currentId == rid { currentId = nil; showNext() }
    }

    private func answer(allow: Bool, perms: [String], remember: Bool) {
        guard let rid = currentId else { return }
        model.cmd("consent", [
            "id": .number(Double(rid)),
            "allow": .bool(allow),
            "perms": .array(perms.map { .string($0) }),
            "remember": .bool(remember),
        ])
        currentId = nil
        showNext()
    }

    private func showNext() {
        armTimer?.invalidate()
        guard !queue.isEmpty else { window?.orderOut(nil); return }
        let req = queue.removeFirst()
        currentId = id(of: req)
        state.current = req
        state.allowArmed = false
        ensureWindow()
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        armTimer = Timer.scheduledTimer(withTimeInterval: 0.7, repeats: false) { [weak self] _ in
            self?.state.allowArmed = true
        }
    }

    private func ensureWindow() {
        if window != nil { return }
        let host = NSHostingController(rootView: ConsentView(state: state))
        let w = NSWindow(contentViewController: host)
        w.title = Loc.t("app")
        w.styleMask = [.titled, .closable]
        w.level = .floating
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.center()
        window = w
    }

    // Clicking the window's close button denies whatever is showing; the window
    // itself is kept for the next prompt (showNext hides it when the queue empties).
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        answer(allow: false, perms: [], remember: false)
        return false
    }
}

private struct ConsentView: View {
    @ObservedObject var state: ConsentState
    @State private var perms: [String: Bool] = [:]
    @State private var remember = false

    var body: some View {
        let req = state.current
        let origin = req["origin"].string ?? ""
        let kind = req["kind"].string ?? ""
        let detail = req["detail"].string ?? ""
        let pairPerms = req["perms"].array.compactMap { $0.string }

        return VStack(alignment: .leading, spacing: 12) {
            Text(origin).font(.title3.weight(.semibold)).fixedSize(horizontal: false, vertical: true)
            Text(message(kind: kind, origin: origin, detail: detail))
                .fixedSize(horizontal: false, vertical: true)

            if kind == "pair" {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(pairPerms, id: \.self) { p in
                        Toggle(Loc.t("perm_" + p), isOn: Binding(
                            get: { perms[p] ?? true },
                            set: { perms[p] = $0 }
                        ))
                    }
                }
            }
            if kind == "pair" || kind == "launch" {
                Toggle(Loc.t("remember"), isOn: $remember)
            }

            HStack {
                Spacer()
                Button(Loc.t("deny")) { state.onAnswer?(false, [], false) }
                    .keyboardShortcut(.cancelAction)
                Button(Loc.t("allow")) {
                    let picked = kind == "pair" ? pairPerms.filter { perms[$0] ?? true } : []
                    state.onAnswer?(true, picked, remember)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!state.allowArmed)
            }
        }
        .padding(20)
        .frame(width: 420)
        .environment(\.layoutDirection, Loc.rtl ? .rightToLeft : .leftToRight)
        .onChange(of: idValue) { _ in perms = [:]; remember = false }
    }

    private var idValue: Double { Double(state.current["id"].int64 ?? 0) }

    private func message(kind: String, origin: String, detail: String) -> String {
        switch kind {
        case "pair": return Loc.f("consent_pair", ["origin": origin])
        case "launch": return Loc.f("consent_launch", ["origin": origin]) + "\n" + detail
        case "kill": return Loc.f("consent_kill", ["origin": origin]) + "\n" + detail
        case "power": return Loc.t("consent_power") + " " + detail
        case "clipboard": return Loc.f("consent_clipboard", ["origin": origin])
        case "elevate": return Loc.f("consent_elevate", ["origin": origin])
        case "hostwrite": return Loc.f("consent_hostwrite", ["origin": origin, "verb": detail])
        default: return kind + " " + detail
        }
    }
}
