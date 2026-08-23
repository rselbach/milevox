import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import qs.Commons
import qs.Ui

Item {
  id: root

  property string state: "unavailable"
  property string message: ""
  property string partialTranscript: ""
  property string transcript: ""
  property real audioLevel: 0
  property real phase: 0
  property bool receivedInitialStatus: false
  property bool overlayVisible: false
  property bool fading: false

  readonly property bool recording: state === "recording"
  readonly property bool busy: state === "transcribing" || state === "refining"
  readonly property color statusColor: state === "error" ? Color.urgent
    : (recording ? Color.accent : Util.alpha(Color.popups.text, 0.55))
  readonly property string statusLabel: recording ? "Listening"
    : (state === "transcribing" ? "Transcribing"
    : (state === "refining" ? "Refining"
    : (state === "error" ? "Something went wrong" : "Done")))
  readonly property string displayText: state === "error" ? message
    : (state === "idle" ? transcript
    : (partialTranscript !== "" ? partialTranscript
    : (recording ? "Speak naturally — Milevox is listening"
    : (state === "refining" ? "Polishing your transcript…"
    : "Turning speech into text…"))))

  function showOverlay() {
    hideTimer.stop()
    closeTimer.stop()
    overlayVisible = true
    fading = false
  }

  function hideOverlay(delay) {
    hideTimer.interval = delay
    hideTimer.restart()
  }

  function beginFade() {
    if (!overlayVisible) return
    fading = true
    closeTimer.restart()
  }

  function updateStatus(line) {
    try {
      var event = JSON.parse(String(line || ""))
      if (event.type !== "state") return

      var initial = !receivedInitialStatus
      var previousState = state
      receivedInitialStatus = true
      state = String(event.state || "unavailable")
      message = String(event.message || "")

      if (state === "recording" && previousState !== "recording") {
        partialTranscript = ""
        transcript = ""
      }
      if (event.partial_transcript !== undefined)
        partialTranscript = String(event.partial_transcript)
      if (event.transcript !== undefined)
        transcript = String(event.transcript)
      audioLevel = event.level === undefined ? 0 : Math.max(0, Math.min(1, Number(event.level)))

      if (recording || busy) {
        showOverlay()
      } else if (state === "error") {
        showOverlay()
        hideOverlay(3200)
      } else if (state === "idle" && transcript !== "" && !initial) {
        showOverlay()
        hideOverlay(1500)
      } else {
        beginFade()
      }
    } catch (error) {
      state = "unavailable"
      beginFade()
    }
  }

  Process {
    id: statusProcess
    command: ["milevox", "status", "--follow"]
    running: true
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    onExited: {
      root.state = "unavailable"
      root.beginFade()
      reconnectTimer.restart()
    }
  }

  Timer {
    id: reconnectTimer
    interval: 2000
    onTriggered: if (!statusProcess.running) statusProcess.running = true
  }

  Timer {
    id: hideTimer
    onTriggered: root.beginFade()
  }

  Timer {
    id: closeTimer
    interval: 180
    onTriggered: {
      root.overlayVisible = false
      root.fading = false
    }
  }

  Timer {
    interval: 80
    repeat: true
    running: root.overlayVisible
    onTriggered: root.phase += root.recording ? 0.55 : 0.35
  }

  PanelWindow {
    id: panel
    visible: root.overlayVisible
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    WlrLayershell.namespace: "milevox-overlay"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.None
    exclusionMode: ExclusionMode.Ignore
    mask: Region {}

    BorderSurface {
      id: card
      width: Style.space(440)
      height: Style.space(112)
      anchors.horizontalCenter: parent.horizontalCenter
      anchors.bottom: parent.bottom
      anchors.bottomMargin: Style.space(72)
      color: Util.alpha(Color.popups.background, 0.97)
      borderSpec: Border.surfaceSpec("popups", "border", Color.popups.border, Math.max(1, Style.space(2)))
      radius: Style.cornerRadius
      opacity: root.fading ? 0 : 1
      scale: root.fading ? 0.98 : 1

      Behavior on opacity {
        NumberAnimation { duration: 180; easing.type: Easing.OutCubic }
      }

      Behavior on scale {
        NumberAnimation { duration: 180; easing.type: Easing.OutCubic }
      }

      Column {
        anchors.fill: parent
        anchors.margins: Style.space(16)
        spacing: Style.space(8)

        Row {
          width: parent.width
          height: Style.space(18)
          spacing: Style.space(8)

          Rectangle {
            width: Style.space(8)
            height: width
            anchors.verticalCenter: parent.verticalCenter
            radius: width / 2
            color: root.statusColor
            opacity: root.recording ? 0.65 + Math.abs(Math.sin(root.phase)) * 0.35 : 1
          }

          Text {
            anchors.verticalCenter: parent.verticalCenter
            text: root.statusLabel.toUpperCase()
            color: Util.alpha(Color.popups.text, 0.65)
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            font.bold: true
            font.letterSpacing: 1.1
          }

          Item { width: 1; height: 1 }
        }

        Text {
          width: parent.width
          height: Style.space(36)
          text: root.displayText
          color: root.state === "error" ? Color.urgent : Color.popups.text
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          wrapMode: Text.WordWrap
          maximumLineCount: 2
          elide: Text.ElideRight
          verticalAlignment: Text.AlignVCenter
        }

        Item {
          id: waveform
          width: parent.width
          height: Style.space(18)

          Row {
            anchors.centerIn: parent
            spacing: Style.space(4)

            Repeater {
              model: 32

              Rectangle {
                required property int index
                width: Style.space(3)
                height: {
                  var shape = 0.35 + 0.65 * Math.abs(Math.sin(index * 0.73 + root.phase))
                  var activity = root.recording ? root.audioLevel * shape
                    : (root.busy ? 0.18 + 0.5 * Math.max(0, Math.sin(index * 0.48 - root.phase)) : 0.08)
                  return Math.max(Style.space(2), waveform.height * Math.min(1, activity))
                }
                anchors.verticalCenter: parent.verticalCenter
                radius: width / 2
                color: root.state === "error" ? Color.urgent
                  : (root.recording ? Color.accent : Util.alpha(Color.popups.text, 0.5))

                Behavior on height {
                  NumberAnimation { duration: 90; easing.type: Easing.OutCubic }
                }
              }
            }
          }
        }
      }
    }
  }
}
