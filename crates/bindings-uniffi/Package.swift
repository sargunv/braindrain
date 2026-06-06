// swift-tools-version: 6.0

import Foundation
import PackageDescription

let packageRoot = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let repositoryRoot = packageRoot
    .deletingLastPathComponent()
    .deletingLastPathComponent()
let rustTargetDebug = repositoryRoot.appendingPathComponent("target/debug").path

let package = Package(
    name: "BrainDrainBindings",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "BrainDrainBindings", targets: ["BrainDrainBindings"]),
    ],
    targets: [
        .systemLibrary(
            name: "braindrain_bindings_uniffiFFI",
            path: "swift"
        ),
        .target(
            name: "BrainDrainBindings",
            dependencies: ["braindrain_bindings_uniffiFFI"],
            path: ".generated/swift",
            exclude: [
                "braindrain_bindings_uniffiFFI.h",
                "braindrain_bindings_uniffiFFI.modulemap",
            ],
            linkerSettings: [
                .linkedLibrary("braindrain_bindings_uniffi"),
                .unsafeFlags([
                    "-L\(rustTargetDebug)",
                    "-Xlinker", "-rpath",
                    "-Xlinker", rustTargetDebug,
                ]),
            ]
        ),
    ]
)
