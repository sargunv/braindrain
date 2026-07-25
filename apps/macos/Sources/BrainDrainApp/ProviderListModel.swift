import BrainDrainBindings
import Foundation

@Observable
@MainActor
final class ProviderListModel {
    private(set) var providers: [ProviderViewState]
    private(set) var isRefreshing = false
    private(set) var lastRefresh: Date?
    var selectedProviderID: String?

    init(providerIDs: [String] = providerIds()) {
        providers = providerIDs.map { ProviderViewState(id: $0) }
        selectedProviderID = providerIDs.first
    }

    var selectedProvider: ProviderViewState? {
        if let selectedProviderID,
           let provider = providers.first(where: { $0.id == selectedProviderID })
        {
            return provider
        }

        return providers.first
    }

    func refreshIfStale(minimumAge: TimeInterval) async {
        guard !isRefreshing else {
            return
        }

        if let lastRefresh,
           Date().timeIntervalSince(lastRefresh) < minimumAge
        {
            return
        }

        await refreshAll()
    }

    func runPeriodicRefresh(every interval: TimeInterval) async {
        while !Task.isCancelled {
            do {
                try await Task.sleep(for: .seconds(interval))
            } catch {
                return
            }

            await refreshIfStale(minimumAge: interval)
        }
    }

    func refreshAll() async {
        guard !isRefreshing else {
            return
        }

        isRefreshing = true
        defer {
            isRefreshing = false
            lastRefresh = Date()
        }

        await withTaskGroup(of: ProviderRefreshResult.self) { group in
            for provider in providers {
                let id = provider.id
                group.addTask {
                    do {
                        return try .success(id, await checkProvider(provider: id))
                    } catch {
                        return .failure(id, error.localizedDescription)
                    }
                }
            }

            for await result in group {
                apply(result)
            }
        }
    }

    private func apply(_ result: ProviderRefreshResult) {
        guard let index = providers.firstIndex(where: { $0.id == result.id }) else {
            return
        }

        switch result {
        case let .success(_, snapshot):
            providers[index].snapshot = snapshot
            providers[index].errorMessage = nil
        case let .failure(_, message):
            providers[index].errorMessage = message
        }
    }
}

enum ProviderRefreshResult {
    case success(String, FfiProviderSnapshot)
    case failure(String, String)

    var id: String {
        switch self {
        case let .success(id, _), let .failure(id, _):
            id
        }
    }
}

struct ProviderViewState: Identifiable {
    let id: String
    var snapshot: FfiProviderSnapshot?
    var errorMessage: String?

    var title: String {
        switch id {
        case "openai":
            "OpenAI"
        case "claude":
            "Claude Code"
        case "cursor":
            "Cursor"
        case "kimi":
            "Kimi Code"
        case "zai":
            "z.ai"
        case "opencode-go":
            "OpenCode Go"
        default:
            id
        }
    }
}
