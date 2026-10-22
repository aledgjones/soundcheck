// Keeps one FBSimulatorControl session open and serves `axe describe-ui` JSON for the booted simulator on
// 127.0.0.1:9009 (GET /describe, GET /ping), so each read skips axe's start-up (~0.6 s, under Rosetta).
// The fetch and serialisation follow AXe 1.8.0's AccessibilityFetcher (MIT, github.com/cameroncooke/AXe) and link
// the FB frameworks Homebrew's axe ships, so the output matches `axe describe-ui` exactly. Test tool, simulator only.
//   axe-reader --udid <udid> [--port 9009]
import FBControlCore
import FBSimulatorControl
import Foundation
import Network

@objc final class NoEvents: NSObject, FBEventReporter {
  var metadata: [String: String] = [:]
  func report(_ subject: FBEventReporterSubject) {}
  func addMetadata(_ metadata: [String: String]) {}
}

@MainActor
final class Reader {
  // AXe's legacy public schema; Xcode 27 returns an empty tree when `.traits` is requested, so it's added afterwards.
  static let outputKeys: Set<FBAXKeys> = [
    .label, .frame, .value, .uniqueID, .type, .title, .frameDict, .help, .enabled, .customActions, .role,
    .roleDescription, .subrole, .contentRequired, .pid, .traits,
  ]
  static let requestKeys = outputKeys.subtracting([.traits])
  // What ui.mjs reads; fewer keys means fewer round trips per element.
  static let leanKeys: Set<FBAXKeys> = [.label, .value, .uniqueID, .frame, .frameDict, .type, .enabled]

  let udid: String
  var simulator: FBSimulator?
  var hid: FBSimulatorHID?
  // Whether the last script left a touch down, so the next one must keep its session.
  var touching = false

  init(udid: String) {
    self.udid = udid
  }

  func setUp() throws {
    let logger = FBControlCoreLoggerFactory.systemLoggerWriting(toStderr: false, withDebugLogging: false)
    FBControlCoreGlobalConfiguration.defaultLogger = logger
    _ = try FBXcodeDirectory.resolveDeveloperDirectory()
    try FBSimulatorControlFrameworkLoader.essentialFrameworks.loadPrivateFrameworks(logger)
    try FBSimulatorControlFrameworkLoader.xcodeFrameworks.loadPrivateFrameworks(logger)
    let control = try FBSimulatorControl.withConfiguration(
      FBSimulatorControlConfiguration(deviceSetPath: nil, logger: logger, reporter: NoEvents())
    )
    guard let simulator = control.set.allSimulators.first(where: { $0.udid == udid }) else {
      throw NSError(domain: "axe-reader", code: 1, userInfo: [NSLocalizedDescriptionKey: "No simulator \(udid)"])
    }
    self.simulator = simulator
  }

