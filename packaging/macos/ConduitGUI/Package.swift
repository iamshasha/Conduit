// swift-tools-version:5.9
import PackageDescription

// Conduit's macOS window process. The core (conduit) spawns the built
// `conduit-gui` binary over a Unix socket; this app renders the dashboard and
// consent prompts from the streamed JSON, mirroring the GTK and WinUI GUIs.
let package = Package(
    name: "ConduitGUI",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "conduit-gui", targets: ["ConduitGUI"]),
    ],
    targets: [
        .executableTarget(
            name: "ConduitGUI",
            path: "Sources/ConduitGUI",
            resources: [.process("Resources")]
        ),
    ]
)
