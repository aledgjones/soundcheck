// Keeps one FBSimulatorControl session open and serves the booted simulator's accessibility tree as JSON on
// 127.0.0.1:9009 (GET /describe, GET /ping), and input through one held HID session (GET /hid). /ping answers
// `ok <udid> <build id> <pid> <started, unix seconds>`, so the driver can replace an old or stale reader by its pid.
// The fetch and serialisation follow AXe 1.8.0's AccessibilityFetcher (MIT, github.com/cameroncooke/AXe), so the
// JSON is `axe describe-ui`'s schema; build.sh builds idb's frameworks from source. Test tool, simulator only.
//   reader --udid <udid> [--port 9009]
import FBControlCore
import FBSimulatorControl
import Foundation
import Network

@objc final class NoEvents: NSObject, FBEventReporter {
  var metadata: [String: String] = [:]
  func report(_ subject: FBEventReporterSubject) {}
  func addMetadata(_ metadata: [String: String]) {}
}

struct Hold {
  let x: Double
  let y: Double
  let originX: Double
  let originY: Double
}

@MainActor
final class Reader {
  // AXe's legacy public schema. Xcode 27.0 returned an empty tree when `.traits` was requested; 27.1 doesn't.
  static let requestKeys: Set<FBAXKeys> = [
    .label, .frame, .value, .uniqueID, .type, .title, .frameDict, .help, .enabled, .customActions, .role,
    .roleDescription, .subrole, .contentRequired, .pid, .traits,
  ]
  // What `see` reads (traits carry selected); fewer keys means fewer round trips per element. The pid costs none,
  // and tells another process's element from the app's own with the same label and frame.
  static let leanKeys: Set<FBAXKeys> = [.label, .value, .uniqueID, .frame, .frameDict, .type, .enabled, .traits, .pid]

