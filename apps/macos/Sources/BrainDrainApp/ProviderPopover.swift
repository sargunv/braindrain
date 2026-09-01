import AppKit
import BrainDrainBindings
import SwiftUI

struct ProviderPopover: View {
    private static let visibilityRefreshMinimumAge: TimeInterval = 10

    @Bindable var model: ProviderListModel

    var body: some View {
        VStack(spacing: 0) {
            header

            Divider()

            content

            Divider()

            footer
        }
        .frame(width: 360)
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { notification in
            guard let window = notification.object as? NSWindow,
                  window.isVisible
            else {
                return
            }

            Task {
                await model.refreshIfStale(minimumAge: Self.visibilityRefreshMinimumAge)
            }
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            if let provider = model.selectedProvider {
                Button {
                    model.selectedProviderID = nil
                } label: {
                    Image(systemName: "chevron.left")
                }
                .buttonStyle(.borderless)
                .keyboardShortcut(.cancelAction)
                .accessibilityLabel("All providers")
                .help("All providers")

                Text(provider.title)
                    .font(.headline)
                    .lineLimit(1)

                if let plan = provider.snapshot?.identity?.plan, !plan.isEmpty {
                    Text(plan)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            } else {
                Text("BrainDrain")
                    .font(.headline)
            }

            Spacer()

            Button {
                Task {
                    await model.refreshAll()
                }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.borderless)
            .disabled(model.isRefreshing)
            .help("Refresh")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private var content: some View {
        Group {
            if let provider = model.selectedProvider {
                ScrollView {
                    ProviderSection(provider: provider, showsHeader: false)
                }
            } else {
                ProviderOverview(
                    providers: model.providers,
                    onSelect: { providerID in
                        model.selectedProviderID = providerID
                    }
                )
            }
        }
        .frame(height: providerContentHeight)
    }

    private var providerContentHeight: CGFloat {
        guard !model.providers.isEmpty else {
            return 56
        }

        return min(max(CGFloat(model.providers.count) * 66, 260), 480)
    }

    private var footer: some View {
        HStack(spacing: 10) {
            footerStatus
                .frame(width: 120, alignment: .leading)

            Spacer()

            Button("Quit") {
                NSApplication.shared.terminate(nil)
            }
            .buttonStyle(.borderless)
        }
        .font(.caption)
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
    }

    private var footerStatus: some View {
        HStack(spacing: 6) {
            if model.isRefreshing {
                ProgressView()
                    .controlSize(.small)
                Text("Refreshing")
                    .foregroundStyle(.secondary)
            } else if let lastRefresh = model.lastRefresh {
                Text("Updated \(lastRefresh.formatted(date: .omitted, time: .shortened))")
                    .foregroundStyle(.secondary)
            } else {
                Text("Not updated")
                    .foregroundStyle(.secondary)
            }
        }
    }
}

struct ProviderOverview: View {
    let providers: [ProviderViewState]
    let onSelect: (String) -> Void

    var body: some View {
        if providers.isEmpty {
            EmptyProviderListView()
        } else {
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(Array(providers.enumerated()), id: \.element.id) { index, provider in
                        ProviderOverviewRow(provider: provider) {
                            onSelect(provider.id)
                        }

                        if index < providers.count - 1 {
                            Divider()
                                .padding(.leading, 14)
                        }
                    }
                }
            }
        }
    }
}

struct ProviderOverviewRow: View {
    let provider: ProviderViewState
    let action: () -> Void
    @State private var isHovered = false

    private var summary: ProviderOverviewSummary {
        ProviderOverviewSummary(provider: provider)
    }