  // The frontmost app's tree; like AXe, retry briefly while it has no children (just after boot or a launch).
  func describe(lean: Bool = false) async throws -> Data {
    guard let simulator else {
      throw NSError(domain: "axe-reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    var latest = Data()
    for attempt in 0..<5 {
      let element = try await simulator.accessibilityElementForFrontmostApplication()
      defer { element.close() }
      let response = try element.serialize(
        with: FBAccessibilityRequestOptions(nestedFormat: true, keys: lean ? Reader.leanKeys : Reader.requestKeys)
      )
      latest = try JSONSerialization.data(
        withJSONObject: Reader.withDefaults(response.elements), options: [.prettyPrinted]
      )
      if Reader.hasDescendant(latest) {
        return latest
      }
      if attempt < 4 {
        try await Task.sleep(for: .milliseconds(50 * (1 << attempt)))
      }
    }
    return latest
  }

  // Sends a script of input events through one held HID session, rather than starting `axe` per touch (~1.2 s each).
  // Steps are separated by `;`: `d x y` touch down (or move, while down), `u x y` touch up, `w ms` wait, `k code`
  // key press, `kd code` / `ku code` key down and up.
  func sendHID(_ script: String) async throws {
    guard let simulator else {
      throw NSError(domain: "axe-reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    // Any `xcodebuild` that loads a workspace leaves an open session dead without an error: taps land nowhere while
    // reads still work. So each script gets a fresh session (~10 ms), except while a touch is held across scripts.
    if !touching {
      hid?.disconnect()
      hid = nil
    }
    if hid == nil {
      hid = try await simulator.connectToHID()
    }
    guard let hid else { return }
    var events: [FBSimulatorHIDEvent] = []
    var touchingAfter = touching
    for step in script.split(separator: ";") {
      let parts = step.split(separator: " ")
      let n = parts.dropFirst().compactMap { Double($0) }
      switch (parts.first, n.count) {
      case ("d", 2):
        events.append(.touch(direction: .down, x: n[0], y: n[1]))
        touchingAfter = true
      case ("u", 2):
        events.append(.touch(direction: .up, x: n[0], y: n[1]))
        touchingAfter = false
      case ("w", 1): events.append(.delay(n[0] / 1000))
      case ("k", 1): events.append(.shortKeyPress(UInt32(n[0])))
      case ("kd", 1): events.append(.keyboard(direction: .down, keyCode: UInt32(n[0])))
      case ("ku", 1): events.append(.keyboard(direction: .up, keyCode: UInt32(n[0])))
      case ("b", 0) where parts.count == 2 && parts[1] == "home": events.append(.shortButtonPress(.homeButton))
      default:
        throw NSError(domain: "axe-reader", code: 5, userInfo: [NSLocalizedDescriptionKey: "Bad step: \(step)"])
      }
    }
    do {
      try await FBSimulatorHIDEvent.composite(events).sendAsync(on: hid)
      touching = touchingAfter
    } catch {
      hid.disconnect()
      self.hid = nil
      touching = false
      throw error
    }
  }

  // What Xcode's Device Manager toggles: with it off, the on-screen keyboard shows and iOS ignores AXe's typing, which
  // arrives as a hardware keyboard. CoreSimulator's SimDevice method is private, so it's called through the runtime.
  func setHardwareKeyboard(_ enabled: Bool) throws {
    guard let device = simulator?.value(forKey: "device") as? NSObject else {
      throw NSError(domain: "axe-reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    let selector = NSSelectorFromString("setHardwareKeyboardEnabled:keyboardType:error:")
    guard device.responds(to: selector) else {
      throw NSError(domain: "axe-reader", code: 3, userInfo: [NSLocalizedDescriptionKey: "No hardware keyboard switch"])
    }
    typealias Set = @convention(c) (NSObject, Selector, Bool, UInt32, AutoreleasingUnsafeMutablePointer<NSError?>?)
      -> Bool
    let set = unsafeBitCast(device.method(for: selector), to: Set.self)
    var error: NSError?
    if !set(device, selector, enabled, 0, &error) {
      throw error ?? NSError(domain: "axe-reader", code: 4, userInfo: [NSLocalizedDescriptionKey: "Switch refused"])
    }
  }

  static func withDefaults(_ info: Any) -> Any {
    if let list = info as? [Any] {
      return list.map(withDefaults)
    }
    guard var element = info as? [String: Any] else {
      return info
    }
    for (key, value) in element {
      element[key] = withDefaults(value)
    }
    if element["type"] != nil, element["traits"] == nil {
      element["traits"] = [String]()
    }
    return element
  }

  static func hasDescendant(_ data: Data) -> Bool {
    guard let object = try? JSONSerialization.jsonObject(with: data) else {
      return false
    }
    let roots = (object as? [[String: Any]]) ?? ((object as? [String: Any]).map { [$0] } ?? [])
    return roots.contains { ($0["children"] as? [Any])?.isEmpty == false }
  }
}

@main
struct Main {
  static func main() async {
    let args = CommandLine.arguments
    func option(_ name: String) -> String? {
      args.firstIndex(of: name).flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
    }
    guard let udid = option("--udid") else {
      FileHandle.standardError.write("usage: axe-reader --udid <udid> [--port 9009]\n".data(using: .utf8)!)
      exit(2)
    }
    let port = NWEndpoint.Port(rawValue: UInt16(option("--port") ?? "9009") ?? 9009)!
    let reader = await Reader(udid: udid)
    do {
      try await reader.setUp()
    } catch {
      FileHandle.standardError.write("axe-reader: \(error)\n".data(using: .utf8)!)
      exit(1)
    }
    let parameters = NWParameters.tcp
    parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: port)
    parameters.allowLocalEndpointReuse = true
    let listener: NWListener
    do {
      listener = try NWListener(using: parameters)
    } catch {
      FileHandle.standardError.write("axe-reader: \(error)\n".data(using: .utf8)!)
      exit(1)
    }
    listener.newConnectionHandler = { connection in
      connection.start(queue: .main)
      connection.receive(minimumIncompleteLength: 1, maximumLength: 65536) { data, _, _, _ in
        let line = data.flatMap { String(data: $0, encoding: .utf8) }?.split(separator: "\r\n").first ?? ""
        Task { @MainActor in
          var status = "200 OK"
          var body: Data
          if line.hasPrefix("GET /describe") {
            do {
              body = try await reader.describe(lean: line.hasPrefix("GET /describe?lean"))
            } catch {
              status = "500 Error"
              body = "\(error)".data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /hid?") {
            let query = String(line.dropFirst("GET /hid?".count).prefix { $0 != " " })
            let script = query.removingPercentEncoding ?? query
            do {
              try await reader.sendHID(script)
              body = "ok".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = "\(error)".data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /keyboard?hardware=") {
            do {
              try reader.setHardwareKeyboard(line.hasPrefix("GET /keyboard?hardware=on"))
              body = "ok".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = "\(error)".data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /ping") {
            body = "ok \(udid)".data(using: .utf8)!
          } else {
            status = "404 Not Found"
            body = Data()
          }
          var reply = "HTTP/1.0 \(status)\r\nContent-Type: application/json\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n"
            .data(using: .utf8)!
          reply.append(body)
          connection.send(content: reply, completion: .contentProcessed { _ in connection.cancel() })
        }
      }
    }
    listener.stateUpdateHandler = { state in
      if case .failed(let error) = state {
        FileHandle.standardError.write("axe-reader: \(error)\n".data(using: .utf8)!)
        exit(1)
      }
    }
    listener.start(queue: .main)
    while true {
      try? await Task.sleep(for: .seconds(3600))
    }
  }
}
