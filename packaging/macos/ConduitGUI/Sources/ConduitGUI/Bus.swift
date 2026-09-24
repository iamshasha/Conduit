import Foundation

/// The link to the core: a Unix-domain socket carrying newline-delimited JSON.
/// A background thread reads lines and hands them to `onMessage` on the main
/// queue; writes go out under a lock. Mirrors gui-gtk/src/bus.rs.
final class Bus {
    private var fd: Int32 = -1
    private let wlock = NSLock()
    var onMessage: ((JSON) -> Void)?
    var onClose: (() -> Void)?

    /// Connect, send the `hello` handshake with the per-launch key, and start
    /// the reader. Returns false if the socket can't be reached.
    func connect(path: String, key: String) -> Bool {
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        path.withCString { cs in
            withUnsafeMutablePointer(to: &addr.sun_path) { raw in
                raw.withMemoryRebound(to: CChar.self, capacity: capacity) { dst in
                    var i = 0
                    while cs[i] != 0 && i < capacity - 1 { dst[i] = cs[i]; i += 1 }
                    dst[i] = 0
                }
            }
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) { sp in
                Darwin.connect(fd, sp, len)
            }
        }
        if rc != 0 { close(fd); fd = -1; return false }

        send(.object(["cmd": .string("hello"), "key": .string(key)]))
        startReader()
        return true
    }

    func send(_ msg: JSON) {
        guard fd >= 0,
              let data = try? JSONSerialization.data(withJSONObject: msg.foundation)
        else { return }
        var out = data
        out.append(0x0a)
        wlock.lock(); defer { wlock.unlock() }
        out.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            guard let base = buf.baseAddress else { return }
            var off = 0
            while off < buf.count {
                let n = write(fd, base + off, buf.count - off)
                if n <= 0 { return }
                off += n
            }
        }
    }

    /// `{"cmd": cmd, ...extra}` convenience.
    func cmd(_ cmd: String, _ extra: [String: JSON] = [:]) {
        var o = extra
        o["cmd"] = .string(cmd)
        send(.object(o))
    }

    private func startReader() {
        let f = fd
        Thread.detachNewThread { [weak self] in
            var acc = Data()
            var tmp = [UInt8](repeating: 0, count: 8192)
            while true {
                let n = read(f, &tmp, tmp.count)
                if n <= 0 { break }
                acc.append(tmp, count: n)
                if acc.count > (1 << 22) { break } // runaway guard
                while let nl = acc.firstIndex(of: 0x0a) {
                    let line = acc.subdata(in: acc.startIndex..<nl)
                    acc.removeSubrange(acc.startIndex...nl)
                    guard let s = String(data: line, encoding: .utf8),
                          let j = JSON.parse(s) else { continue }
                    DispatchQueue.main.async { self?.onMessage?(j) }
                }
            }
            DispatchQueue.main.async { self?.onClose?() }
        }
    }
}
