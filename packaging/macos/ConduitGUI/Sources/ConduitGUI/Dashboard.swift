import SwiftUI
import Combine
import AppKit
import UniformTypeIdentifiers

private enum Page: String, CaseIterable, Identifiable {
    case overview, sites, activity, ai, settings
    var id: String { rawValue }
    var label: String { Loc.t("nav_" + rawValue) }
    var symbol: String {
        switch self {
        case .overview: return "gauge.with.dots.needle.33percent"
        case .sites: return "globe"
        case .activity: return "list.bullet.rectangle"
        case .ai: return "brain"
        case .settings: return "gearshape"
        }
    }
}

struct DashboardView: View {
    @EnvironmentObject var model: AppModel
    @State private var page: Page? = .overview

    var body: some View {
        NavigationSplitView {
            List(Page.allCases, selection: $page) { p in
                Label(p.label, systemImage: p.symbol).tag(p)
            }
            .navigationSplitViewColumnWidth(200)
        } detail: {
            ZStack(alignment: .bottom) {
                Group {
                    switch page ?? .overview {
                    case .overview: OverviewPage()
                    case .sites: SitesPage()
                    case .activity: ActivityPage()
                    case .ai: AiPage()
                    case .settings: SettingsPage()
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)

                if let toast = model.toast {
                    Text(toast)
                        .padding(.horizontal, 16).padding(.vertical, 10)
                        .background(.thinMaterial, in: Capsule())
                        .padding(.bottom, 20)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .animation(.easeInOut(duration: 0.2), value: model.toast)
        }
        .environment(\.layoutDirection, Loc.rtl ? .rightToLeft : .leftToRight)
        .frame(minWidth: 640, minHeight: 460)
    }
}

/// Standard page scaffold: a big title and a scrolling column.
private struct PageScaffold<Content: View>: View {
    let title: String
    @ViewBuilder var content: () -> Content
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                Text(title).font(.largeTitle.weight(.semibold))
                content()
            }
            .frame(maxWidth: 820, alignment: .leading)
            .padding(28)
        }
    }
}

private struct Card<Content: View>: View {
    @ViewBuilder var content: () -> Content
    var body: some View {
        content()
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 12))
    }
}

// ---------------------------------------------------------------- overview

private struct OverviewPage: View {
    @EnvironmentObject var model: AppModel
    private let tick = Timer.publish(every: 1.5, on: .main, in: .common).autoconnect()

    var body: some View {
        let snap = model.snapshot
        let stats = model.stats
        let elevated = snap["elevated"].boolValue
        let port = snap["port"].int.map(String.init) ?? "8765"
        return PageScaffold(title: Loc.t("nav_overview")) {
            Card {
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(Loc.f("status_running", ["port": port])).font(.headline)
                        Text(elevated ? Loc.t("elevated") : Loc.t("not_elevated"))
                            .foregroundStyle(.secondary).font(.subheadline)
                    }
                    Spacer()
                    if !elevated {
                        Button(Loc.t("request_admin")) { model.cmd("elevate") }
                    }
                }
            }

            Grid(horizontalSpacing: 12, verticalSpacing: 12) {
                GridRow {
                    Stat(label: Loc.t("cpu"), value: stats["cpu"].double.map { String(format: "%.0f%%", $0) } ?? "—")
                    Stat(label: Loc.t("memory"),
                         value: statMem(stats))
                }
                GridRow {
                    Stat(label: Loc.t("uptime"),
                         value: stats["uptime"].int64.map { Fmt.uptime($0) } ?? "—")
                    Stat(label: Loc.t("battery"), value: statBattery(stats))
                }
            }
        }
        .onReceive(tick) { _ in model.cmd("stats") }
        .onAppear { model.cmd("stats") }
    }

    private func statMem(_ s: JSON) -> String {
        guard let used = s["mem_used"].double, let total = s["mem_total"].double, total > 0 else { return "—" }
        return "\(Fmt.bytes(used)) / \(Fmt.bytes(total))"
    }

    private func statBattery(_ s: JSON) -> String {
        let b = s["battery"]
        guard b["present"].boolValue else { return Loc.t("no_battery") }
        let pct = b["percent"].int ?? 0
        let key = b["charging"].boolValue ? "charging" : (b["on_ac"].boolValue ? "on_ac" : "on_battery")
        return "\(pct)%, \(Loc.t(key))"
    }
}

