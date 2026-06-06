import AppKit
import BrainDrainBindings
import SwiftUI

struct ProviderPopover: View {
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
            Picker("Provider", selection: selectedProviderBinding) {
                ForEach(model.providers) { provider in
                    Text(provider.title)
                        .tag(provider.id as String?)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
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
                if snapshot.usage.windows.isEmpty {
                    EmptyUsageView()
                } else {
                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(snapshot.usage.windows, id: \.id) { window in
                            QuotaRow(window: window)
                        }
                    }
                }
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
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .full
    formatter.dateTimeStyle = .numeric
    return formatter.localizedString(for: date, relativeTo: Date())
}

private func exactResetText(for date: Date) -> String {
    date.formatted(.dateTime.weekday(.abbreviated).month(.abbreviated).day().year().hour().minute())
}
