import Foundation

/// The always-on service as `market-search mcp service ... --json` reports it.
struct PhoneStatus: Decodable {
    var installed: Bool
    var running: Bool
    /// up, down, unknown
    var tunnel: String
    var provider: String?
    var url: String?
    var serviceFile: String?
    var logs: String?
    var message: String
    /// nil, or what the person has to give before it can go on: ngrok_authtoken, ngrok_install
    var needs: String?

    enum CodingKeys: String, CodingKey {
        case installed, running, tunnel, provider, url, logs, message, needs
        case serviceFile = "service_file"
    }
}

/// Phone access: a background service on this Mac runs market-search with a
/// tunnel, so claude.ai on the web and on a phone can reach it through one
/// private link that survives restarts.
enum Phone {
    static let tokenPage = URL(string: "https://dashboard.ngrok.com/get-started/your-authtoken")!
    static let connectorsPage = URL(string: "https://claude.ai/settings/connectors")!

    static func status() async -> PhoneStatus? {
        let out = await Setup.run(["mcp", "service", "status", "--json"], timeout: 30)
        return try? Setup.decode(PhoneStatus.self, from: out).get()
    }

    /// Installs (or repairs) the service and waits for its link. A token is
    /// only needed the first time; ngrok keeps it after that.
    static func install(authtoken: String?) async -> (Result<PhoneStatus, SetupFailure>, String) {
        var args = ["mcp", "service", "install", "--yes", "--json", "--binary", Setup.helperDestination.path]
        if let token = authtoken?.trimmingCharacters(in: .whitespacesAndNewlines), !token.isEmpty {
            args += ["--authtoken", token]
        }
        let out = await Setup.run(args, timeout: 240)
        return (Setup.decode(PhoneStatus.self, from: out), out.stderr)
    }
}
