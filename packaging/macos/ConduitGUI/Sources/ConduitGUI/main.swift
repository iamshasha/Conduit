import AppKit

// Started by the core with: --pipe <socket> --key <key> [--theme T --lang L].
// We connect to the socket before AppKit starts; if the core is gone there is
// nothing to show.
private func argument(_ name: String) -> String? {
    let a = CommandLine.arguments
    guard let i = a.firstIndex(of: name), i + 1 < a.count else { return nil }
    return a[i + 1]
}

guard let pipe = argument("--pipe") else {
    FileHandle.standardError.write(Data("conduit-gui: started without --pipe; nothing to connect to\n".utf8))
    exit(1)
}
Loc.setLang(argument("--lang"))

let app = NSApplication.shared
let delegate = AppDelegate(
    pipe: pipe,
    key: argument("--key") ?? "",
    dark: argument("--theme") == "dark"
)
app.delegate = delegate
// A helper GUI: no Dock icon, but it can still show and focus windows.
app.setActivationPolicy(.accessory)
app.run()
