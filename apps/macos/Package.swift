// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "BrainDrainMac",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "BrainDrain", targets: ["BrainDrainApp"]),
    ],
    targets: [
        .executableTarget(
            name: "BrainDrainApp",
            linkerSettings: [
                .linkedFramework("SwiftUI"),
            ]
        ),
        .testTarget(
            name: "BrainDrainAppTests"
        ),
    ]
)
