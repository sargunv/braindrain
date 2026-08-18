import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QtControls
import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.plasmoid
import org.kde.plasma.workspace.dbus as DBus

PlasmoidItem {
  id: root

  readonly property string busName: "dev.sargunv.BrainDrain1"
  readonly property string objectPath: "/dev/sargunv/BrainDrain1"
  readonly property string interfaceName: "dev.sargunv.BrainDrain1"
  property var providerStates: ({})
  property var providerOrder: []
  property string selectedProviderId: providerOrder.length > 0 ? providerOrder[0] : ""
  property bool isRefreshing: false
  property string daemonError: ""
  property date lastRefresh
  property bool hasLastRefresh: false

  switchWidth: Plasmoid.formFactor === PlasmaCore.Types.Horizontal ? 1 : Kirigami.Units.gridUnit * 22
  switchHeight: Plasmoid.formFactor === PlasmaCore.Types.Vertical ? 1 : Kirigami.Units.gridUnit * 19
  Plasmoid.icon: "speedometer-symbolic"
  Plasmoid.title: "BrainDrain"
  Plasmoid.status: daemonError.length > 0 ? PlasmaCore.Types.NeedsAttentionStatus : PlasmaCore.Types.ActiveStatus

  compactRepresentation: Kirigami.Icon {
    Layout.minimumWidth: {
      switch (Plasmoid.formFactor) {
      case PlasmaCore.Types.Vertical:
        return 0;
      case PlasmaCore.Types.Horizontal:
        return height;
      default:
        return Kirigami.Units.gridUnit * 3;
      }
    }

    Layout.minimumHeight: {
      switch (Plasmoid.formFactor) {
      case PlasmaCore.Types.Vertical:
        return width;
      case PlasmaCore.Types.Horizontal:
        return 0;
      default:
        return Kirigami.Units.gridUnit * 3;
      }
    }

    source: Plasmoid.icon
    active: compactMouseArea.containsMouse

    PlasmaComponents3.BusyIndicator {
      anchors.centerIn: parent
      width: Kirigami.Units.iconSizes.small
      height: Kirigami.Units.iconSizes.small
      running: root.isRefreshing
      visible: running
    }

    MouseArea {
      id: compactMouseArea
      anchors.fill: parent
      hoverEnabled: true
      onClicked: root.expanded = !root.expanded
    }
  }

  fullRepresentation: Item {
    implicitWidth: Kirigami.Units.gridUnit * 22
    implicitHeight: Math.min(Kirigami.Units.gridUnit * 36, Math.max(Kirigami.Units.gridUnit * 19, popupLayout.implicitHeight))

    ColumnLayout {
      id: popupLayout
      anchors.fill: parent
      spacing: 0

      RowLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing * 2
        spacing: Kirigami.Units.smallSpacing
        visible: root.providerOrder.length > 0

        Repeater {
          model: root.providerOrder

          PlasmaComponents3.Button {
            required property string modelData

            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: 1
            checkable: true
            checked: root.selectedProviderId === modelData
            text: root.providerTitle(modelData)
            onClicked: root.selectedProviderId = modelData
          }
        }
      }

      Kirigami.Separator {
        Layout.fillWidth: true
        visible: root.providerOrder.length > 0
      }

      QtControls.ScrollView {
        id: detailsScrollView
        Layout.fillWidth: true
        Layout.fillHeight: true
        clip: true

        Item {
          width: Math.max(detailsScrollView.availableWidth, providerSection.implicitWidth)
          implicitHeight: providerSection.implicitHeight

          ColumnLayout {
            id: providerSection
            width: parent.width
            spacing: Kirigami.Units.smallSpacing
            anchors.margins: Kirigami.Units.smallSpacing * 2
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top

            RowLayout {
              Layout.fillWidth: true
              spacing: Kirigami.Units.smallSpacing

              PlasmaComponents3.Label {
                text: root.selectedState() ? root.providerTitle(root.selectedState().provider) : "No providers"
                font.weight: Font.DemiBold
                Layout.fillWidth: true
                elide: Text.ElideRight
              }

              PlasmaComponents3.Label {
                visible: root.selectedPlan().length > 0
                text: root.selectedPlan()
                opacity: 0.72
                font.pointSize: Kirigami.Theme.smallFont.pointSize
                elide: Text.ElideRight
              }
            }

            PlasmaComponents3.Label {
              Layout.fillWidth: true
              visible: root.daemonError.length > 0
              text: root.daemonError
              opacity: 0.72
              wrapMode: Text.WordWrap
              font.pointSize: Kirigami.Theme.smallFont.pointSize
            }

            PlasmaComponents3.Label {
              Layout.fillWidth: true
              visible: root.daemonError.length === 0 && root.selectedState() !== null && !!root.selectedState().error
              text: root.selectedState() && root.selectedState().error ? root.selectedState().error : ""
              opacity: 0.72
              wrapMode: Text.WordWrap
              font.pointSize: Kirigami.Theme.smallFont.pointSize
            }

            PlasmaComponents3.Label {
              Layout.fillWidth: true
              visible: root.daemonError.length === 0 && root.providerOrder.length === 0
              text: "No providers loaded"
              opacity: 0.72
              font.pointSize: Kirigami.Theme.smallFont.pointSize
            }

            ColumnLayout {
              Layout.fillWidth: true
              spacing: Kirigami.Units.smallSpacing * 1.5
              visible: root.hasUsage()

              Repeater {
                model: root.selectedUsage().windows || []

                ColumnLayout {
                  required property var modelData

                  Layout.fillWidth: true
                  spacing: Kirigami.Units.smallSpacing / 2

                  RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    PlasmaComponents3.Label {
                      text: modelData.label || modelData.id || "Quota"
                      Layout.fillWidth: true
                      elide: Text.ElideRight
                      font.pointSize: Kirigami.Theme.smallFont.pointSize
                    }

                    PlasmaComponents3.Label {
                      visible: root.resetText(modelData).length > 0
                      text: root.resetText(modelData)
                      opacity: 0.72
                      font.pointSize: Kirigami.Theme.smallFont.pointSize
                    }

                    PlasmaComponents3.Label {
                      text: root.percentText(modelData.used_percent)
                      opacity: 0.72
                      font.family: "monospace"
                      font.pointSize: Kirigami.Theme.smallFont.pointSize
                    }
                  }

                  PlasmaComponents3.ProgressBar {
                    Layout.fillWidth: true
                    from: 0
                    to: 100
                    value: root.clampedPercent(modelData.used_percent)
                  }
                }
              }

              Repeater {
                model: root.selectedUsage().balances || []

                RowLayout {
                  required property var modelData

                  Layout.fillWidth: true
                  Layout.topMargin: Kirigami.Units.smallSpacing / 2
                  spacing: Kirigami.Units.smallSpacing

                  PlasmaComponents3.Label {
                    text: modelData.label || modelData.id || "Balance"
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    font.pointSize: Kirigami.Theme.smallFont.pointSize
                  }

                  PlasmaComponents3.Label {
                    text: root.balanceText(modelData)
                    opacity: 0.72
                    font.family: "monospace"
                    font.pointSize: Kirigami.Theme.smallFont.pointSize
                  }
                }
              }

              ColumnLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing / 2
                visible: (root.selectedUsage().reset_credits || []).length > 0

                PlasmaComponents3.Label {
                  text: "Quota reset credits"
                  font.weight: Font.DemiBold
                  font.pointSize: Kirigami.Theme.smallFont.pointSize
                }

                Repeater {
                  model: root.selectedUsage().reset_credits || []

                  PlasmaComponents3.Label {
                    required property var modelData

                    Layout.fillWidth: true
                    text: root.resetCreditText(modelData)
                    opacity: 0.72
                    wrapMode: Text.WordWrap
                    font.pointSize: Kirigami.Theme.smallFont.pointSize
                  }
                }
              }
            }

            PlasmaComponents3.Label {
              Layout.fillWidth: true
              visible: root.daemonError.length === 0 && root.selectedState() !== null && !root.selectedState().snapshot && !root.selectedState().error
              text: root.selectedState() && root.selectedState().refreshing ? "Refreshing" : "No usage data"
              opacity: 0.72
              font.pointSize: Kirigami.Theme.smallFont.pointSize
            }
          }
        }
      }

      Kirigami.Separator {
        Layout.fillWidth: true
      }

      RowLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing * 2
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.Label {
          text: root.footerText()
          opacity: 0.72
          Layout.fillWidth: true
          elide: Text.ElideRight
          font.pointSize: Kirigami.Theme.smallFont.pointSize
        }

        PlasmaComponents3.BusyIndicator {
          Layout.preferredWidth: Kirigami.Units.iconSizes.small
          Layout.preferredHeight: Kirigami.Units.iconSizes.small
          running: root.isRefreshing
          visible: running
        }

        PlasmaComponents3.ToolButton {
          icon.name: "view-refresh-symbolic"
          text: "Refresh"
          display: QtControls.AbstractButton.IconOnly
          enabled: !root.isRefreshing
          onClicked: root.refreshAll()
          QtControls.ToolTip.text: "Refresh"
          QtControls.ToolTip.visible: hovered
        }
      }
    }
  }

  Component.onCompleted: {
    loadProviders();
    loadStatus();
  }

  DBus.SignalWatcher {
    service: root.busName
    path: root.objectPath
    iface: root.interfaceName

    function dbusProviderRefreshStarted(provider) {
      root.isRefreshing = true
      refreshClearTimer.stop()
    }

    function dbusProviderRefreshFinished(provider, stateJson) {
      root.applyStateJson(provider, stateJson)
      refreshClearTimer.restart()
    }
  }

  Timer {
    id: refreshClearTimer
    interval: 800
    repeat: false
    onTriggered: root.isRefreshing = false
  }

  function callDaemon(method, args, onSuccess) {
    if (!DBus.SessionBus) {
      daemonError = "BrainDrain daemon D-Bus module is unavailable";
      return;
    }

    try {
      const pending = DBus.SessionBus.asyncCall({
        service: busName,
        path: objectPath,
        iface: interfaceName,
        member: method,
        arguments: args || []
      });
      pending.finished.connect(function() {
        if (pending.isError || (pending.error && pending.error.isValid)) {
          const message = pending.error && pending.error.message ? pending.error.message : "D-Bus call failed";
          daemonError = "BrainDrain daemon is not running: " + message;
          isRefreshing = false;
          pending.destroy();
          return;
        }

        daemonError = "";
        onSuccess(replyPayload(pending));
        pending.destroy();
      });
    } catch (error) {
      daemonError = "BrainDrain daemon is not running: " + error;
      isRefreshing = false;
    }
  }

  function replyPayload(reply) {
    if (reply.value !== undefined) {
      return firstValue(reply.value);
    }
    if (reply.values !== undefined) {
      return firstValue(reply.values);
    }
    return reply;
  }

  function firstValue(value) {
    if (Array.isArray(value)) {
      return value.length > 0 ? value[0] : "";
    }
    if (value && typeof value === "object") {
      if (value.value !== undefined) {
        return firstValue(value.value);
      }
      if (value[0] !== undefined) {
        return firstValue(value[0]);
      }
      const keys = Object.keys(value);
      if (keys.length === 1) {
        return firstValue(value[keys[0]]);
      }
    }
    return value;
  }

  function parseJsonPayload(payload, fallback) {
    if (typeof payload !== "string") {
      return payload || fallback;
    }
    try {
      return JSON.parse(payload);
    } catch (error) {
      daemonError = "BrainDrain daemon returned invalid data";
      return fallback;
    }
  }

  function loadProviders() {
    callDaemon("ListProviders", [], function(payload) {
      const providers = Array.isArray(payload) ? payload : [];
      providerOrder = providers;
      if (!selectedProviderId && providers.length > 0) {
        selectedProviderId = providers[0];
      }
    });
  }

  function loadStatus() {
    callDaemon("Status", [], function(payload) {
      const status = parseJsonPayload(payload, null);
      if (status && Array.isArray(status.providers)) {
        applyStates(status.providers);
      }
    });
  }

  function refreshAll() {
    if (isRefreshing) {
      return;
    }
    isRefreshing = true;
    callDaemon("RefreshAll", [], function(payload) {
      const states = parseJsonPayload(payload, []);
      if (Array.isArray(states)) {
        applyStates(states);
      }
      isRefreshing = false;
      lastRefresh = new Date();
      hasLastRefresh = true;
    });
  }

  function applyStateJson(provider, stateJson) {
    let state = null
    try {
      state = JSON.parse(stateJson)
    } catch (error) {
      return
    }
    if (!state || typeof state !== "object" || !provider) {
      return
    }
    state.provider = provider
    const next = {}
    const keys = Object.keys(providerStates)
    for (let i = 0; i < keys.length; i += 1) {
      next[keys[i]] = providerStates[keys[i]]
    }
    next[provider] = state
    providerStates = next
    lastRefresh = new Date()
    hasLastRefresh = true
  }

  function applyStates(states) {
    const next = {};
    for (let i = 0; i < states.length; i += 1) {
      if (states[i] && states[i].provider) {
        next[states[i].provider] = states[i];
      }
    }
    providerStates = next;
    if (providerOrder.length === 0) {
      providerOrder = states.map(function(state) {
        return state.provider;
      });
    }
    if (!selectedProviderId && providerOrder.length > 0) {
      selectedProviderId = providerOrder[0];
    }
    lastRefresh = new Date();
    hasLastRefresh = true;
  }

  function selectedState() {
    return providerStates[selectedProviderId] || null;
  }

  function selectedUsage() {
    const state = selectedState();
    if (!state || !state.snapshot || !state.snapshot.usage) {
      return {
        windows: [],
        balances: [],
        reset_credits: []
      };
    }
    return state.snapshot.usage;
  }

  function selectedPlan() {
    const state = selectedState();
    const identity = state && state.snapshot ? state.snapshot.identity : null;
    return identity && identity.plan ? identity.plan : "";
  }

  function hasUsage() {
    const usage = selectedUsage();
    return (usage.windows || []).length > 0 || (usage.balances || []).length > 0 || (usage.reset_credits || []).length > 0;
  }

  function providerTitle(provider) {
    if (provider === "openai") {
      return "OpenAI";
    }
    if (provider === "claude") {
      return "Claude Code";
    }
    if (provider === "cursor") {
      return "Cursor";
    }
    if (provider === "kimi") {
      return "Kimi Code";
    }
    if (provider === "zai") {
      return "z.ai";
    }
    if (provider === "opencode-go") {
      return "OpenCode Go";
    }
    return provider || "";
  }

  function clampedPercent(value) {
    const number = Number(value || 0);
    return Math.max(0, Math.min(100, number));
  }

  function percentText(value) {
    return Math.round(clampedPercent(value)).toString() + "%";
  }

  function balanceText(balance) {
    const remaining = Number(balance.remaining || 0);
    const unit = balance.unit ? " " + balance.unit : "";
    return remaining.toLocaleString(Qt.locale(), "f", remaining % 1 === 0 ? 0 : 1) + unit;
  }

  function resetText(window) {
    if (!window || !window.resets_at) {
      return "";
    }
    const date = new Date(window.resets_at);
    if (isNaN(date.getTime())) {
      return "";
    }
    return "resets " + relativeTime(date);
  }

  function resetCreditText(credit) {
    const granted = credit.granted_at ? new Date(credit.granted_at) : null;
    const expires = credit.expires_at ? new Date(credit.expires_at) : null;
    if (expires && !isNaN(expires.getTime())) {
      return "Expires " + relativeTime(expires);
    }
    if (granted && !isNaN(granted.getTime())) {
      return "Granted " + relativeTime(granted);
    }
    return credit.id || "Reset credit";
  }

  // Keep in sync with braindrain_core::RelativeTimeStyle::Short.
  function relativeTime(date) {
    const diffSeconds = Math.trunc((date.getTime() - Date.now()) / 1000);
    return formatRelativeSeconds(diffSeconds);
  }

  function formatRelativeSeconds(seconds) {
    if (seconds >= 0) {
      return "in " + formatDurationShort(seconds);
    }
    const ago = Math.abs(seconds);
    if (ago < 60) {
      return "now";
    }
    return formatDurationShort(ago) + " ago";
  }

  function formatDurationShort(seconds) {
    if (seconds < 60) {
      return "<1m";
    }
    const days = Math.floor(seconds / 86400);
    const hours = Math.floor((seconds % 86400) / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    if (days > 0) {
      return hours > 0 ? days + "d " + hours + "h" : days + "d";
    }
    if (hours > 0) {
      return minutes > 0 ? hours + "h " + minutes + "m" : hours + "h";
    }
    return minutes + "m";
  }

  function footerText() {
    if (daemonError.length > 0) {
      return "Daemon offline";
    }
    if (isRefreshing) {
      return "Refreshing";
    }
    if (hasLastRefresh) {
      return "Updated " + lastRefresh.toLocaleTimeString(Qt.locale());
    }
    return "Not updated";
  }
}
