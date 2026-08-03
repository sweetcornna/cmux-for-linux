import Darwin
import Foundation
import WebKit

enum BrowserPanelPurpose: String, Codable, Equatable, Sendable {
    case standard
    case code
}

enum ContextualSurfaceCreationKind: Equatable, Sendable {
    case terminal
    case code

    static func resolve(panel: (any Panel)?) -> Self {
        guard let browser = panel as? BrowserPanel, browser.purpose == .code else {
            return .terminal
        }
        return .code
    }
}

private enum CodeSidecarError: LocalizedError {
    case missingResource(String)
    case noLoopbackPort
    case launchFailed

    var errorDescription: String? {
        switch self {
        case .missingResource(let name): return "Missing bundled Code resource: \(name)"
        case .noLoopbackPort: return "No loopback port was available"
        case .launchFailed: return "The Code server did not start"
        }
    }
}

@MainActor
final class CodeSidecarService {
    static let shared = CodeSidecarService()

    private struct RunningProcess {
        let process: Process
        let url: URL
        let logHandle: FileHandle
    }

    private var activeSurfaceIDs = Set<UUID>()
    private var running: RunningProcess?
    private var startupTask: Task<URL, Error>?

    static func launcherURL(bundle: Bundle = .main) -> URL? {
        bundle.url(
            forResource: "code",
            withExtension: "html",
            subdirectory: "markdown-viewer/webviews-app"
        )
    }

    func mount(surfaceID: UUID, workingDirectory: String?) async throws -> URL {
        activeSurfaceIDs.insert(surfaceID)
        if let running, running.process.isRunning {
            return running.url
        }
        if let running {
            running.logHandle.closeFile()
            self.running = nil
        }

        let task: Task<URL, Error>
        if let startupTask {
            task = startupTask
        } else {
            let newTask = Task { @MainActor [weak self] in
                guard let self else { throw CancellationError() }
                return try await self.start(workingDirectory: workingDirectory)
            }
            startupTask = newTask
            task = newTask
        }

        do {
            let url = try await task.value
            startupTask = nil
            guard activeSurfaceIDs.contains(surfaceID) else {
                if activeSurfaceIDs.isEmpty {
                    stop()
                }
                throw CancellationError()
            }
            return url
        } catch {
            startupTask = nil
            activeSurfaceIDs.remove(surfaceID)
            throw error
        }
    }

    func release(surfaceID: UUID) {
        activeSurfaceIDs.remove(surfaceID)
        guard activeSurfaceIDs.isEmpty else { return }
        startupTask?.cancel()
        startupTask = nil
        stop()
    }

    func stop() {
        activeSurfaceIDs.removeAll()
        startupTask?.cancel()
        startupTask = nil
        guard let running else { return }
        self.running = nil
        if running.process.isRunning {
            running.process.terminate()
        }
        running.logHandle.closeFile()
    }

    private func start(workingDirectory: String?) async throws -> URL {
        guard let resources = Bundle.main.resourceURL else {
            throw CodeSidecarError.missingResource("Resources")
        }
        let executable = resources.appendingPathComponent("bin/cmux-code-sidecar", isDirectory: false)
        let staticDirectory = resources.appendingPathComponent("code-sidecar/client", isDirectory: true)
        let architecture = Self.processArchitecture
        let resourceMonitor = resources.appendingPathComponent(
            "code-sidecar/resource-monitor/darwin-\(architecture)/cmux-code-resource-monitor",
            isDirectory: false
        )
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw CodeSidecarError.missingResource(executable.lastPathComponent)
        }
        guard FileManager.default.fileExists(atPath: staticDirectory.appendingPathComponent("index.html").path) else {
            throw CodeSidecarError.missingResource("Code client")
        }
        guard FileManager.default.isExecutableFile(atPath: resourceMonitor.path) else {
            throw CodeSidecarError.missingResource(resourceMonitor.lastPathComponent)
        }

