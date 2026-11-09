// swift-tools-version:5.7
import PackageDescription

let package = Package(
    name: "WBProto",
    platforms: [
        .iOS(.v16),
        .macOS(.v12),
    ],
    products: [
        .library(name: "WBProto", targets: ["WBProto"]),
    ],
    targets: [
        .target(
            name: "WBProto",
            path: "Sources/WBProto"
        ),
        .testTarget(
            name: "WBProtoTests",
            dependencies: ["WBProto"],
            path: "Tests/WBProtoTests"
        ),
    ]
)
