import SwiftUI

@main
struct BrainDrainApp: App {
    private static let periodicRefreshInterval: TimeInterval = 5 * 60

    @State private var model = ProviderListModel()

    var body: some Scene {
        MenuBarExtra {
            ProviderPopover(model: model)
                .task {
                    await model.refreshAll()
                    await model.runPeriodicRefresh(every: Self.periodicRefreshInterval)
                }
        } label: {
            Image(systemName: "brain.head.profile")
        }
        .menuBarExtraStyle(.window)
    }
}
