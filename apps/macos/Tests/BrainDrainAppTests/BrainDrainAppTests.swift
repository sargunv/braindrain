import AppKit
@testable import BrainDrainApp
import SwiftUI
import XCTest

final class BrainDrainAppTests: XCTestCase {
    @MainActor
    func testProviderPickerFitsPopoverWidth() {
        let providers = [
            ProviderPickerOption(id: "openai", title: "OpenAI"),
            ProviderPickerOption(id: "claude", title: "Claude Code"),
            ProviderPickerOption(id: "cursor", title: "Cursor"),
            ProviderPickerOption(id: "kimi", title: "Kimi Code"),
            ProviderPickerOption(id: "zai", title: "z.ai"),
            ProviderPickerOption(id: "opencode", title: "OpenCode"),
        ]
        var selection: String? = providers[0].id
        let picker = ProviderPicker(
            providers: providers,
            selection: Binding(
                get: { selection },
                set: { selection = $0 }
            )
        )
        let hostingController = NSHostingController(rootView: picker)

        let size = hostingController.sizeThatFits(
            in: NSSize(width: 360, height: CGFloat.greatestFiniteMagnitude)
        )

        XCTAssertLessThanOrEqual(size.width, 360)
    }
}
