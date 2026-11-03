import AppKit
import SwiftUI

@MainActor
final class SetupModel: ObservableObject {
    enum Phase {
        case loading
        case choose
        case working
        case done(SetupRun)
        case failed(SetupFailure)
    }

    enum PhonePhase {
        case idle
        case needsToken(String?)
        case working
        case ready(String)
        case failed(String)
    }

    @Published var phase: Phase = .loading
    @Published var clients: [Client] = []
    @Published var selected: Set<String> = []
    @Published var secContact = ""
    @Published var askSecContact = false
    @Published var phone: PhonePhase = .idle
    @Published var phoneToken = ""
    @Published var log = ""

    var detected: [Client] { clients.filter(\.detected) }

    func load() {
        phase = .loading
        Task {
            let (result, stderr) = await Setup.list()
            log = stderr
            switch result {
            case .success(let list):
                clients = list.clients
                selected = Set(list.clients.filter(\.detected).map(\.id))
                askSecContact = !list.secUserAgentSet
                phase = .choose
            case .failure(let failure):
                phase = .failed(failure)
            }
        }
    }

    func connect() {
        let ids = detected.map(\.id).filter { selected.contains($0) }
        guard !ids.isEmpty else { return }
        phase = .working
        Task {
            let (result, stderr) = await Setup.connect(ids: ids, secContact: askSecContact ? secContact : nil)
            log = stderr
            switch result {
            case .success(let run): phase = .done(run)
            case .failure(let failure): phase = .failed(failure)
            }
        }
    }

    func setUpPhone() {
        let token = phoneToken
        phone = .working
        Task {
            let (result, stderr) = await Phone.install(authtoken: token.isEmpty ? nil : token)
            log += (log.isEmpty ? "" : "\n") + stderr
            switch result {
            case .success(let status):
                if let url = status.url, status.running {
                    phone = .ready(url)
                } else if status.needs == "ngrok_authtoken" {
                    phone = .needsToken(token.isEmpty ? nil : status.message)
                } else {
                    phone = .failed(status.message)
                }
            case .failure(let failure):
                phone = .failed(failure.message)
            }
        }
    }

    /// Shows the link if phone access is already set up on this Mac.
    func checkPhone() {
        Task {
            if let status = await Phone.status(), status.running, let url = status.url {
                phone = .ready(url)
            }
        }
    }
}