  let udid: String
  var simulator: FBSimulator?
  var hid: FBSimulatorHID?
  var held: Hold?
  // Bumped whenever the held touch changes, which ends the old one's keep-alive.
  var holdID = 0
  // The latest frontmost read's elements, by process, label and frame, and its screen: what `others` compares hits with.
  var known = Set<String>()
  var screen = CGRect.zero
  var reloadedPID: Int?

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
      throw NSError(domain: "reader", code: 1, userInfo: [NSLocalizedDescriptionKey: "No simulator \(udid)"])
    }
    self.simulator = simulator
  }

  // The frontmost app's tree; like AXe, retry briefly while it has no children (just after boot or a launch).
  func describe(lean: Bool = false) async throws -> Data {
    guard let simulator else {
      throw NSError(domain: "reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    let latest = try await read(simulator, lean: lean)
    // A relaunched app reads as an empty 0×0 node until Accessibility is loaded into the new process, which idb
    // only does on a reader's first read; load it once per app process, through the first read's session (a
    // second session ends the reader), rather than restarting the reader.
    guard !Reader.hasDescendant(latest), let pid = Reader.rootPID(latest), pid != reloadedPID else {
      return latest
    }
    reloadedPID = pid
    try FBSimulatorControlFrameworkLoader.reloadAccessibility(withTimeout: 5)
    return try await read(simulator, lean: lean)
  }

  // What other processes draw over the app (the document picker, Google's sign-in in SafariViewService and WebKit),
  // as a root holding each element the latest frontmost read lacks. None of it is in that tree, no element of it
  // leads up to its own root, and idb's remote pass drops WebKit's hits, so it's found by hit-testing: a grid 80
  // points apart over the screen says where the sheet is, and one 32 points apart inside that finds what a
  // container doesn't list (the picker's 36-point toolbar buttons). A hit is 3 to 4 ms; serialising one is a round
  // trip per attribute of every descendant, so only a hit whose process, label and frame are new is serialised,
  // and no point inside a labelled leaf already found is hit again. The picker on an iPad takes about 4 s; with no
  // sheet up it is one hit.
  func others() async throws -> Data {
    guard let simulator else {
      throw NSError(domain: "reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }

    if screen.isEmpty {
      _ = try await describe(lean: true)
    }

    let screen = self.screen
    var known = self.known
    var found: [[String: Any]] = []
    var leaves: [CGRect] = []
    var sheet = CGRect.null

    func scan(_ area: CGRect, step: Double) async throws {
      var y = area.minY + step / 2
      while y < area.maxY {
        var x = area.minX + step / 2
        while x < area.maxX {
          let point = CGPoint(x: x, y: y)
          x += step
          // A point over nothing throws.
          guard !leaves.contains(where: { $0.contains(point) }),
            let hit = try? await simulator.accessibilityElement(at: point)
          else {
            continue
          }
          defer { hit.close() }

          // The key goes in with the element, so one that fails to serialise is tried again at its next point.
          guard !known.contains(try Reader.key(hit)),
            let element = try? hit.serialize(
              with: FBAccessibilityRequestOptions(nestedFormat: true, keys: Reader.leanKeys)
            ).elements as? [String: Any]
          else {
            continue
          }

          Reader.collect(element, keys: &known, leaves: &leaves)
          // A labelled leaf as large as a backdrop would hide everything drawn over it.
          leaves.removeAll { $0.width > screen.width / 2 || $0.height > screen.height / 2 }
          sheet = sheet.union(Reader.rect(element).intersection(screen))
          found.append(element)
        }

        y += step
      }
    }

    func centre() async throws -> String? {
      guard let hit = try? await simulator.accessibilityElement(at: CGPoint(x: screen.midX, y: screen.midY)) else {
        return nil
      }

      defer { hit.close() }
      return try Reader.key(hit)
    }

    // The app's own element at the centre means no sheet is up. A sheet still arriving reports its elements where
    // they aren't yet, so one whose centre changed during the scan is scanned again, once: the driver waits
    // 15 s for an answer.
    var at = try await centre()

    for _ in 0..<2 {
      known = self.known
      found = []
      leaves = []
      sheet = .null

      if let at, known.contains(at) {
        break
      }

      try await scan(screen, step: 80)
      try await scan(sheet, step: 32)
      let now = try await centre()

      if now == at {
        break
      }

      at = now
    }

    let root: [String: Any] = [
      "type": "Application",
      "frame": ["x": 0, "y": 0, "width": screen.width, "height": screen.height],
      "children": found,
    ]
    return try JSONSerialization.data(withJSONObject: [root], options: [.prettyPrinted])
  }

  // A hit's process, label and frame, read from idb's own element: its handle offers only `serialize`. build.sh pins idb,
  // so the field can't be renamed unseen, and a handle without it is an error, never an empty answer.
  static func key(_ hit: FBAccessibilityElement) throws -> String {
    guard let element = Mirror(reflecting: hit).children.first(where: { $0.label == "element" })?.value as? NSObject,
      let frame = (element.value(forKey: "accessibilityFrame") as? NSValue)?.rectValue
    else {
      throw NSError(
        domain: "reader", code: 7, userInfo: [NSLocalizedDescriptionKey: "idb's accessibility handle has no element"]
      )
    }

    let label = element.perform(Selector(("accessibilityLabel")))?.takeUnretainedValue() as? String
    let pid = (element.value(forKey: "translation") as? NSObject)?.value(forKey: "pid") as? NSNumber
    return key(pid: pid, label: label, frame: frame)
  }

  static func key(_ element: [String: Any]) -> String {
    // idb's pid is an Int32, which bridges to NSNumber and not to Int.
    key(pid: element["pid"] as? NSNumber, label: element["AXLabel"] as? String, frame: rect(element))
  }

  // The picker's root is an unlabelled group the size of the screen, as several of the app's own are, so the
  // process is part of what an element is.
  static func key(pid: NSNumber?, label: String?, frame: CGRect) -> String {
    "\(pid?.intValue ?? 0) \(label ?? "") \(frame.minX) \(frame.minY) \(frame.width) \(frame.height)"
  }

  static func rect(_ element: [String: Any]) -> CGRect {
    // A hit's frame holds CGFloats and a parsed tree's holds NSNumbers; both bridge to NSNumber.
    let frame = element["frame"] as? [String: Any] ?? [:]
    let box = ["x", "y", "width", "height"].map { (frame[$0] as? NSNumber)?.doubleValue ?? 0 }
    return CGRect(x: box[0], y: box[1], width: box[2], height: box[3])
  }

  static func collect(_ element: [String: Any], keys: inout Set<String>, leaves: inout [CGRect]) {
    keys.insert(key(element))
    let children = element["children"] as? [[String: Any]] ?? []
    // Only a leaf with a label is all there is at its points: the picker's toolbar is an unlabelled leaf group
    // with its buttons over it.
    if children.isEmpty, !(element["AXLabel"] as? String ?? "").isEmpty {
      leaves.append(rect(element))
    }

    for child in children {
      collect(child, keys: &keys, leaves: &leaves)
    }
  }

  func read(_ simulator: FBSimulator, lean: Bool) async throws -> Data {
    var latest = Data()
    for attempt in 0..<5 {
      let element = try await simulator.accessibilityElementForFrontmostApplication()
      defer { element.close() }
      let response = try element.serialize(
        with: FBAccessibilityRequestOptions(nestedFormat: true, keys: lean ? Reader.leanKeys : Reader.requestKeys)
      )
      latest = try JSONSerialization.data(
        withJSONObject: response.elements, options: [.prettyPrinted]
      )
      if Reader.hasDescendant(latest) {
        if let root = (response.elements as? [[String: Any]])?.first {
          var leaves: [CGRect] = []
          known = []
          Reader.collect(root, keys: &known, leaves: &leaves)
          screen = Reader.rect(root)
        }
        return latest
      }
      if attempt < 4 {
        try await Task.sleep(for: .milliseconds(50 * (1 << attempt)))
      }
    }
    return latest
  }

  // Sends a script of input events through one held HID session, rather than a process per touch (~1.2 s each).
  // Steps are separated by `;`: `d x y` touch down (or move, while down), `u x y` touch up, `w ms` wait, `k code`
  // key press, `kd code` / `ku code` key down and up, and a last `h` that leaves the touch down until `release`.
  // A touch still held from an earlier script goes back to where it began and lifts first.
  func sendHID(_ script: String) async throws {
    guard let simulator else {
      throw NSError(domain: "reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    if hid == nil {
      // Built directly: `connectToHID()`'s session goes dead without an error after any `xcodebuild` that loads a
      // workspace (taps land nowhere, reads still work), while one built here survives builds. Indigo takes no
      // input at all on Xcode 27, but before Xcode 27 there's no dtuhidd to connect to, so it's the fallback.
      if let dtuhid = try? FBSimulatorHID(for: simulator, transport: .dtuhid) {
        hid = dtuhid
      } else {
        hid = try FBSimulatorHID(for: simulator, transport: .indigo)
      }
    }
    guard let hid else { return }
    var events: [FBSimulatorHIDEvent] = []
    if let held {
      events = Reader.lift(held, back: true)
      self.held = nil
      holdID += 1
    }
    let steps = script.split(separator: ";")
    var origin: (x: Double, y: Double)?
    var last: (x: Double, y: Double)?
    var keep = false
    for (i, step) in steps.enumerated() {
      let parts = step.split(separator: " ")
      let n = parts.dropFirst().compactMap { Double($0) }
      switch (parts.first, n.count) {
      case ("h", 0) where parts.count == 1 && i == steps.count - 1 && last != nil: keep = true
      case ("d", 2):
        events.append(.touch(direction: .down, x: n[0], y: n[1]))
        origin = origin ?? (n[0], n[1])
        last = (n[0], n[1])
      case ("u", 2):
        events.append(.touch(direction: .up, x: n[0], y: n[1]))
        origin = nil
        last = nil
      case ("w", 1): events.append(.delay(n[0] / 1000))
      case ("k", 1): events.append(.shortKeyPress(UInt32(n[0])))
      case ("kd", 1): events.append(.keyboard(direction: .down, keyCode: UInt32(n[0])))
      case ("ku", 1): events.append(.keyboard(direction: .up, keyCode: UInt32(n[0])))
      case ("b", 0) where parts.count == 2 && parts[1] == "home": events.append(.shortButtonPress(.homeButton))
      default:
        throw NSError(domain: "reader", code: 5, userInfo: [NSLocalizedDescriptionKey: "Bad step: \(step)"])
      }
    }
    do {
      try await FBSimulatorHIDEvent.composite(events).sendAsync(on: hid)
    } catch {
      // A session from before a simulator reboot is dead; the next script opens a fresh one.
      self.hid = nil
      throw error
    }
    if keep, let origin, let last {
      held = Hold(x: last.x, y: last.y, originX: origin.x, originY: origin.y)
      holdID += 1
      keepAlive(holdID)
    }
  }

  // iOS ends a touch held perfectly still after a few seconds (a dragged row drops), so a held touch rocks a point
  // to and fro. After 30 s it goes back and lifts, so a forgotten one drops nothing and can't spoil later input.
  func keepAlive(_ id: Int) {
    Task { @MainActor in
      let start = Date()
      var nudge = 1.0
      while holdID == id, let held, let hid {
        if Date().timeIntervalSince(start) > 30 {
          try? await release(back: true)
          return
        }
        try? await FBSimulatorHIDEvent.touch(direction: .down, x: held.x + nudge, y: held.y).sendAsync(on: hid)
        nudge = 1 - nudge
        try? await Task.sleep(for: .milliseconds(400))
      }
    }
  }

  // Lifts the held touch where it is, or `back` along a straight line to where it began, so nothing is dropped.
  func release(back: Bool) async throws {
    guard let held, let hid else {
      throw NSError(domain: "reader", code: 6, userInfo: [NSLocalizedDescriptionKey: "No touch held"])
    }
    self.held = nil
    holdID += 1
    try await FBSimulatorHIDEvent.composite(Reader.lift(held, back: back)).sendAsync(on: hid)
  }

  static func lift(_ held: Hold, back: Bool) -> [FBSimulatorHIDEvent] {
    var events: [FBSimulatorHIDEvent] = []
    var end = (x: held.x, y: held.y)
    if back {
      let steps = max(1, Int((max(abs(held.originX - held.x), abs(held.originY - held.y)) / 10).rounded(.up)))
      for i in 1...steps {
        let t = Double(i) / Double(steps)
        let x = held.x + (held.originX - held.x) * t
        let y = held.y + (held.originY - held.y) * t
        events.append(.touch(direction: .down, x: x, y: y))
        events.append(.delay(0.03))
      }
      end = (held.originX, held.originY)
    }
    events.append(.touch(direction: .up, x: end.x, y: end.y))
    return events
  }

  // What Xcode's Device Manager toggles: with it off, the on-screen keyboard shows and iOS ignores the reader's typing, which
  // arrives as a hardware keyboard. CoreSimulator's SimDevice method is private, so it's called through the runtime.
  func setHardwareKeyboard(_ enabled: Bool) throws {
    guard let device = simulator?.value(forKey: "device") as? NSObject else {
      throw NSError(domain: "reader", code: 2, userInfo: [NSLocalizedDescriptionKey: "Not set up"])
    }
    let selector = NSSelectorFromString("setHardwareKeyboardEnabled:keyboardType:error:")
    guard device.responds(to: selector) else {
      throw NSError(domain: "reader", code: 3, userInfo: [NSLocalizedDescriptionKey: "No hardware keyboard switch"])
    }
    typealias Set = @convention(c) (NSObject, Selector, Bool, UInt32, AutoreleasingUnsafeMutablePointer<NSError?>?)
      -> Bool
    let set = unsafeBitCast(device.method(for: selector), to: Set.self)
    var error: NSError?
    if !set(device, selector, enabled, 0, &error) {
      throw error ?? NSError(domain: "reader", code: 4, userInfo: [NSLocalizedDescriptionKey: "Switch refused"])
    }
  }

  static func hasDescendant(_ data: Data) -> Bool {
    guard let object = try? JSONSerialization.jsonObject(with: data) else {
      return false
    }
    let roots = (object as? [[String: Any]]) ?? ((object as? [String: Any]).map { [$0] } ?? [])
    return roots.contains { ($0["children"] as? [Any])?.isEmpty == false }
  }

  static func rootPID(_ data: Data) -> Int? {
    let object = try? JSONSerialization.jsonObject(with: data)
    let root = (object as? [[String: Any]])?.first ?? (object as? [String: Any])
    return root?["pid"] as? Int
  }
}

@main
struct Main {
  static func main() async {
    let started = Int(Date().timeIntervalSince1970)
    let args = CommandLine.arguments
    func option(_ name: String) -> String? {
      args.firstIndex(of: name).flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
    }
    guard let udid = option("--udid") else {
      FileHandle.standardError.write("usage: reader --udid <udid> [--port 9009]\n".data(using: .utf8)!)
      exit(2)
    }
    let port = NWEndpoint.Port(rawValue: UInt16(option("--port") ?? "9009") ?? 9009)!
    let reader = Reader(udid: udid)
    do {
      try reader.setUp()
    } catch {
      FileHandle.standardError.write("reader: \(error)\n".data(using: .utf8)!)
      exit(1)
    }
    let parameters = NWParameters.tcp
    parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: port)
    parameters.allowLocalEndpointReuse = true
    let listener: NWListener
    do {
      listener = try NWListener(using: parameters)
    } catch {
      FileHandle.standardError.write("reader: \(error)\n".data(using: .utf8)!)
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
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /others") {
            do {
              body = try await reader.others()
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /screenshot"), let simulator = reader.simulator {
            // From the framebuffer the reader already holds: `simctl io screenshot` starts a process each time, about
            // 0.5 s idle and over a second on a busy Mac.
            do {
              body = try await simulator.takeScreenshot(format: .png)
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /launch?"), let simulator = reader.simulator {
            // Quits and launches through CoreSimulator, as simctl does, without starting two simctl processes a spec.
            let bundle = String(line.dropFirst("GET /launch?".count).prefix { $0 != " " })
            try? simulator.device.terminateApplication(withID: bundle)
            do {
              let pid = try await withCheckedThrowingContinuation { (done: CheckedContinuation<pid_t, Error>) in
                simulator.device.launchApplicationAsync(withID: bundle, options: [:], completionQueue: simulator.workQueue) {
                  error, pid in
                  if let error {
                    done.resume(throwing: error)
                  } else {
                    done.resume(returning: pid)
                  }
                }
              }
              body = "\(pid)".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /alive?") {
            // A simulator's apps are the Mac's own processes.
            let pid = pid_t(line.dropFirst("GET /alive?".count).prefix { $0.isNumber }) ?? 0
            body = (pid > 0 && kill(pid, 0) == 0 ? "yes" : "no").data(using: .utf8)!
          } else if line.hasPrefix("GET /hid?") {
            let query = String(line.dropFirst("GET /hid?".count).prefix { $0 != " " })
            let script = query.removingPercentEncoding ?? query
            do {
              try await reader.sendHID(script)
              body = "ok".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /release") {
            do {
              try await reader.release(back: line.hasPrefix("GET /release?back"))
              body = "ok".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /keyboard?hardware=") {
            do {
              try reader.setHardwareKeyboard(line.hasPrefix("GET /keyboard?hardware=on"))
              body = "ok".data(using: .utf8)!
            } catch {
              status = "500 Error"
              body = error.localizedDescription.data(using: .utf8)!
            }
          } else if line.hasPrefix("GET /ping") {
            body = "ok \(udid) \(readerBuildID) \(getpid()) \(started)".data(using: .utf8)!
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
        FileHandle.standardError.write("reader: \(error)\n".data(using: .utf8)!)
        exit(1)
      }
    }
    listener.start(queue: .main)
    while true {
      try? await Task.sleep(for: .seconds(3600))
    }
  }
}
