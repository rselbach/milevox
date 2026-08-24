import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Commons
import qs.Ui
import "MilevoxStatusLogic.js" as StatusLogic

Item {
  id: root
  property real phase: 0
  property bool overlayVisible: false
  property bool fading: false
  readonly property var status: MilevoxStatus
  readonly property bool warning: status.hasWarning && status.state === "idle"
  readonly property color statusColor: status.state === "error" || warning ? Color.urgent
    : status.recording ? Color.accent : Util.alpha(Color.popups.text, 0.55)
  readonly property string statusLabel: status.recording ? "Listening"
    : status.state === "transcribing" ? "Transcribing"
    : status.state === "refining" ? "Refining"
    : status.state === "error" ? "Something went wrong"
    : warning ? "Finished with warning" : "Done"
  readonly property string displayText: status.state === "error" ? status.displayMessage
    : status.state === "idle" ? (warning && status.displayMessage !== "" ? status.displayMessage : status.transcript)
    : status.partialTranscript !== "" ? status.partialTranscript
    : status.recording ? "Speak naturally — Milevox is listening"
    : status.state === "refining" ? "Polishing your transcript…" : "Turning speech into text…"

  function showOverlay() { hideTimer.stop(); closeTimer.stop(); overlayVisible = true; fading = false }
  function hideOverlay(delay) { hideTimer.interval = delay; hideTimer.restart() }
  function beginFade() { if (!overlayVisible) return; fading = true; closeTimer.restart() }

  Connections {
    target: status
    function onEventReceived(event, previousState, initial) {
      if (status.recording || status.busy) root.showOverlay()
      else if (status.state === "error") { root.showOverlay(); root.hideOverlay(4000) }
      else if (StatusLogic.shouldShowCompletionOverlay(
        status.state, status.transcript, status.hasWarning, initial)) {
        root.showOverlay(); root.hideOverlay(status.hasWarning ? 4000 : 1800)
      } else root.beginFade()
    }
    function onStateChanged() { if (!status.available) root.beginFade() }
  }

  Timer { id: hideTimer; onTriggered: root.beginFade() }
  Timer { id: closeTimer; interval: 180; onTriggered: { root.overlayVisible = false; root.fading = false } }
  Timer { interval: 80; repeat: true; running: root.overlayVisible; onTriggered: root.phase += status.recording ? 0.55 : 0.35 }

  PanelWindow {
    visible: root.overlayVisible
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    WlrLayershell.namespace: "milevox-overlay"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.None
    exclusionMode: ExclusionMode.Ignore
    mask: Region {}

    BorderSurface {
      width: Math.min(Style.space(440), parent.width - Style.space(32))
      height: Math.min(parent.height - Style.space(32),
        Math.max(Style.space(112), content.implicitHeight + Style.space(32)))
      anchors.horizontalCenter: parent.horizontalCenter
      anchors.bottom: parent.bottom
      anchors.bottomMargin: Style.space(72)
      color: Util.alpha(Color.popups.background, 0.97)
      borderSpec: Border.surfaceSpec("popups", "border", Color.popups.border, Math.max(1, Style.space(2)))
      radius: Style.cornerRadius
      opacity: root.fading ? 0 : 1
      scale: root.fading ? 0.98 : 1
      Behavior on opacity { NumberAnimation { duration: 180; easing.type: Easing.OutCubic } }
      Behavior on scale { NumberAnimation { duration: 180; easing.type: Easing.OutCubic } }

      Column {
        id: content
        anchors.fill: parent
        anchors.margins: Style.space(16)
        spacing: Style.space(8)
        Row {
          width: parent.width; height: Style.space(18); spacing: Style.space(8)
          Rectangle { width: Style.space(8); height: width; anchors.verticalCenter: parent.verticalCenter; radius: width / 2; color: root.statusColor; opacity: status.recording ? 0.65 + Math.abs(Math.sin(root.phase)) * 0.35 : 1 }
          Text { anchors.verticalCenter: parent.verticalCenter; text: root.statusLabel.toUpperCase(); color: Util.alpha(Color.popups.text, 0.65); font.family: Style.font.family; font.pixelSize: Style.font.caption; font.bold: true; font.letterSpacing: 1.1 }
        }
        Text {
          width: parent.width
          text: root.displayText
          textFormat: Text.PlainText
          color: status.state === "error" || root.warning ? Color.urgent : Color.popups.text
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          wrapMode: Text.WordWrap
          maximumLineCount: status.state === "error" || root.warning ? 12 : 2
          elide: Text.ElideRight
        }
        Item {
          id: waveform; width: parent.width; height: Style.space(18)
          Row {
            anchors.centerIn: parent; spacing: Style.space(4)
            Repeater {
              model: 32
              Rectangle {
                required property int index
                width: Style.space(3)
                height: Math.max(Style.space(2), waveform.height * Math.min(1,
                  status.recording ? status.audioLevel * (0.35 + 0.65 * Math.abs(Math.sin(index * 0.73 + root.phase)))
                  : status.busy ? 0.18 + 0.5 * Math.max(0, Math.sin(index * 0.48 - root.phase)) : 0.08))
                anchors.verticalCenter: parent.verticalCenter; radius: width / 2
                color: status.state === "error" || root.warning ? Color.urgent : status.recording ? Color.accent : Util.alpha(Color.popups.text, 0.5)
                Behavior on height { NumberAnimation { duration: 90; easing.type: Easing.OutCubic } }
              }
            }
          }
        }
      }
    }
  }
}