        let root = try Self.dataDirectory()
        let port = try Self.allocateLoopbackPort()
        let url = URL(string: "http://127.0.0.1:\(port)/")!
        let logURL = root.appendingPathComponent("server.log", isDirectory: false)
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: nil)
        }
        let logHandle = try FileHandle(forWritingTo: logURL)
        try logHandle.seekToEnd()

        let process = Process()
        process.executableURL = executable
        var arguments = [
            "serve",
            "--mode", "desktop",
            "--host", "127.0.0.1",
            "--port", String(port),
            "--base-dir", root.path,
            "--no-browser",
        ]
        if let workingDirectory = Self.validWorkingDirectory(workingDirectory) {
            arguments.append(contentsOf: ["--auto-bootstrap-project-from-cwd", workingDirectory])
        }
        process.arguments = arguments
        var environment = ProcessInfo.processInfo.environment
        environment["CMUX_CODE_STATIC_DIR"] = staticDirectory.path
        environment["CMUX_CODE_RESOURCE_MONITOR_PATH"] = resourceMonitor.path
        environment["NO_COLOR"] = "1"
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let inheritedPath = environment["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        environment["PATH"] = "\(home)/.local/bin:\(home)/.bun/bin:/opt/homebrew/bin:/usr/local/bin:\(inheritedPath)"
        process.environment = environment
        process.standardOutput = logHandle
        process.standardError = logHandle

        do {
            try process.run()
            try await Self.waitUntilReady(process: process, url: url)
            try Task.checkCancellation()
            guard !activeSurfaceIDs.isEmpty else { throw CancellationError() }
            running = RunningProcess(process: process, url: url, logHandle: logHandle)
            return url
        } catch {
            if process.isRunning { process.terminate() }
            logHandle.closeFile()
            throw error
        }
    }

    private static func waitUntilReady(process: Process, url: URL) async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 0.4
        configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }

        let deadline = ContinuousClock.now.advanced(by: .seconds(15))
        while ContinuousClock.now < deadline {
            try Task.checkCancellation()
            guard process.isRunning else { throw CodeSidecarError.launchFailed }
            do {
                var request = URLRequest(url: url)
                request.timeoutInterval = 0.4
                let (_, response) = try await session.data(for: request)
                if let response = response as? HTTPURLResponse, (200..<400).contains(response.statusCode) {
                    return
                }
            } catch {
                // Startup commonly refuses the first few connections while the database opens.
            }
            try await ContinuousClock().sleep(for: .milliseconds(100))
        }
        throw CodeSidecarError.launchFailed
    }

    private static var processArchitecture: String {
#if arch(arm64)
        "arm64"
#else
        "x64"
#endif
    }

    private static func validWorkingDirectory(_ candidate: String?) -> String? {
        guard let candidate = candidate?.trimmingCharacters(in: .whitespacesAndNewlines),
              !candidate.isEmpty else { return nil }
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: candidate, isDirectory: &isDirectory), isDirectory.boolValue else {
            return nil
        }
        return candidate
    }

    private static func dataDirectory() throws -> URL {
        let base = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let bundleComponent = (Bundle.main.bundleIdentifier ?? "cmux")
            .replacingOccurrences(of: "/", with: "_")
        let directory = base
            .appendingPathComponent("cmux", isDirectory: true)
            .appendingPathComponent("code", isDirectory: true)
            .appendingPathComponent(bundleComponent, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    private static func allocateLoopbackPort() throws -> Int {
        for _ in 0..<8 {
            let descriptor = socket(AF_INET, SOCK_STREAM, 0)
            guard descriptor >= 0 else { break }
            defer { close(descriptor) }

            var address = sockaddr_in()
            address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            address.sin_family = sa_family_t(AF_INET)
            address.sin_port = 0
            address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
            let bindResult = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            guard bindResult == 0 else { continue }

            var bound = sockaddr_in()
            var length = socklen_t(MemoryLayout<sockaddr_in>.size)
            let nameResult = withUnsafeMutablePointer(to: &bound) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    getsockname(descriptor, $0, &length)
                }
            }
            guard nameResult == 0 else { continue }
            let port = Int(UInt16(bigEndian: bound.sin_port))
            if (1...65535).contains(port) { return port }
        }
        throw CodeSidecarError.noLoopbackPort
    }
}

@MainActor
final class CodeSurfaceMessageHandler: NSObject, WKScriptMessageHandler {
    static let name = "cmuxCode"
    weak var panel: BrowserPanel?

    init(panel: BrowserPanel) {
        self.panel = panel
    }

    func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame,
              let body = message.body as? [String: Any],
              body["type"] as? String == "mount",
              let panel,
              panel.purpose == .code else { return }
        panel.mountCodeSidecar()
    }
}
