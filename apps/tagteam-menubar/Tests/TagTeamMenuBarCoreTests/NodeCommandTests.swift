// Verifies each exposed menu action maps to a fixed, shell-free argument vector.
// Adding a menu action requires an explicit command case rather than interpolated text.
import Testing

@testable import TagTeamMenuBarCore

struct NodeCommandTests {
  @Test
  func buildsFixedArgumentVectors() {
    #expect(NodeCommand.status.arguments == ["status"])
    #expect(NodeCommand.serviceStatus.arguments == ["service", "status", "--json"])
    #expect(NodeCommand.serviceInstall.arguments == ["service", "install"])
    #expect(NodeCommand.serviceStart.arguments == ["service", "start"])
    #expect(NodeCommand.serviceStop.arguments == ["service", "stop"])
    #expect(NodeCommand.serviceUninstall.arguments == ["service", "uninstall"])
    #expect(NodeCommand.pause.arguments == ["pause"])
    #expect(NodeCommand.resume.arguments == ["resume"])
    #expect(NodeCommand.leave.arguments == ["leave"])
  }
}
