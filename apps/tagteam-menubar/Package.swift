// swift-tools-version: 6.0
// SwiftPM manifest for the native macOS menu bar app and its dependency-free status core.
// The package targets macOS 14 and keeps command parsing independently testable.
import PackageDescription

let package = Package(
    name: "TagTeamMenuBar",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "TagTeamMenuBarCore", targets: ["TagTeamMenuBarCore"]),
        .executable(name: "TagTeamMenuBar", targets: ["TagTeamMenuBarApp"]),
    ],
    targets: [
        .target(name: "TagTeamMenuBarCore"),
        .executableTarget(
            name: "TagTeamMenuBarApp",
            dependencies: ["TagTeamMenuBarCore"]
        ),
        .testTarget(
            name: "TagTeamMenuBarCoreTests",
            dependencies: ["TagTeamMenuBarCore"]
        ),
    ]
)
