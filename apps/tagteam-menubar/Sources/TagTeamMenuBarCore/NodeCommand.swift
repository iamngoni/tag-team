// Defines the app's fixed tagteam-node argument vocabulary and bounded Foundation process runner.
// Commands never use a shell and no user-provided value is appended to an argument list.
import Darwin
import Foundation

/// Fixed commands issued by the menu bar app.
public enum NodeCommand: Sendable, Equatable {
  case status
  case serviceStatus
  case serviceInstall
  case serviceStart
  case serviceStop
  case serviceUninstall
  case pause
  case resume
  case leave

  /// The complete argument vector passed to `tagteam-node`.
  public var arguments: [String] {
    switch self {
    case .status: ["status"]
    case .serviceStatus: ["service", "status", "--json"]
    case .serviceInstall: ["service", "install"]
    case .serviceStart: ["service", "start"]
    case .serviceStop: ["service", "stop"]
    case .serviceUninstall: ["service", "uninstall"]
    case .pause: ["pause"]
    case .resume: ["resume"]
    case .leave: ["leave"]
    }
  }

  fileprivate var timeout: TimeInterval {
    switch self {
    case .serviceInstall, .serviceStart, .serviceStop, .serviceUninstall: 12
    case .serviceStatus: 3
    case .status, .pause, .resume, .leave: 5
    }
  }
}

/// Outcome of one bounded node command.
public enum NodeCommandOutcome: Sendable, Equatable {
  case completed(exitCode: Int32, output: String)
  case timedOut(output: String)
  case launchFailed(String)
}

/// Runs the fixed CLI through `Process` with no shell and bounded termination waits.
public struct NodeCommandRunner: Sendable {
  private static let outputLimit = 1_048_576

  public init() {}

  /// Invokes `tagteam-node` using a fixed argument vector and returns a bounded result.
  public func run(_ command: NodeCommand) -> NodeCommandOutcome {
    let outputURL = FileManager.default.temporaryDirectory
      .appendingPathComponent("tagteam-node-\(UUID().uuidString).output")
    guard
      FileManager.default.createFile(
        atPath: outputURL.path,
        contents: Data(),
        attributes: [.posixPermissions: 0o600]
      )
    else {
      return .launchFailed("Could not create a private output file for tagteam-node.")
    }
    defer { try? FileManager.default.removeItem(at: outputURL) }

    do {
      let outputHandle = try FileHandle(forWritingTo: outputURL)
      defer { try? outputHandle.close() }

      let process = Process()
      process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
      process.arguments = ["tagteam-node"] + command.arguments
      process.standardOutput = outputHandle
      process.standardError = outputHandle
      process.environment = environmentWithNodeSearchPath()

      let terminated = DispatchSemaphore(value: 0)
      process.terminationHandler = { _ in terminated.signal() }
      try process.run()

      let timedOut = terminated.wait(timeout: .now() + command.timeout) == .timedOut
      if timedOut {
        if process.isRunning { process.terminate() }
        if terminated.wait(timeout: .now() + 0.5) == .timedOut, process.isRunning {
          _ = Darwin.kill(process.processIdentifier, SIGKILL)
          _ = terminated.wait(timeout: .now() + 0.75)
        }
      }

      try? outputHandle.synchronize()
      let output = readOutput(from: outputURL)
      if timedOut { return .timedOut(output: output) }
      return .completed(exitCode: process.terminationStatus, output: output)
    } catch {
      return .launchFailed(error.localizedDescription)
    }
  }

  private func environmentWithNodeSearchPath() -> [String: String] {
    var environment = ProcessInfo.processInfo.environment
    let localBin = FileManager.default.homeDirectoryForCurrentUser
      .appendingPathComponent(".local/bin", isDirectory: true)
      .path
    let preferred = [
      localBin, "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin",
      "/bin", "/usr/sbin", "/sbin",
    ]
    let inherited = (environment["PATH"] ?? "").split(separator: ":").map(String.init)
    var seen = Set<String>()
    let paths = (preferred + inherited).filter { seen.insert($0).inserted }
    environment["PATH"] = paths.joined(separator: ":")
    return environment
  }

  private func readOutput(from url: URL) -> String {
    guard let handle = try? FileHandle(forReadingFrom: url) else { return "" }
    defer { try? handle.close() }
    do {
      guard let data = try handle.read(upToCount: Self.outputLimit) else { return "" }
      return String(data: data, encoding: .utf8) ?? ""
    } catch {
      return ""
    }
  }
}