struct ContentView: View {
    @StateObject private var model = SetupModel()
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Group {
                switch model.phase {
                case .loading: progress("Looking for AI apps on this Mac…")
                case .choose: choose
                case .working: progress("Connecting…")
                case .done(let run): done(run)
                case .failed(let failure): failed(failure)
                }
            }
            .padding(.top, 20)
        }
        .padding(28)
        .frame(width: 460, alignment: .topLeading)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear { model.load() }
    }

    private var header: some View {
        HStack(spacing: 14) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 52, height: 52)
            Text("Connect your AI to the market")
                .font(.title2.weight(.semibold))
        }
    }

    private func progress(_ text: String) -> some View {
        HStack(spacing: 10) {
            ProgressView().controlSize(.small)
            Text(text)
        }
    }

    // MARK: choose

    private var choose: some View {
        VStack(alignment: .leading, spacing: 16) {
            if model.detected.isEmpty {
                Text("No AI apps found on this Mac. Market Search works with the Claude app, Claude Code, Cursor, VS Code, Windsurf, Codex and Gemini CLI.")
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Button("Check again") { model.load() }
                    Spacer()
                    Button("Get the Claude app") { NSWorkspace.shared.open(URL(string: "https://claude.ai/download")!) }
                        .keyboardShortcut(.defaultAction)
                        .controlSize(.large)
                }
            } else {
                Text("Market Search gives your AI live odds, prices, bonds and SEC filings. Connect it to:")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(model.detected) { client in
                        Toggle(isOn: Binding(
                            get: { model.selected.contains(client.id) },
                            set: { on in if on { model.selected.insert(client.id) } else { model.selected.remove(client.id) } }
                        )) {
                            HStack(spacing: 6) {
                                Text(client.name)
                                if client.connected {
                                    Text("connected").font(.footnote).foregroundStyle(.secondary)
                                }
                            }
                        }
                        .toggleStyle(.checkbox)
                    }
                }
                if model.askSecContact {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Email for SEC filings (optional)")
                        TextField("", text: $model.secContact, prompt: Text("you@example.com"))
                            .textFieldStyle(.roundedBorder)
                        Text("The SEC asks for a contact before it serves filings. It stays on this Mac.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }
                HStack {
                    Spacer()
                    Button("Connect") { model.connect() }
                        .keyboardShortcut(.defaultAction)
                        .controlSize(.large)
                        .disabled(model.selected.isEmpty)
                }
            }
        }
    }

    // MARK: done

    private func done(_ run: SetupRun) -> some View {
        let connected = run.results.filter(\.connected)
        let problems = run.results.filter { $0.action == "failed" || $0.action == "skipped" }
        let restart = connected.contains { $0.restartRequired }
        let claudeConnected = connected.contains { $0.id == "claude-desktop" }
        return VStack(alignment: .leading, spacing: 16) {
            Text(connected.isEmpty ? "Nothing was connected." : "Market Search is connected.")
                .font(.headline)
            VStack(alignment: .leading, spacing: 6) {
                ForEach(connected) { result in
                    Label(result.name, systemImage: "checkmark.circle.fill")
                        .symbolRenderingMode(.multicolor)
                }
                ForEach(problems) { result in
                    Label("\(result.name): \(result.message)", systemImage: "exclamationmark.triangle.fill")
                        .symbolRenderingMode(.multicolor)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if !connected.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Next").font(.headline)
                    if restart {
                        Text(claudeConnected
                             ? "Restart the Claude app so it sees Market Search."
                             : "Restart the apps above so they see Market Search.")
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Text("Then ask: “Plot the odds of a recession against the S&P 500 and oil this year.”")
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                }
            }
            Divider()
            phoneSection
            details
            HStack {
                if claudeConnected {
                    Button("Restart Claude") { Task { await Setup.restartClaude() } }
                }
                Spacer()
                Button("Done") { NSApp.terminate(nil) }
                    .keyboardShortcut(.defaultAction)
                    .controlSize(.large)
            }
        }
        .onAppear { model.checkPhone() }
    }

    // MARK: phone

    private var phoneSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Use it on your phone").font(.headline)
            switch model.phone {
            case .idle:
                Text("Keep this Mac on, and Claude on your phone and on the web can use Market Search through a private link.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Set up phone access") { model.setUpPhone() }
            case .needsToken(let message):
                Text("Phone access runs through ngrok, a free tunnel. Copy your token from ngrok and paste it here.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let message {
                    Text(message).font(.footnote).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                HStack {
                    Button("Get your free token") { NSWorkspace.shared.open(Phone.tokenPage) }
                    TextField("", text: $model.phoneToken, prompt: Text("ngrok token"))
                        .textFieldStyle(.roundedBorder)
                    Button("Continue") { model.setUpPhone() }
                        .disabled(model.phoneToken.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            case .working:
                progress("Setting up phone access…")
            case .ready(let url):
                Text("In Claude, open Settings, then Connectors, then Add custom connector, and paste this link. Keep it private: it opens your Market Search.")
                    .fixedSize(horizontal: false, vertical: true)
                Text(url)
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
                    .lineLimit(2)
                HStack {
                    Button(copied ? "Copied" : "Copy link") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(url, forType: .string)
                        copied = true
                    }
                    Button("Open Claude connectors") { NSWorkspace.shared.open(Phone.connectorsPage) }
                }
            case .failed(let message):
                Text(message)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                Button("Try again") { model.setUpPhone() }
            }
        }
    }

    // MARK: failed

    private func failed(_ failure: SetupFailure) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Could not connect").font(.headline)
            Text(failure.message)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            details
            HStack {
                Spacer()
                Button("Try again") { model.load() }
                    .keyboardShortcut(.defaultAction)
                    .controlSize(.large)
            }
        }
    }

    private var details: some View {
        DisclosureGroup("Details") {
            ScrollView {
                Text(model.log.trimmingCharacters(in: .whitespacesAndNewlines))
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .frame(height: 120)
            .background(Color(nsColor: .textBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .font(.footnote)
    }
}
