import Foundation

/// The same Strings.json the WinUI and GTK GUIs use, loaded from the bundle.
/// Lookups fall back to English, then to the key itself.
enum Loc {
    private static var table: [String: [String: String]] = Loc.load()
    private static var lang = "en"

    /// The core passes its setting through, which defaults to "auto": follow
    /// the system language then, as the WinUI GUI does.
    static func setLang(_ l: String?) {
        if let l, !l.isEmpty, l != "auto", table[l] != nil { lang = l; return }
        lang = systemLang()
    }

    private static func systemLang() -> String {
        for pref in Locale.preferredLanguages {
            if pref.hasPrefix("zh") {
                let hant = ["Hant", "TW", "HK", "MO"].contains { pref.contains($0) }
                return hant ? "zh-Hant" : "zh-Hans"
            }
            let two = String(pref.prefix { $0 != "-" && $0 != "_" })
            if table[two] != nil { return two }
        }
        return "en"
    }

    /// SwiftPM's `Bundle.module` traps when its resource bundle isn't where the
    /// build left it, and inside Conduit.app it would look at the .app's top
    /// level (which breaks signing). Look in Contents/Resources and next to the
    /// executable instead; with no bundle, keys show rather than crashing.
    private static func resourceBundle() -> Bundle? {
        let name = "ConduitGUI_ConduitGUI.bundle"
        let exeDir = Bundle.main.executableURL?.deletingLastPathComponent()
        let dirs = [Bundle.main.resourceURL, exeDir, exeDir?.appendingPathComponent("../Resources")]
        for dir in dirs.compactMap({ $0 }) {
            if let b = Bundle(url: dir.appendingPathComponent(name).standardizedFileURL) { return b }
        }
        return nil
    }

    private static func load() -> [String: [String: String]] {
        guard let url = resourceBundle()?.url(forResource: "Strings", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return [:] }
        var out: [String: [String: String]] = [:]
        for (code, v) in obj {
            if let m = v as? [String: Any] {
                out[code] = m.compactMapValues { $0 as? String }
            }
        }
        return out
    }

    static func t(_ key: String) -> String {
        table[lang]?[key] ?? table["en"]?[key] ?? key
    }

    static func f(_ key: String, _ args: [String: String]) -> String {
        var s = t(key)
        for (k, v) in args { s = s.replacingOccurrences(of: "{\(k)}", with: v) }
        return s
    }

    static var rtl: Bool { lang == "ar" || lang == "he" }

    /// (code, display name) for every language that carries a _name.
    static var languages: [(String, String)] {
        table.compactMap { code, m in m["_name"].map { (code, $0) } }
            .sorted { $0.1.localizedCaseInsensitiveCompare($1.1) == .orderedAscending }
    }
}
