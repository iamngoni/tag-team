// Owns periodic CLI polling and menu-triggered node actions on the main actor.
// Process execution runs off the UI thread and never accepts free-form CLI arguments.
import AppKit
import Combine
import Foundation
import TagTeamMenuBarCore

@MainActor
final class NodeMonitor: ObservableObject {
  @Published private(set) var status: NodeStatus?
  @Published private(set) var statusError: String?
  @Published private(set) var serviceStatus: NodeServiceStatus?
  @Published private(set) var serviceStatusWasChecked = false
  @Published private(set) var actionMessage: String?
  @Published private(set) var isPerformingAction = false

  private let runner = NodeCommandRunner()
  private var pollTimer: Timer?
  private var isRefreshing = false

  init() {
    pollTimer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
      Task { @MainActor [weak self] in self?.refresh() }
    }
    refresh()
  }

  var symbolName: String {
    guard let status else {
      return statusError == nil ? "questionmark.circle" : "exclamationmark.circle.fill"
    }
    if status.left { return "person.crop.circle.badge.xmark" }
    if !status.enrolled { return "person.crop.circle.badge.questionmark" }
    if status.paused { return "pause.circle.fill" }
    return "bolt.horizontal.circle.fill"
  }

  var primaryStatus: String {
    guard let status else {
      return statusError == nil ? "Checking node…" : "Node status unavailable"
    }
    if status.left { return "Left Tag Team" }
    if !status.enrolled { return "Not enrolled" }
    if status.paused { return "Lending paused" }
    return "Lending enabled"
  }

  var accessibilitySummary: String {
    "Tag Team: \(primaryStatus)"
  }

  var canPause: Bool {
    status?.enrolled == true && status?.paused == false && status?.left == false
  }

  var canResume: Bool {
    status?.enrolled == true && status?.paused == true && status?.left == false
  }

  var canLeave: Bool {
    status?.enrolled == true && status?.left == false
  }

  func refresh() {
    guard !isRefreshing else { return }
    isRefreshing = true
    let commandRunner = runner

    Task { [weak self] in
      let statusOutcome = await Task.detached(priority: .utility) {
        commandRunner.run(.status)
      }.value
      guard let self else { return }
      defer { self.isRefreshing = false }

      switch Self.decodeNodeStatus(from: statusOutcome) {
      case .success(let snapshot):
        self.status = snapshot
        self.statusError = nil
      case .failure(let message):
        self.status = nil
        self.statusError = message
        self.serviceStatus = nil
        self.serviceStatusWasChecked = false
        return
      }

      let serviceOutcome = await Task.detached(priority: .utility) {
        commandRunner.run(.serviceStatus)
      }.value
      guard let decodedServiceStatus = Self.decodeServiceStatus(from: serviceOutcome) else {
        self.serviceStatus = nil
        self.serviceStatusWasChecked = true
        return
      }
      self.serviceStatus = decodedServiceStatus
      self.serviceStatusWasChecked = true
    }
  }

  func perform(_ command: NodeCommand) {
    guard !isPerformingAction else { return }
    isPerformingAction = true
    actionMessage = nil
    let commandRunner = runner

    Task { [weak self] in
      let outcome = await Task.detached(priority: .userInitiated) {
        commandRunner.run(command)
      }.value
      guard let self else { return }
      self.isPerformingAction = false
      self.actionMessage = Self.actionResult(for: command, outcome: outcome)
      self.refresh()
    }
  }

  func showActionMessage(_ message: String) {
    actionMessage = message
  }

  private static func decodeNodeStatus(from outcome: NodeCommandOutcome) -> StatusDecodeResult {
    switch outcome {
    case .completed(let exitCode, let output) where exitCode == 0:
      do {
        return .success(try NodeStatus.decode(Data(output.utf8)))
      } catch {
        return .failure("Could not read node status JSON: \(error.localizedDescription)")
      }
    case .completed(let exitCode, let output):
      return .failure("tagteam-node status exited with code \(exitCode). \(detail(output))")
    case .timedOut(let output):
      return .failure("tagteam-node status timed out. \(detail(output))")
    case .launchFailed(let message):
      return .failure(
        "Could not run tagteam-node. Check that it is installed and on PATH. \(message)")
    }
  }

  private static func decodeServiceStatus(from outcome: NodeCommandOutcome) -> NodeServiceStatus? {
    guard case .completed(let exitCode, let output) = outcome, exitCode == 0 else { return nil }
    return try? NodeServiceStatus.decode(Data(output.utf8))
  }

  private static func actionResult(for command: NodeCommand, outcome: NodeCommandOutcome) -> String
  {
    let action: String
    switch command {
    case .serviceInstall: action = "Install service"
    case .serviceStart: action = "Start service"
    case .serviceStop: action = "Stop service"
    case .serviceUninstall: action = "Uninstall service"
    case .pause: action = "Pause lending"
    case .resume: action = "Resume lending"
    case .leave: action = "Leave"
    case .status, .serviceStatus: action = "Node command"
    }

    switch outcome {
    case .completed(let exitCode, let output) where exitCode == 0:
      let detail = output.trimmingCharacters(in: .whitespacesAndNewlines)
      return detail.isEmpty ? "\(action) completed." : "\(action) completed: \(detail)"
    case .completed(let exitCode, let output):
      return "\(action) failed (exit code \(exitCode)). \(detail(output))"
    case .timedOut(let output):
      return "\(action) timed out. \(detail(output))"
    case .launchFailed(let message):
      return "\(action) could not run: \(message)"
    }
  }

  private static func detail(_ output: String) -> String {
    let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty else { return "" }
    let clipped = String(trimmed.prefix(240))
    return clipped == trimmed ? clipped : clipped + "…"
  }
}

private enum StatusDecodeResult {
  case success(NodeStatus)
  case failure(String)
}