private struct Stat: View {
    let label: String
    let value: String
    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: 4) {
                Text(label).font(.caption).foregroundStyle(.secondary)
                Text(value).font(.title2.weight(.medium)).monospacedDigit()
            }
        }
    }
}

// ------------------------------------------------------------------- sites

private struct SitesPage: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        let grants = model.snapshot["grants"].array
        let allPerms = model.snapshot["perms"].array.compactMap { $0.string }
        return PageScaffold(title: Loc.t("nav_sites")) {
            if grants.isEmpty {
                Text(Loc.t("sites_none")).foregroundStyle(.secondary)
            } else {
                Text(Loc.t("sites_hint")).foregroundStyle(.secondary).font(.subheadline)
                ForEach(Array(grants.enumerated()), id: \.offset) { _, g in
                    SiteCard(grant: g, allPerms: allPerms)
                }
                Button(Loc.t("revoke_all"), role: .destructive) { model.cmd("revoke_all") }
                    .padding(.top, 6)
            }
        }
    }
}

private struct SiteCard: View {
    @EnvironmentObject var model: AppModel
    let grant: JSON
    let allPerms: [String]

    var body: some View {
        let origin = grant["origin"].string ?? ""
        let granted = Set(grant["perms"].array.compactMap { $0.string })
        let folders = grant["folders"].array
        return Card {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(Fmt.host(origin)).font(.headline)
                    Spacer()
                    Button(Loc.t("open_sandbox")) { model.cmd("open_sandbox", ["origin": .string(origin)]) }
                    Button(Loc.t("export_files")) { model.cmd("export_site", ["origin": .string(origin)]) }
                    Button(Loc.t("import_files")) { importZip(origin: origin) }
                    Button(Loc.t("revoke"), role: .destructive) { model.cmd("revoke", ["origin": .string(origin)]) }
                }
                if let exp = expiryText() {
                    Text("\(Loc.t("expires_label")) · \(exp)").font(.caption).foregroundStyle(.secondary)
                }
                Divider()
                ForEach(allPerms, id: \.self) { p in
                    Toggle(Loc.t("perm_" + p), isOn: Binding(
                        get: { granted.contains(p) },
                        set: { on in
                            var next = granted
                            if on { next.insert(p) } else { next.remove(p) }
                            model.cmd("set_perms", [
                                "origin": .string(origin),
                                "perms": .array(next.sorted().map { .string($0) }),
                            ])
                        }
                    ))
                    .toggleStyle(.switch)
                }
                if !folders.isEmpty {
                    Divider()
                    Text(Loc.t("folders_title")).font(.caption).foregroundStyle(.secondary)
                    ForEach(Array(folders.enumerated()), id: \.offset) { _, f in
                        HStack {
                            VStack(alignment: .leading, spacing: 1) {
                                Text(f["name"].string ?? "").font(.callout)
                                Text(f["path"].string ?? "").font(.caption2).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                            }
                            if f["read_only"].boolValue {
                                Text(Loc.t("read_only")).font(.caption2).foregroundStyle(.secondary)
                            }
                            Spacer()
                            Button(Loc.t("forget")) {
                                model.cmd("forget_folder", ["origin": .string(origin), "id": .string(f["id"].string ?? "")])
                            }
                        }
                    }
                }
            }
        }
    }

    private func expiryText() -> String? {
        if grant["session"].boolValue { return Loc.t("session_only") }
        guard let s = grant["expires_in"].int64 else { return nil }
        if s >= 86_400 { return "\(s / 86_400)d" }
        if s >= 3_600 { return "\(s / 3_600)h" }
        return "\(max(1, s / 60))m"
    }

    private func importZip(origin: String) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.zip]
        if panel.runModal() == .OK, let url = panel.url {
            model.cmd("import_site", ["origin": .string(origin), "path": .string(url.path)])
        }
    }
}

