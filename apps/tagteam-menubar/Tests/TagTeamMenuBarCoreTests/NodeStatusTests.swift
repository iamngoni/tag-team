// Verifies stable node status fields, optional resource details, and optional service state parsing.
// These tests exercise the production decoders without launching the menu bar UI.
import Foundation
import Testing

@testable import TagTeamMenuBarCore

struct NodeStatusTests {
  @Test
  func decodesStableStatusAndCapacityPolicy() throws {
    let data = Data(
      #"{"name":"lent-mac","enrolled":true,"paused":false,"left":false,"node_id":"node-123","policy":{"max_cores":4,"max_subtasks":2,"max_memory":2147483648}}"#
        .utf8)
    let status = try NodeStatus.decode(data)

    #expect(status.name == "lent-mac")
    #expect(status.enrolled)
    #expect(!status.paused)
    #expect(!status.left)
    #expect(status.nodeID == "node-123")
    #expect(status.capacitySummary == "4 cores · 2 subtasks · 2.0 GiB memory")
    #expect(status.workspaceCount == nil)
  }

  @Test
  func toleratesOptionalAndFutureFields() throws {
    let data = Data(
      #"{"name":"unconfigured","enrolled":false,"paused":false,"left":false,"node_id":null,"future_field":{"value":true},"workspaces":["a","b"]}"#
        .utf8)
    let status = try NodeStatus.decode(data)

    #expect(status.nodeID == nil)
    #expect(status.policy == nil)
    #expect(status.workspaceCount == 2)
  }

  @Test
  func decodesOptionalConnectionFieldsFromNodeStatus() throws {
    let data = Data(
      #"{"name":"lent-mac","enrolled":true,"paused":false,"left":false,"connected":false,"reconnecting":true}"#
        .utf8)
    let status = try NodeStatus.decode(data)

    #expect(status.connectionStatus?.summary == "Reconnecting")
  }

  @Test
  func ignoresMalformedOptionalStatusDetails() throws {
    let data = Data(
      #"{"name":"lent-mac","enrolled":true,"paused":false,"left":false,"node_id":42,"workspace_count":"unknown","connected":"unknown"}"#
        .utf8)
    let status = try NodeStatus.decode(data)

    #expect(status.nodeID == nil)
    #expect(status.workspaceCount == nil)
    #expect(status.connectionStatus?.summary == nil)
  }

  @Test
  func decodesServiceConnectionAndReconnectStateWhenPresent() throws {
    let connected = try NodeServiceStatus.decode(
      Data(#"{"service":{"running":true,"connected":true}}"#.utf8))
    let reconnecting = try NodeServiceStatus.decode(
      Data(#"{"reconnecting":true,"running":true}"#.utf8))

    #expect(connected.summary == "Connected")
    #expect(connected.isRunning == true)
    #expect(reconnecting.summary == "Reconnecting")
  }

  @Test
  func decodesServiceStateFromTheNodeCliContract() throws {
    let running = try NodeServiceStatus.decode(
      Data(#"{"label":"com.tagteam.node","state":"running"}"#.utf8))
    let stopped = try NodeServiceStatus.decode(
      Data(#"{"label":"com.tagteam.node","state":"stopped"}"#.utf8))
    let missing = try NodeServiceStatus.decode(
      Data(#"{"label":"com.tagteam.node","state":"not_installed"}"#.utf8))

    #expect(running.isInstalled == true)
    #expect(running.isRunning == true)
    #expect(running.summary == "Service running")
    #expect(stopped.isInstalled == true)
    #expect(stopped.isRunning == false)
    #expect(stopped.summary == "Service stopped")
    #expect(missing.isInstalled == false)
    #expect(missing.summary == "Service not installed")
  }

  @Test
  func acceptsServiceJsonWithoutRecognizedFields() throws {
    let status = try NodeServiceStatus.decode(Data(#"{"future_state":"starting-soon"}"#.utf8))
    #expect(status.summary == nil)
    #expect(status.isInstalled == nil)
  }
}
