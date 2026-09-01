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

            VStack(alignment: .leading, spacing: 0) {
                providerPicker

                Divider()

                ScrollView {
                    if let provider = model.selectedProvider {
                        ProviderSection(provider: provider)
                    } else {
                        EmptyProviderListView()
                    }
                }
                .frame(minHeight: model.providers.isEmpty ? 56 : 260, maxHeight: 840)
            }

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
            Text("BrainDrain")
                .font(.headline)

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

    @ViewBuilder
    private var providerPicker: some View {
        if !model.providers.isEmpty {
            ProviderPicker(
                providers: model.providers.map {
                    ProviderPickerOption(id: $0.id, title: $0.title)
                },
                selection: selectedProviderBinding
            )
        }
    }

    private var selectedProviderBinding: Binding<String?> {
        Binding(
            get: { model.selectedProviderID },
            set: { model.selectedProviderID = $0 }
        )
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

struct ProviderPickerOption: Identifiable {
    let id: String
    let title: String
}

struct ProviderPicker: View {
    let providers: [ProviderPickerOption]
    @Binding var selection: String?

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Picker("Provider", selection: $selection) {
                ForEach(providers) { provider in
                    Text(provider.title)
                        .tag(provider.id as String?)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize(horizontal: true, vertical: false)
            .padding(.horizontal, 14)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 10)
    }
}

struct ProviderSection: View {
    let provider: ProviderViewState

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
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
        if usage.windows.isEmpty && usage.balances.isEmpty && usage.resetCredits.isEmpty {
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
                .tint(.primary)
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

private func relativeResetText(for date: Date) -> String {
    let interval = date.timeIntervalSinceNow
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
