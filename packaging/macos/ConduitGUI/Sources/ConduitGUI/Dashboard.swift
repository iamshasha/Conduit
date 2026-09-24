import SwiftUI
import Combine

private enum Page: String, CaseIterable, Identifiable {
    case overview, sites, activity, settings
    var id: String { rawValue }
    var label: String { Loc.t("nav_" + rawValue) }
    var symbol: String {
        switch self {
        case .overview: return "gauge.with.dots.needle.33percent"
        case .sites: return "globe"
        case .activity: return "list.bullet.rectangle"
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
        return Card {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(Fmt.host(origin)).font(.headline)
                    Spacer()
                    Button(Loc.t("open_sandbox")) { model.cmd("open_sandbox", ["origin": .string(origin)]) }
                    Button(Loc.t("revoke"), role: .destructive) { model.cmd("revoke", ["origin": .string(origin)]) }
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
            }
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
        }
    }
}
