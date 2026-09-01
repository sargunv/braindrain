// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "BrainDrainMac",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "BrainDrain", targets: ["BrainDrainApp"]),
    ],
    dependencies: [
        .package(path: "../../crates/bindings-uniffi"),
    ],
    targets: [
        .executableTarget(
            name: "BrainDrainApp",
            dependencies: [
                .product(name: "BrainDrainBindings", package: "bindings-uniffi"),
            ],
            linkerSettings: [
                .linkedFramework("SwiftUI"),
            ]
        ),
        .testTarget(
            name: "BrainDrainAppTests",
            dependencies: [
                "BrainDrainApp",
                .product(name: "BrainDrainBindings", package: "bindings-uniffi"),
            ]
        ),
    ]
)
