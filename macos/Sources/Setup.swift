import AppKit
import Foundation

/// One AI app as `market-search setup --list --json` describes it.
struct Client: Decodable, Identifiable, Hashable {
    var id: String
    var name: String
    var detected: Bool
    var connected: Bool
    var config: String?
    var command: String?
    var restartRequired: Bool

    enum CodingKeys: String, CodingKey {
        case id, name, detected, connected, config, command
        case restartRequired = "restart_required"
    }
}

struct ClientList: Decodable {
    var binary: String
    var secUserAgentSet: Bool
    var clients: [Client]

    enum CodingKeys: String, CodingKey {
        case binary, clients
        case secUserAgentSet = "sec_user_agent_set"
    }
}

/// What `market-search setup --yes --json` did in one app.
struct ClientResult: Decodable, Identifiable, Hashable {
    var id: String
    var name: String
    /// added, updated, unchanged, removed, not_found, skipped, failed
    var action: String
    var config: String?
    var message: String
    var restartRequired: Bool

    enum CodingKeys: String, CodingKey {
        case id, name, action, config, message
        case restartRequired = "restart_required"
    }

    var connected: Bool { ["added", "updated", "unchanged"].contains(action) }
}

struct SetupRun: Decodable {
    var binary: String
    var secUserAgentSet: Bool
    var results: [ClientResult]

    enum CodingKeys: String, CodingKey {
        case binary, results
        case secUserAgentSet = "sec_user_agent_set"
    }
}

struct SetupFailure: Error {
    var message: String
    var output: String
}

/// Puts the market-search this app carries in ~/.local/bin and runs it the
/// way a person would in a terminal, with their PATH, so `setup` finds the
/// same Claude Code, Codex and Gemini CLI they use.
enum Setup {
    /// HOME as the environment gives it, so a test run with HOME set to a
    /// scratch folder stays inside it.
    static var home: URL {
        if let h = ProcessInfo.processInfo.environment["HOME"], !h.isEmpty {
            return URL(fileURLWithPath: h, isDirectory: true)
        }
        return URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
    }

    static var helperDestination: URL {
        home.appendingPathComponent(".local/bin/market-search")
    }

