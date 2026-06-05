import SwiftUI

struct ContentView: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("BrainDrain")
                .font(.largeTitle)
                .fontWeight(.semibold)
            Text("Rust core, SwiftUI shell.")
                .foregroundStyle(.secondary)
        }
        .padding(24)
        .frame(minWidth: 420, minHeight: 240)
    }
}
