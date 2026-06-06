import SwiftUI

@main
struct BrainDrainApp: App {
    @State private var model = ProviderListModel()

    var body: some Scene {
        MenuBarExtra {
            ProviderPopover(model: model)
                .task {
                    await model.refreshAll()
                }
        } label: {
            Image(systemName: "brain.head.profile")
        }
        .menuBarExtraStyle(.window)
    }
}
