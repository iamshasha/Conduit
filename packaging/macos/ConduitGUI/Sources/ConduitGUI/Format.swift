import Foundation

enum Fmt {
    /// Human byte count, matching the GUIs (1024 base, one decimal until GB).
    static func bytes(_ n: Double) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"]
        var v = n, i = 0
        while v >= 1024, i < units.count - 1 { v /= 1024; i += 1 }
        return i == 0 ? "\(Int(v)) B" : String(format: "%.1f %@", v, units[i])
    }

    static func bytes(_ n: Int64) -> String { bytes(Double(n)) }

    /// Compact uptime, e.g. "3d 04:12" or "12:07".
    static func uptime(_ secs: Int64) -> String {
        let d = secs / 86400, h = (secs % 86400) / 3600, m = (secs % 3600) / 60
        if d > 0 { return String(format: "%dd %02d:%02d", d, h, m) }
        return String(format: "%02d:%02d", h, m)
    }

    /// Host portion of an origin for compact display.
    static func host(_ origin: String) -> String {
        guard let r = origin.range(of: "://") else { return origin }
        return String(origin[r.upperBound...])
    }
}
