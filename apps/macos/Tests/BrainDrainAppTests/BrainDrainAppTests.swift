import AppKit
@testable import BrainDrainApp
import BrainDrainBindings
import SwiftUI
import XCTest

final class BrainDrainAppTests: XCTestCase {
    @MainActor
    func testProviderListStartsOnOverview() {
        let model = ProviderListModel(providerIDs: ["openai", "claude"])

        XCTAssertNil(model.selectedProviderID)
        XCTAssertNil(model.selectedProvider)
    }

    func testProviderSummaryUsesHighestQuota() throws {
        let now = try XCTUnwrap(ISO8601DateFormatter().date(from: "2026-09-01T12:00:00Z"))
        let provider = ProviderViewState(
            id: "openai",
            snapshot: FfiProviderSnapshot(
                provider: "openai",
                source: "oauth",
                usage: FfiUsageSnapshot(
                    windows: [
                        FfiRateWindow(
                            id: "daily",
                            label: "Daily",
                            usedPercent: 18,
                            durationSeconds: nil,
                            resetsAt: nil
                        ),
                        FfiRateWindow(
                            id: "weekly",
                            label: "Weekly",
                            usedPercent: 82,
                            durationSeconds: nil,
                            resetsAt: "2026-09-02T12:00:00Z"
                        ),
                    ],
                    balances: [],
                    resetCredits: []
                ),
                identity: FfiAccountIdentity(email: nil, plan: "pro"),
                updatedAt: "2026-09-01T12:00:00Z"
            )
        )

        let summary = ProviderOverviewSummary(provider: provider, now: now)

        XCTAssertEqual(summary.valueText, "82%")
        XCTAssertEqual(summary.detailText, "Highest: Weekly · resets in 1d")
        XCTAssertEqual(summary.progress, 0.82)
        XCTAssertFalse(summary.hasError)
    }

    func testProviderSummaryDoesNotExposeErrorDetails() {
        let provider = ProviderViewState(
            id: "cursor",
            errorMessage: "credential file /Users/example/private.json is missing"
        )

        let summary = ProviderOverviewSummary(provider: provider)

        XCTAssertEqual(summary.valueText, "Error")
        XCTAssertEqual(summary.detailText, "Couldn’t refresh usage")
        XCTAssertFalse(summary.accessibilityValue.contains("/Users/example"))
        XCTAssertTrue(summary.hasError)
    }

    @MainActor
    func testProviderOverviewFitsPopoverWidth() {
        let providers = [
            ProviderViewState(id: "openai"),
            ProviderViewState(id: "claude"),
            ProviderViewState(id: "cursor"),
            ProviderViewState(id: "kimi"),
            ProviderViewState(id: "zai"),
            ProviderViewState(id: "opencode-go"),
            ProviderViewState(id: "google"),
        ]
        let overview = ProviderOverview(
            providers: providers,
            onSelect: { _ in }
        )
        let hostingController = NSHostingController(rootView: overview)

        let size = hostingController.sizeThatFits(
            in: NSSize(width: 360, height: 480)
        )

        XCTAssertLessThanOrEqual(size.width, 360)
    }

    @MainActor
    func testProviderPopoverKeepsItsHeightWhenShowingDetails() {
        let providerIDs = ["openai", "claude", "cursor", "kimi", "zai", "opencode-go", "google"]
        let overviewModel = ProviderListModel(providerIDs: providerIDs)
        let overviewController = NSHostingController(
            rootView: ProviderPopover(model: overviewModel)
        )
        let overviewSize = overviewController.sizeThatFits(
            in: NSSize(width: 360, height: CGFloat.greatestFiniteMagnitude)
        )

        let detailModel = ProviderListModel(providerIDs: providerIDs)
        detailModel.selectedProviderID = "openai"
        let detailController = NSHostingController(
            rootView: ProviderPopover(model: detailModel)
        )
        let detailSize = detailController.sizeThatFits(
            in: NSSize(width: 360, height: CGFloat.greatestFiniteMagnitude)
        )

        XCTAssertEqual(detailSize.height, overviewSize.height, accuracy: 1)
    }
}
