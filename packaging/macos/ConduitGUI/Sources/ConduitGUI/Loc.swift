import Foundation

/// The same Strings.json the WinUI and GTK GUIs use, loaded from the bundle.
/// Lookups fall back to English, then to the key itself.
enum Loc {
    private static var table: [String: [String: String]] = Loc.load()
    private static var lang = "en"

    static func setLang(_ l: String?) { lang = (l?.isEmpty == false) ? l! : "en" }

    private static func load() -> [String: [String: String]] {
        guard let url = Bundle.module.url(forResource: "Strings", withExtension: "json"),
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
