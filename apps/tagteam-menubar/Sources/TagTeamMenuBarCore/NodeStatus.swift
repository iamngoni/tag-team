// Owns tolerant decoding and display summaries for local node and optional service JSON.
// Unknown fields are ignored so the menu remains compatible as the CLI adds status details.
import Foundation

/// JSON values used for optional policy and capacity fields whose schemas may evolve.
public indirect enum JSONValue: Decodable, Sendable, Equatable {
  case object([String: JSONValue])
  case array([JSONValue])
  case string(String)
  case number(Double)
  case bool(Bool)
  case null

  public init(from decoder: Decoder) throws {
    if let keyed = try? decoder.container(keyedBy: DynamicCodingKey.self) {
      var values: [String: JSONValue] = [:]
      for key in keyed.allKeys {
        values[key.stringValue] = try keyed.decode(JSONValue.self, forKey: key)
      }
      self = .object(values)
      return
    }

    if var unkeyed = try? decoder.unkeyedContainer() {
      var values: [JSONValue] = []
      while !unkeyed.isAtEnd {
        values.append(try unkeyed.decode(JSONValue.self))
      }
      self = .array(values)
      return
    }

    let value = try decoder.singleValueContainer()
    if value.decodeNil() {
      self = .null
    } else if let boolean = try? value.decode(Bool.self) {
      self = .bool(boolean)
    } else if let integer = try? value.decode(Int64.self) {
      self = .number(Double(integer))
    } else if let number = try? value.decode(Double.self) {
      self = .number(number)
    } else if let string = try? value.decode(String.self) {
      self = .string(string)
    } else {
      throw DecodingError.dataCorruptedError(
        in: value,
        debugDescription: "Unsupported JSON value"
      )
    }
  }
}

private struct DynamicCodingKey: CodingKey {
  let stringValue: String
  let intValue: Int?

  init?(stringValue: String) {
    self.stringValue = stringValue
    self.intValue = nil
  }

  init?(intValue: Int) {
    self.stringValue = String(intValue)
    self.intValue = intValue
  }
}

/// The fields reported by `tagteam-node status`, plus optional resource details.
public struct NodeStatus: Decodable, Sendable, Equatable {
  public let name: String
  public let enrolled: Bool
  public let paused: Bool
  public let left: Bool
  public let nodeID: String?
  public let policy: JSONValue?
  public let capacity: JSONValue?
  public let workspaceCount: Int?

  private enum CodingKeys: String, CodingKey {
    case name
    case enrolled
    case paused
    case left
    case nodeID = "node_id"
    case policy
    case capacity
    case workspaceCount = "workspace_count"
    case camelWorkspaceCount = "workspaceCount"
    case workspaces
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    name = try container.decode(String.self, forKey: .name)
    enrolled = try container.decode(Bool.self, forKey: .enrolled)
    paused = try container.decode(Bool.self, forKey: .paused)
    left = try container.decode(Bool.self, forKey: .left)
    nodeID = try container.decodeIfPresent(String.self, forKey: .nodeID)
    policy = try container.decodeIfPresent(JSONValue.self, forKey: .policy)
    capacity = try container.decodeIfPresent(JSONValue.self, forKey: .capacity)

    let directCount = try container.decodeIfPresent(Int.self, forKey: .workspaceCount)
    let camelCount = try container.decodeIfPresent(Int.self, forKey: .camelWorkspaceCount)
    let workspaces = try container.decodeIfPresent(JSONValue.self, forKey: .workspaces)
    workspaceCount = directCount ?? camelCount ?? workspaces?.countValue
  }

  /// Parses one status document and rejects missing or malformed stable fields.
  public static func decode(_ data: Data) throws -> NodeStatus {
    try JSONDecoder().decode(NodeStatus.self, from: data)
  }

  /// A compact capacity summary sourced only from fields actually present in status JSON.
  public var capacitySummary: String? {
    var parts: [String] = []
    let sources = [capacity, policy].compactMap { $0 }

    if let cores = firstInteger(
      in: sources, keys: ["max_cores", "available_cores", "cpu_cores", "cores"])
    {
      parts.append("\(cores) cores")
    }
    if let subtasks = firstInteger(in: sources, keys: ["max_subtasks", "running_subtasks"]) {
      parts.append("\(subtasks) subtasks")
    }
    if let memory = firstInteger(in: sources, keys: ["max_memory", "memory_bytes"]), memory > 0 {
      let gibibytes = Double(memory) / 1_073_741_824
      if gibibytes >= 1 {
        parts.append(String(format: "%.1f GiB memory", gibibytes))
      } else {
        parts.append("\(memory / 1_048_576) MiB memory")
      }
    }
    return parts.isEmpty ? nil : parts.joined(separator: " · ")
  }
}

