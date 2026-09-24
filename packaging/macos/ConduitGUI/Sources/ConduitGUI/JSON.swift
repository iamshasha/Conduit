import Foundation

/// A small JSON value with serde_json-style ergonomics, so the streamed
/// messages read the same way they do in the Rust and GTK code. Backed by
/// Foundation's JSONSerialization for parse and encode.
enum JSON {
    case object([String: JSON])
    case array([JSON])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    init(any: Any) {
        // JSONSerialization yields NSNumber for both bools and numbers, NSNull
        // for null, String for strings, and NSArray/NSDictionary for the rest.
        switch any {
        case let d as [String: Any]:
            self = .object(d.mapValues { JSON(any: $0) })
        case let a as [Any]:
            self = .array(a.map { JSON(any: $0) })
        case let s as String:
            self = .string(s)
        case let n as NSNumber:
            // Distinguish a boolean NSNumber from a numeric one by its type id;
            // casting `as? Bool` would treat 1/0 as booleans.
            if CFGetTypeID(n) == CFBooleanGetTypeID() {
                self = .bool(n.boolValue)
            } else {
                self = .number(n.doubleValue)
            }
        default:
            self = .null
        }
    }

    static func parse(_ line: String) -> JSON? {
        guard let data = line.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        else { return nil }
        return JSON(any: obj)
    }

    // ---- accessors (all forgiving; a missing/typed-wrong value reads empty) ----

    subscript(_ key: String) -> JSON {
        if case let .object(d) = self { return d[key] ?? .null }
        return .null
    }

    subscript(_ index: Int) -> JSON {
        if case let .array(a) = self, index >= 0, index < a.count { return a[index] }
        return .null
    }

    var string: String? {
        if case let .string(s) = self { return s }
        return nil
    }
    var double: Double? {
        switch self {
        case let .number(n): return n
        case let .string(s): return Double(s)
        default: return nil
        }
    }
    var int: Int? { double.map { Int($0) } }
    var int64: Int64? { double.map { Int64($0) } }
    var boolValue: Bool {
        switch self {
        case let .bool(b): return b
        case let .number(n): return n != 0
        default: return false
        }
    }
    var array: [JSON] {
        if case let .array(a) = self { return a }
        return []
    }
    var isNull: Bool {
        if case .null = self { return true }
        return false
    }

    /// Back to a Foundation object for JSONSerialization (sending commands).
    var foundation: Any {
        switch self {
        case let .object(d): return d.mapValues { $0.foundation }
        case let .array(a): return a.map { $0.foundation }
        case let .string(s): return s
        case let .number(n):
            // Emit whole numbers as integers so the core's as_u64() (e.g. the
            // consent id) parses them; keep fractional values as doubles.
            if n == n.rounded() && abs(n) < 9.007199254740992e15 { return Int64(n) }
            return n
        case let .bool(b): return b
        case .null: return NSNull()
        }
    }
}