    /// Copies Contents/Helpers/market-search to ~/.local/bin/market-search
    /// (0755). The copy lands beside the old file and is renamed over it, so an
    /// app already running the old one is not disturbed. AI apps are pointed at
    /// this copy, so it keeps working after the app is thrown away.
    static func installHelper() throws -> URL {
        let fm = FileManager.default
        let source = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/market-search")
        guard fm.isExecutableFile(atPath: source.path) else {
            throw SetupFailure(message: "This copy of the app is incomplete. Download it again from eliterminal.com.", output: "missing \(source.path)")
        }
        let dest = helperDestination
        let binDir = dest.deletingLastPathComponent()
        try fm.createDirectory(at: binDir, withIntermediateDirectories: true)
        let staging = binDir.appendingPathComponent(".market-search.new")
        try? fm.removeItem(at: staging)
        try fm.copyItem(at: source, to: staging)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: staging.path)
        removexattr(staging.path, "com.apple.quarantine", 0)
        if rename(staging.path, dest.path) != 0 {
            let err = String(cString: strerror(errno))
            try? fm.removeItem(at: staging)
            throw SetupFailure(message: "Could not put market-search in \(binDir.path).", output: err)
        }
        return dest
    }

    /// The person's PATH as a terminal would have it. A GUI app starts with far
    /// less, and many agents live in folders only ~/.zshrc adds, so the usual
    /// ones are added up front.
    static func searchPath() -> String {
        let fm = FileManager.default
        let h = home.path
        var dirs = [
            "\(h)/.local/bin", "\(h)/.claude/local", "\(h)/.npm-global/bin", "\(h)/.bun/bin",
            "\(h)/.volta/bin", "\(h)/.deno/bin", "\(h)/bin", "\(h)/Library/pnpm",
        ]
        let nvm = "\(h)/.nvm/versions/node"
        if let versions = try? fm.contentsOfDirectory(atPath: nvm) {
            let sorted = versions.sorted { $0.compare($1, options: .numeric) == .orderedDescending }
            dirs += sorted.map { "\(nvm)/\($0)/bin" }
        }
        dirs += ["/opt/homebrew/bin", "/usr/local/bin"]
        var path = dirs.filter { fm.fileExists(atPath: $0) }
        let inherited = ProcessInfo.processInfo.environment["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        for dir in inherited.split(separator: ":").map(String.init) where !path.contains(dir) {
            path.append(dir)
        }
        for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] where !path.contains(dir) {
            path.append(dir)
        }
        return path.joined(separator: ":")
    }

    struct Output {
        var status: Int32
        var stdout: String
        var stderr: String
    }

    private final class Buffer: @unchecked Sendable {
        private let lock = NSLock()
        private var data = Data()
        func append(_ chunk: Data) { lock.lock(); data.append(chunk); lock.unlock() }
        var text: String { lock.lock(); defer { lock.unlock() }; return String(decoding: data, as: UTF8.self) }
    }

    /// Runs ~/.local/bin/market-search with `args`; stdout carries the JSON,
    /// stderr the progress people can read under Details.
    static func run(_ args: [String], timeout: TimeInterval = 120) async -> Output {
        await withCheckedContinuation { continuation in
            let process = Process()
            process.executableURL = helperDestination
            process.arguments = args
            var env = ProcessInfo.processInfo.environment
            env["HOME"] = home.path
            env["PATH"] = searchPath()
            process.environment = env
            process.currentDirectoryURL = home
            process.standardInput = FileHandle.nullDevice
            let out = Pipe(), err = Pipe()
            process.standardOutput = out
            process.standardError = err
            let stdout = Buffer(), stderr = Buffer()
            out.fileHandleForReading.readabilityHandler = { h in let d = h.availableData; if !d.isEmpty { stdout.append(d) } }
            err.fileHandleForReading.readabilityHandler = { h in let d = h.availableData; if !d.isEmpty { stderr.append(d) } }
            let stop = DispatchWorkItem { if process.isRunning { process.terminate() } }
            process.terminationHandler = { p in
                stop.cancel()
                out.fileHandleForReading.readabilityHandler = nil
                err.fileHandleForReading.readabilityHandler = nil
                stdout.append(out.fileHandleForReading.readDataToEndOfFile())
                stderr.append(err.fileHandleForReading.readDataToEndOfFile())
                continuation.resume(returning: Output(status: p.terminationStatus, stdout: stdout.text, stderr: stderr.text))
            }
            do {
                try process.run()
                DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: stop)
            } catch {
                continuation.resume(returning: Output(status: -1, stdout: "", stderr: "could not start \(helperDestination.path): \(error.localizedDescription)"))
            }
        }
    }

    private struct ErrorReply: Decodable { var error: String }

    /// The JSON object on stdout, or the reason there is none.
    static func decode<T: Decodable>(_ type: T.Type, from out: Output) -> Result<T, SetupFailure> {
        let details = [out.stderr, out.stdout].filter { !$0.isEmpty }.joined(separator: "\n")
        if let start = out.stdout.firstIndex(of: "{") {
            let json = Data(out.stdout[start...].utf8)
            if let value = try? JSONDecoder().decode(T.self, from: json) {
                return .success(value)
            }
            // `{"error": "..."}` is market-search saying why it stopped.
            if let reply = try? JSONDecoder().decode(ErrorReply.self, from: json) {
                return .failure(SetupFailure(message: reply.error, output: details))
            }
        }
        if out.status == 15 || out.status == 143 {
            return .failure(SetupFailure(message: "Setup took too long and was stopped. Try again.", output: details))
        }
        let last = out.stderr.split(separator: "\n").last.map(String.init)?.trimmingCharacters(in: .whitespaces)
        return .failure(SetupFailure(message: last.flatMap { $0.isEmpty ? nil : $0 } ?? "Setup did not finish. Try again.", output: details))
    }

    static func list() async -> (Result<ClientList, SetupFailure>, String) {
        do {
            _ = try installHelper()
        } catch let failure as SetupFailure {
            return (.failure(failure), failure.output)
        } catch {
            return (.failure(SetupFailure(message: "Could not put market-search in \(helperDestination.deletingLastPathComponent().path).", output: error.localizedDescription)), "")
        }
        let out = await run(["setup", "--list", "--json", "--binary", helperDestination.path])
        return (decode(ClientList.self, from: out), out.stderr)
    }

    static func connect(ids: [String], secContact: String?) async -> (Result<SetupRun, SetupFailure>, String) {
        var args = ["setup", "--yes", "--json", "--binary", helperDestination.path, "--client", ids.joined(separator: ",")]
        if let contact = secContact?.trimmingCharacters(in: .whitespacesAndNewlines), !contact.isEmpty {
            args += ["--sec-contact", contact]
        }
        let out = await run(args)
        return (decode(SetupRun.self, from: out), out.stderr)
    }

    static let claudeBundleID = "com.anthropic.claudefordesktop"

    static var claudeIsRunning: Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: claudeBundleID).isEmpty
    }

    /// Quits the Claude app and opens it again, so it reads its new settings.
    /// The Claude app can write its settings file back as it quits and drop
    /// entries it did not know about, so the entry is checked again in between.
    static func restartClaude() async {
        for app in NSRunningApplication.runningApplications(withBundleIdentifier: claudeBundleID) {
            app.terminate()
        }
        for _ in 0..<40 where claudeIsRunning {
            try? await Task.sleep(nanoseconds: 250_000_000)
        }
        _ = await run(["setup", "--yes", "--json", "--binary", helperDestination.path, "--client", "claude-desktop"])
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: claudeBundleID) else { return }
        _ = try? await NSWorkspace.shared.openApplication(at: url, configuration: NSWorkspace.OpenConfiguration())
    }
}
