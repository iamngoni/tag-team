// Renders accessible node status and native menu actions for the lent-machine owner.
// Leave requires an explicit AppKit confirmation before the fixed CLI command is run.
import AppKit
import SwiftUI
import TagTeamMenuBarCore

struct MenuContent: View {
  @ObservedObject var monitor: NodeMonitor

  var body: some View {
    if let status = monitor.status {
      Text(status.name)
        .accessibilityLabel("Node name: \(status.name)")
      Text(monitor.primaryStatus)
        .accessibilityLabel("Lending status: \(monitor.primaryStatus)")
      if monitor.serviceStatusWasChecked {
        Text(monitor.serviceStatus?.summary ?? "Service status unavailable")
          .accessibilityLabel("Service status: \(monitor.serviceStatus?.summary ?? "unavailable")")
      }
      if let capacity = status.capacitySummary {
        Text("Capacity: \(capacity)")
          .accessibilityLabel("Capacity: \(capacity)")
      }
      if let workspaceCount = status.workspaceCount {
        Text("Workspaces: \(workspaceCount)")
          .accessibilityLabel("Workspaces: \(workspaceCount)")
      }
    } else if let error = monitor.statusError {
      Text("Node status unavailable")
        .accessibilityLabel("Node status unavailable")
      Text(error)
        .lineLimit(4)
        .accessibilityLabel("Status error: \(error)")
    } else {
      Text("Checking node…")
        .accessibilityLabel("Checking node status")
    }

    if let message = monitor.actionMessage {
      Text(message)
        .lineLimit(4)
        .accessibilityLabel("Action result: \(message)")
    }
    if monitor.isPerformingAction {
      Text("Working…")
        .accessibilityLabel("Node action in progress")
    }

    Divider()

    if monitor.serviceStatus?.isInstalled == false {
      Button {
        monitor.perform(.serviceInstall)
      } label: {
        Label("Install service", systemImage: "square.and.arrow.down")
      }
      .disabled(monitor.status?.enrolled != true || monitor.isPerformingAction)
    } else {
      Button {
        monitor.perform(.serviceStart)
      } label: {
        Label("Start service", systemImage: "play.fill")
      }
      .disabled(
        monitor.serviceStatus?.isInstalled != true || monitor.serviceStatus?.isRunning == true
          || monitor.isPerformingAction)

      Button {
        monitor.perform(.serviceStop)
      } label: {
        Label("Stop service", systemImage: "stop.fill")
      }
      .disabled(
        monitor.serviceStatus?.isInstalled != true || monitor.serviceStatus?.isRunning != true
          || monitor.isPerformingAction)
    }

    Button {
      monitor.perform(.pause)
    } label: {
      Label("Pause lending", systemImage: "pause.fill")
    }
    .disabled(!monitor.canPause || monitor.isPerformingAction)

    Button {
      monitor.perform(.resume)
    } label: {
      Label("Resume lending", systemImage: "playpause.fill")
    }
    .disabled(!monitor.canResume || monitor.isPerformingAction)

    Button {
      confirmLeave()
    } label: {
      Label("Leave", systemImage: "person.crop.circle.badge.xmark")
    }
    .disabled(!monitor.canLeave || monitor.isPerformingAction)

    Button {
      confirmUninstall()
    } label: {
      Label("Uninstall service", systemImage: "trash")
    }
    .disabled(monitor.serviceStatus?.isInstalled != true || monitor.isPerformingAction)

    Divider()

    Button {
      openConfiguration()
    } label: {
      Label("Open config", systemImage: "slider.horizontal.3")
    }
    .keyboardShortcut(",", modifiers: [.command])

    Button {
      copyNodeID()
    } label: {
      Label("Copy node ID", systemImage: "doc.on.doc")
    }
    .disabled(monitor.status?.nodeID == nil)

    Button {
      NSApp.terminate(nil)
    } label: {
      Label("Quit Tag Team", systemImage: "xmark.circle")
    }
    .keyboardShortcut("q", modifiers: [.command])
  }

  @MainActor
  private func confirmLeave() {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = "Leave Tag Team?"
    alert.informativeText =
      "This disables reconnecting from this machine. You can’t resume lending until it is enrolled again."
    alert.addButton(withTitle: "Cancel")
    alert.addButton(withTitle: "Leave")
    if alert.runModal() == .alertSecondButtonReturn {
      monitor.perform(.leave)
    }
  }

  @MainActor
  private func confirmUninstall() {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = "Uninstall the Tag Team service?"
    alert.informativeText =
      "This stops the background service and removes its launch definition. The node identity and configuration stay on this machine."
    alert.addButton(withTitle: "Cancel")
    alert.addButton(withTitle: "Uninstall")
    if alert.runModal() == .alertSecondButtonReturn {
      monitor.perform(.serviceUninstall)
    }
  }

  @MainActor
  private func openConfiguration() {
    let url = FileManager.default.homeDirectoryForCurrentUser
      .appendingPathComponent(".config/tagteam/node.toml")
    guard FileManager.default.fileExists(atPath: url.path) else {
      monitor.showActionMessage("Config file not found at \(url.path)")
      return
    }
    guard NSWorkspace.shared.open(url) else {
      monitor.showActionMessage("Could not open config at \(url.path)")
      return
    }
  }

  @MainActor
  private func copyNodeID() {
    guard let nodeID = monitor.status?.nodeID else { return }
    let pasteboard = NSPasteboard.general
    pasteboard.clearContents()
    guard pasteboard.setString(nodeID, forType: .string) else {
      monitor.showActionMessage("Could not copy node ID")
      return
    }
    monitor.showActionMessage("Node ID copied")
  }
}
