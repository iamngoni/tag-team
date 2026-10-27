// Creates the persistent native menu bar scene and shares one polling model across menu openings.
// The app has no window or web content; menu status is driven only by tagteam-node output.
import SwiftUI

@main
@MainActor
struct TagTeamMenuBarApp: App {
  @StateObject private var monitor = NodeMonitor()

  var body: some Scene {
    MenuBarExtra {
      MenuContent(monitor: monitor)
    } label: {
      Image(systemName: monitor.symbolName)
        .accessibilityLabel(monitor.accessibilitySummary)
    }
    .menuBarExtraStyle(.menu)
  }
}