// ---------------------------------------------------------------- activity

private struct ActivityPage: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        let rows = model.snapshot["activity"].array
        return PageScaffold(title: Loc.t("nav_activity")) {
            if rows.isEmpty {
                Text(Loc.t("activity_none")).foregroundStyle(.secondary)
            } else {
                Button(Loc.t("clear")) { model.cmd("clear_activity") }
                ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                    HStack(spacing: 10) {
                        Circle().fill(r["ok"].boolValue ? Color.green : Color.red).frame(width: 8, height: 8)
                        Text(r["method"].string ?? "").font(.system(.body, design: .monospaced))
                        Text(Fmt.host(r["origin"].string ?? "")).foregroundStyle(.secondary)
                        Spacer()
                        Text(r["ok"].boolValue ? Loc.t("ok") : (r["code"].string ?? Loc.t("failed")))
                            .foregroundStyle(.secondary)
                    }
                    .padding(.vertical, 4)
                    Divider()
                }
            }
        }
    }
}

// --------------------------------------------------------------------- AI

private struct AiPage: View {
    @EnvironmentObject var model: AppModel
    @State private var endpoint = ""
    @State private var prompt = ""
    @State private var selected = ""
    @State private var didInit = false

    var body: some View {
        let status = model.aiStatus
        let online = status["online"].boolValue
        let models = status["models"].array.compactMap { $0.string }
        return PageScaffold(title: Loc.t("nav_ai")) {
            // Endpoint.
            Card {
                VStack(alignment: .leading, spacing: 6) {
                    Text(Loc.t("ai_endpoint")).font(.headline)
                    Text(Loc.t("ai_endpoint_hint")).font(.caption).foregroundStyle(.secondary)
                    HStack {
                        TextField("http://127.0.0.1:11434", text: $endpoint)
                            .textFieldStyle(.roundedBorder)
                        Button(Loc.t("ai_test")) { model.cmd("ai_endpoint", ["url": .string(endpoint)]) }
                    }
                }
            }

            // Status, recommendation, one-key setup with pause/stop/log.
            Card {
                VStack(alignment: .leading, spacing: 10) {
                    Text(online ? Loc.f("ai_online", ["n": String(models.count)]) : Loc.t("ai_offline"))
                        .font(.headline)
                    if !online { Text(Loc.t("ai_none")).font(.subheadline).foregroundStyle(.secondary) }
                    recommendation
                    HStack {
                        Button(setupLabel) { model.startAiSetup() }
                            .buttonStyle(.borderedProminent)
                            .disabled(model.aiBusy)
                        if model.aiBusy {
                            Button(model.aiPaused ? Loc.t("ai_resume") : Loc.t("ai_pause")) { model.pauseAiSetup() }
                            Button(Loc.t("ai_stop"), role: .destructive) { model.stopAiSetup() }
                        }
                    }
                    if model.aiBusy || !model.aiStage.isEmpty { setupProgress }
                    if !model.aiLog.isEmpty {
                        DisclosureGroup(Loc.t("ai_setup_logs")) {
                            ScrollView {
                                Text(model.aiLog)
                                    .font(.system(.caption, design: .monospaced))
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .textSelection(.enabled)
                            }
                            .frame(height: 140)
                        }
                    }
                }
            }

            // Model + prompt + run.
            Card {
                VStack(alignment: .leading, spacing: 8) {
                    Picker(Loc.t("ai_model"), selection: $selected) {
                        ForEach(models, id: \.self) { Text($0).tag($0) }
                    }
                    .disabled(models.isEmpty)
                    Text(Loc.t("ai_prompt")).font(.caption).foregroundStyle(.secondary)
                    TextEditor(text: $prompt)
                        .frame(height: 90)
                        .font(.body)
                        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.3)))
                    Button(model.aiRunning ? Loc.t("ai_running") : Loc.t("ai_run")) {
                        model.runAi(model: selected, prompt: prompt)
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(models.isEmpty || model.aiRunning)
                    if !model.aiOutput.isEmpty {
                        ScrollView {
                            Text(model.aiOutput)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .textSelection(.enabled)
                        }
                        .frame(height: 140)
                    }
                }
            }
        }
        .onAppear {
            if !didInit {
                endpoint = model.snapshot["settings"]["ai_endpoint"].string ?? ""
                didInit = true
            }
            model.cmd("ai_status")
            model.cmd("ai_probe")
        }
        .onChange(of: models) { newModels in
            if selected.isEmpty || !newModels.contains(selected) {
                selected = newModels.first ?? ""
            }
        }
    }

    private var setupLabel: String {
        let m = model.aiRecModel
        return m.isEmpty ? Loc.t("ai_setup") : Loc.f("ai_setup_model", ["model": m])
    }

    @ViewBuilder private var recommendation: some View {
        let p = model.aiProbe
        if let m = p["model"].string, !m.isEmpty {
            let device: String = {
                guard let g = p["gpu"].string else { return Loc.t("ai_no_gpu") }
                if let v = p["vram_gb"].double, v > 0 { return String(format: "%@ (%.1f GB)", g, v) }
                return g
            }()
            Text(Loc.f("ai_recommend", ["device": device, "model": m]))
                .font(.subheadline).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var setupProgress: some View {
        VStack(alignment: .leading, spacing: 4) {
            if model.aiPct >= 0 {
                ProgressView(value: min(max(model.aiPct / 100, 0), 1))
            } else if model.aiBusy {
                ProgressView()
            }
            if !model.aiStage.isEmpty {
                Text(model.aiStage).font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}

// ---------------------------------------------------------------- settings

private struct SettingsPage: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        let snap = model.snapshot
        let autostart = snap["autostart"].boolValue
        let protocolOn = snap["protocol"].boolValue
        return PageScaffold(title: Loc.t("nav_settings")) {
            Card {
                Toggle(Loc.t("autostart"), isOn: Binding(
                    get: { autostart },
                    set: { model.cmd("autostart", ["on": .bool($0)]) }
                )).toggleStyle(.switch)
            }
            Card {
                Toggle(Loc.t("protocol"), isOn: Binding(
                    get: { protocolOn },
                    set: { model.cmd("protocol", ["on": .bool($0)]) }
                )).toggleStyle(.switch)
            }
            Card {
                HStack {
                    Text(Loc.t("nav_settings"))
                    Spacer()
                    Button(Loc.t("open_sandbox")) { model.cmd("open_data") }
                }
            }
            aiStorageCard
        }
        .onAppear { model.cmd("ai_storage") }
    }

    /// Local AI (Ollama) storage: models and sizes, filled on demand.
    @ViewBuilder private var aiStorageCard: some View {
        let s = model.aiStorage
        Card {
            VStack(alignment: .leading, spacing: 6) {
                Text(Loc.t("ai_storage_title")).font(.headline)
                if s.isNull {
                    Text(Loc.t("ai_storage_loading")).font(.caption).foregroundStyle(.secondary)
                } else if !s["installed"].boolValue {
                    Text(Loc.t("ai_storage_none")).font(.caption).foregroundStyle(.secondary)
                } else {
                    let models = s["models"].array
                    let msize = s["models_size"].double ?? 0
                    let dsize = s["disk_size"].double ?? 0
                    Text(Loc.f("ai_storage_models", ["n": String(models.count), "size": Fmt.bytes(msize > 0 ? msize : dsize)]))
                    ForEach(Array(models.enumerated()), id: \.offset) { _, m in
                        HStack {
                            Text(m["name"].string ?? "").lineLimit(1).truncationMode(.middle)
                            Spacer()
                            Text(Fmt.bytes(m["size"].double ?? 0)).foregroundStyle(.secondary)
                        }
                    }
                    if let dir = s["models_dir"].string {
                        Text(dir).font(.caption2).foregroundStyle(.secondary)
                            .lineLimit(1).truncationMode(.middle)
                    }
                    Button(Loc.t("ai_storage_open")) { model.cmd("open_models_dir") }
                        .padding(.top, 2)
                }
            }
        }
    }
}