    var body: some View {
        Button(action: action) {
            VStack(alignment: .leading, spacing: 5) {
                HStack(alignment: .firstTextBaseline, spacing: 7) {
                    Text(provider.title)
                        .fontWeight(.semibold)
                        .lineLimit(1)

                    if let plan = provider.snapshot?.identity?.plan, !plan.isEmpty {
                        Text(plan)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }

                    Spacer(minLength: 8)

                    if let valueText = summary.valueText {
                        Text(valueText)
                            .font(.caption)
                            .monospacedDigit()
                            .foregroundStyle(summary.hasError ? Color.orange : Color.secondary)
                            .lineLimit(1)
                    }

                    Image(systemName: "chevron.right")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }

                Text(summary.detailText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                if let progress = summary.progress {
                    ProgressView(value: progress)
                        .progressViewStyle(.linear)
                        .tint(usageProgressTint(for: progress))
                        .controlSize(.small)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 9)
            .background(isHovered ? Color.primary.opacity(0.055) : Color.clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { isHovered = $0 }
        .accessibilityLabel(provider.title)
        .accessibilityValue(summary.accessibilityValue)
        .accessibilityHint("Shows provider details")
    }
}

struct ProviderOverviewSummary {
    let valueText: String?
    let detailText: String
    let progress: Double?
    let hasError: Bool

    init(provider: ProviderViewState, now: Date = Date()) {
        if provider.errorMessage != nil {
            valueText = "Error"
            detailText = "Couldn’t refresh usage"
            progress = nil
            hasError = true
            return
        }

        guard let usage = provider.snapshot?.usage else {
            valueText = nil
            detailText = "Waiting for usage"
            progress = nil
            hasError = false
            return
        }

        if let window = usage.windows.max(by: { $0.usedPercent < $1.usedPercent }) {
            valueText = Self.percentText(window.usedPercent)
            if let resetsAt = window.resetsAt,
               let resetDate = parseRFC3339(resetsAt)
            {
                detailText = "Highest: \(window.label) · resets \(relativeResetText(for: resetDate, relativeTo: now))"
            } else {
                detailText = "Highest: \(window.label)"
            }
            progress = Self.clampedProgress(window.usedPercent)
            hasError = false
            return
        }

        if let balance = usage.balances.first {
            valueText = "\(balance.remaining.formatted(.number.precision(.fractionLength(0 ... 2)))) \(balance.unit)"
            detailText = balance.label
            progress = nil
            hasError = false
            return
        }

        if !usage.resetCredits.isEmpty {
            let count = usage.resetCredits.count
            valueText = "\(count) \(count == 1 ? "credit" : "credits")"
            detailText = "Quota reset credits"
            progress = nil
            hasError = false
            return
        }

        valueText = nil
        detailText = "No usage reported"
        progress = nil
        hasError = false
    }

    var accessibilityValue: String {
        [valueText, detailText]
            .compactMap(\.self)
            .joined(separator: ", ")
    }

    private static func percentText(_ usedPercent: Double) -> String {
        guard usedPercent.isFinite else {
            return "—"
        }
        return "\(Int(usedPercent.rounded()))%"
    }

    private static func clampedProgress(_ usedPercent: Double) -> Double {
        guard usedPercent.isFinite else {
            return 0
        }
        return min(max(usedPercent / 100, 0), 1)
    }
}

struct ProviderSection: View {
    let provider: ProviderViewState
    var showsHeader = true

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if showsHeader {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(provider.title)
                        .font(.subheadline)
                        .fontWeight(.semibold)

                    Spacer(minLength: 8)

                    if let plan = provider.snapshot?.identity?.plan, !plan.isEmpty {
                        Text(plan)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            if let snapshot = provider.snapshot {
                UsageDetailsView(usage: snapshot.usage)
            } else if let error = provider.errorMessage {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                EmptyUsageView()
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
    }
}

struct UsageDetailsView: View {
    let usage: FfiUsageSnapshot

    var body: some View {
        if usage.windows.isEmpty, usage.balances.isEmpty, usage.resetCredits.isEmpty {
            EmptyUsageView()
        } else {
            VStack(alignment: .leading, spacing: 8) {
                ForEach(usage.windows, id: \.id) { window in
                    QuotaRow(window: window)
                }

                ForEach(usage.balances, id: \.id) { balance in
                    BalanceRow(balance: balance)
                }

                if !usage.resetCredits.isEmpty {
                    VStack(alignment: .leading, spacing: 5) {
                        Text("Quota reset credits")
                            .font(.caption)
                            .fontWeight(.semibold)

                        ForEach(usage.resetCredits, id: \.id) { credit in
                            ResetCreditRow(credit: credit)
                        }
                    }
                }
            }
        }
    }
}

struct QuotaRow: View {
    let window: FfiRateWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(window.label)
                    .lineLimit(1)
                    .truncationMode(.tail)

                Spacer(minLength: 8)

                if let resetDate {
                    Text(relativeResetText(for: resetDate))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .help("Resets \(exactResetText(for: resetDate))")
                }

                Text(percentText)
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            .font(.caption)

            ProgressView(value: progress)
                .progressViewStyle(.linear)
                .tint(usageProgressTint(for: progress))
        }
    }

    private var progress: Double {
        min(max(window.usedPercent / 100, 0), 1)
    }

    private var percentText: String {
        "\(Int(window.usedPercent.rounded()))%"
    }

    private var resetDate: Date? {
        guard let resetsAt = window.resetsAt,
              let date = parseRFC3339(resetsAt)
        else {
            return nil
        }

        return date
    }
}

private func usageProgressTint(for progress: Double) -> Color {
    if progress >= 0.9 {
        return .red
    }
    if progress >= 0.75 {
        return .orange
    }
    return .accentColor
}

struct BalanceRow: View {
    let balance: FfiBalanceSnapshot

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(balance.label)
                .lineLimit(1)
                .truncationMode(.tail)

            Spacer(minLength: 8)

            Text(valueText)
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
        .font(.caption)
    }

    private var valueText: String {
        "\(balance.remaining.formatted(.number.precision(.fractionLength(0 ... 2)))) \(balance.unit)"
    }
}

struct ResetCreditRow: View {
    let credit: FfiResetCreditSnapshot

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(grantedText)
                .foregroundStyle(.secondary)
                .lineLimit(1)

            Spacer(minLength: 8)

            Text(expiresText)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .font(.caption2)
    }

    private var grantedText: String {
        guard let grantedAt = credit.grantedAt,
              let grantedDate = parseRFC3339(grantedAt)
        else {
            return "Granted unknown"
        }

        return "Granted \(compactDateText(for: grantedDate))"
    }

    private var expiresText: String {
        guard let expiresAt = credit.expiresAt,
              let expiresDate = parseRFC3339(expiresAt)
        else {
            return "Expires unknown"
        }

        return "Expires \(compactDateText(for: expiresDate))"
    }
}

struct EmptyUsageView: View {
    var body: some View {
        Text("No usage data")
            .font(.caption)
            .foregroundStyle(.secondary)
    }
}

struct EmptyProviderListView: View {
    var body: some View {
        Text("No providers")
            .font(.caption)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 14)
    }
}

private func parseRFC3339(_ value: String) -> Date? {
    let formatterWithFractionalSeconds = ISO8601DateFormatter()
    formatterWithFractionalSeconds.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    if let date = formatterWithFractionalSeconds.date(from: value) {
        return date
    }

    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime]
    return formatter.date(from: value)
}

private func relativeResetText(for date: Date, relativeTo now: Date = Date()) -> String {
    let interval = date.timeIntervalSince(now)
    if abs(interval) < 60 {
        return interval >= 0 ? "in <1 min" : "now"
    }

    let formatted = compactDurationText(from: interval)
    return interval >= 0 ? "in \(formatted)" : "\(formatted) ago"
}

private func compactDurationText(from interval: TimeInterval) -> String {
    let formatter = DateComponentsFormatter()
    formatter.allowedUnits = [.day, .hour, .minute]
    formatter.unitsStyle = .abbreviated
    formatter.maximumUnitCount = 2
    formatter.zeroFormattingBehavior = .dropAll
    return formatter.string(from: abs(interval)) ?? "now"
}

private func exactResetText(for date: Date) -> String {
    date.formatted(.dateTime.weekday(.abbreviated).month(.abbreviated).day().year().hour().minute())
}

private func compactDateText(for date: Date) -> String {
    date.formatted(.dateTime.month(.abbreviated).day().hour().minute())
}