extension JSONValue {
  fileprivate var countValue: Int? {
    switch self {
    case .number(let number) where number >= 0 && number.rounded(.towardZero) == number:
      return Int(exactly: number)
    case .array(let values):
      return values.count
    case .object(let values):
      return values["count"]?.countValue
    default:
      return nil
    }
  }

  fileprivate func integer(for key: String) -> Int? {
    guard case .object(let values) = self else { return nil }
    return values[key]?.countValue
  }

  fileprivate func firstInteger(matching keys: [String]) -> Int? {
    for key in keys {
      if let value = integer(for: key) { return value }
    }
    if case .object(let values) = self {
      for value in values.values {
        if let result = value.firstInteger(matching: keys) { return result }
      }
    }
    return nil
  }
}

private func firstInteger(in values: [JSONValue], keys: [String]) -> Int? {
  for value in values {
    if let result = value.firstInteger(matching: keys) { return result }
  }
  return nil
}

/// Optional connection and service state returned by `tagteam-node service status --json`.
public struct NodeServiceStatus: Sendable, Equatable {
  public let isConnected: Bool?
  public let isReconnecting: Bool?
  public let isRunning: Bool?
  public let state: String?
  private let stateText: String?

  private init(root: JSONValue) {
    isConnected = root.boolean(matching: ["connected", "is_connected", "online"])
    isReconnecting = root.boolean(matching: ["reconnecting", "is_reconnecting"])
    stateText = root.string(matching: ["state", "status", "connection", "service_state"])
    state = stateText?.lowercased()
    isRunning =
      root.boolean(matching: ["running", "is_running", "started", "active"])
      ?? state.map { ["running", "active", "activating"].contains($0) }
  }

  /// Whether a service definition is present and can be started or stopped.
  public var isInstalled: Bool? {
    guard let state else { return nil }
    return state != "not_installed"
  }

  /// Decodes service state without requiring optional fields to be present.
  public static func decode(_ data: Data) throws -> NodeServiceStatus {
    let root = try JSONDecoder().decode(JSONValue.self, from: data)
    return NodeServiceStatus(root: root)
  }

  /// Human-readable state when the service JSON contains a recognized status.
  public var summary: String? {
    let normalized = stateText?.lowercased() ?? ""
    if isReconnecting == true || normalized == "reconnecting" || normalized == "starting" {
      return "Reconnecting"
    }
    if isConnected == true || normalized == "connected" || normalized == "online" {
      return "Connected"
    }
    if isConnected == false || normalized == "disconnected" || normalized == "offline" {
      return "Disconnected"
    }
    if normalized == "not_installed" { return "Service not installed" }
    if normalized == "failed" { return "Service failed" }
    if isRunning == true || ["running", "active", "started"].contains(normalized) {
      return "Service running"
    }
    if isRunning == false || ["stopped", "inactive"].contains(normalized) {
      return "Service stopped"
    }
    return nil
  }
}

extension JSONValue {
  fileprivate func boolean(matching names: Set<String>) -> Bool? {
    if case .object(let values) = self {
      for (key, value) in values where names.contains(key.lowercased()) {
        if case .bool(let boolean) = value { return boolean }
      }
      for value in values.values {
        if let result = value.boolean(matching: names) { return result }
      }
    } else if case .array(let values) = self {
      for value in values {
        if let result = value.boolean(matching: names) { return result }
      }
    }
    return nil
  }

  fileprivate func string(matching names: Set<String>) -> String? {
    if case .object(let values) = self {
      for (key, value) in values where names.contains(key.lowercased()) {
        if case .string(let string) = value { return string }
      }
      for value in values.values {
        if let result = value.string(matching: names) { return result }
      }
    } else if case .array(let values) = self {
      for value in values {
        if let result = value.string(matching: names) { return result }
      }
    }
    return nil
  }
}
