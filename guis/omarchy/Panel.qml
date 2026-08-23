import QtQuick
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root

  moduleName: "io.github.rselbach.milevox"
  ipcTarget: "milevox"

  property string state: "unavailable"
  property string message: "The Milevox daemon is not running."
  property string transcript: ""
  property string partialTranscript: ""
  property bool settingsAvailable: false
  property bool postProcessingEnabled: false
  property string postProcessingProvider: "openrouter"
  property string postProcessingModel: ""
  property bool tokenConfigured: false

  readonly property bool busy: state === "transcribing" || state === "refining"
  readonly property bool recording: state === "recording"
  readonly property bool settingsEnabled: settingsAvailable && !recording
    && !busy && !actionProcess.running && !tokenProcess.running
  readonly property string providerName: postProcessingProvider === "opencode_zen"
    ? "OpenCode Zen" : "OpenRouter"
  readonly property var providerOptions: [
    { value: "openrouter", label: "OpenRouter" },
    { value: "opencode_zen", label: "OpenCode Zen" }
  ]
  readonly property var openrouterModels: [
    { value: "~openai/gpt-mini-latest", label: "OpenAI GPT Mini" },
    { value: "~anthropic/claude-haiku-latest", label: "Anthropic Claude Haiku" },
    { value: "google/gemini-3.1-flash-lite", label: "Google Gemini Flash Lite" },
    { value: "openai/gpt-5.6-luna", label: "OpenAI GPT-5.6 Luna" }
  ]
  readonly property var zenModels: [
    { value: "deepseek-v4-flash", label: "DeepSeek V4 Flash" },
    { value: "minimax-m3", label: "MiniMax M3" },
    { value: "glm-5.2", label: "GLM 5.2" },
    { value: "gpt-5.6-luna", label: "OpenAI GPT-5.6 Luna" }
  ]
  readonly property var modelOptions: postProcessingProvider === "opencode_zen"
    ? zenModels : openrouterModels
  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function updateStatus(line) {
    try {
      var event = JSON.parse(String(line || ""))
      if (event.type !== "state") return
      root.state = String(event.state || "unavailable")
      root.message = String(event.message || "")
      if (event.transcript !== undefined) root.transcript = String(event.transcript)
      if (event.partial_transcript !== undefined)
        root.partialTranscript = String(event.partial_transcript)
      if (event.settings && event.settings.post_processing) {
        var settings = event.settings.post_processing
        root.postProcessingEnabled = Boolean(settings.enabled)
        root.postProcessingProvider = String(settings.provider || "openrouter")
        root.postProcessingModel = String(settings.model || "")
        root.tokenConfigured = Boolean(settings.token_configured)
        root.settingsAvailable = true
      }
    } catch (error) {
      root.state = "unavailable"
      root.message = "Milevox returned invalid status data."
    }
  }

  function runAction(args) {
    if (actionProcess.running) return
    actionProcess.command = ["milevox"].concat(args)
    actionProcess.running = true
  }

  function toggleRecording() {
    if (!busy) runAction(["record", "toggle"])
  }

  function updateSettings(args) {
    if (!settingsEnabled) return
    runAction(["settings", "set"].concat(args))
  }

  function saveToken() {
    if (!settingsEnabled || tokenField.text.trim() === "") return
    tokenProcess.command = [
      "milevox", "settings", "token", "--provider", postProcessingProvider
    ]
    tokenProcess.running = true
  }

  onOpenedChanged: if (!opened) tokenField.text = ""
  onPostProcessingProviderChanged: tokenField.text = ""

  Process {
    id: statusProcess
    command: ["milevox", "status", "--follow"]
    running: true
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    onExited: {
      root.state = "unavailable"
      root.message = "The Milevox daemon is not running."
      reconnectTimer.restart()
    }
  }

  Process {
    id: actionProcess
    command: []
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: if (String(text || "").trim() !== "")
        root.message = String(text).trim()
    }
    onExited: if (!statusProcess.running) reconnectTimer.restart()
  }

  Process {
    id: tokenProcess
    command: []
    stdinEnabled: true
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: if (String(text || "").trim() !== "")
        root.message = String(text).trim()
    }
    onStarted: {
      var token = tokenField.text
      tokenField.text = ""
      tokenProcess.write(token + "\n")
    }
    onExited: if (!statusProcess.running) reconnectTimer.restart()
  }

  Timer {
    id: reconnectTimer
    interval: 2000
    repeat: false
    onTriggered: if (!statusProcess.running) statusProcess.running = true
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.recording ? "󰍬" : (root.busy ? "󰔟" : "󰍭")
    active: root.recording
    dimmed: root.state === "unavailable"
    tooltipText: root.recording ? "Milevox. Right-click to stop dictation"
      : "Milevox. Right-click to start dictation"
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) root.toggleRecording()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    contentWidth: panel.fittedContentWidth(Style.space(350))
    contentHeight: panel.fittedContentHeight(content.implicitHeight, Style.space(640))

    Column {
      id: content
      width: parent.width
      spacing: Style.space(12)

      PanelHero {
        width: parent.width
        title: "Milevox"
        meta: root.state.charAt(0).toUpperCase() + root.state.slice(1)
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      Text {
        visible: root.message !== ""
        width: parent.width
        text: root.message
        color: root.state === "error" || root.state === "unavailable" ? root.urgent : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      Rectangle {
        width: parent.width
        implicitHeight: actionLabel.implicitHeight + Style.space(16)
        radius: Style.cornerRadius
        color: actionMouse.containsMouse ? Style.hoverFillFor(root.foreground, Color.accent) : "transparent"
        border.width: 1
        border.color: root.dim

        Text {
          id: actionLabel
          anchors.centerIn: parent
          text: root.recording ? "Stop dictation" : (root.busy ? "Working…" : "Start dictation")
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
        }

        MouseArea {
          id: actionMouse
          anchors.fill: parent
          enabled: !root.busy
          hoverEnabled: true
          cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
          onClicked: root.toggleRecording()
        }
      }

      Rectangle {
        width: parent.width
        height: Math.max(1, Style.space(1))
        color: Util.alpha(Color.popups.border, 0.7)
      }

      Text {
        width: parent.width
        text: "POST-PROCESSING"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        font.bold: true
        font.letterSpacing: 1.1
      }

      Toggle {
        width: parent.width
        label: "Clean up transcript"
        description: "Apply dictated formatting, corrections, and punctuation."
        checked: root.postProcessingEnabled
        enabled: root.settingsEnabled
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        accent: Color.accent
        fontFamily: root.fontFamily
        onClicked: root.updateSettings([
          "--enabled", root.postProcessingEnabled ? "false" : "true"
        ])
      }

      Dropdown {
        width: parent.width
        label: "Provider"
        value: root.postProcessingProvider
        options: root.providerOptions
        enabled: root.settingsEnabled && root.postProcessingEnabled
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        fontFamily: root.fontFamily
        onChanged: function(value) {
          root.updateSettings(["--provider", value])
        }
      }

      Dropdown {
        width: parent.width
        label: "Model"
        value: root.postProcessingModel
        options: root.modelOptions
        enabled: root.settingsEnabled && root.postProcessingEnabled
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        fontFamily: root.fontFamily
        onChanged: function(value) {
          root.updateSettings(["--model", value])
        }
      }

      Text {
        width: parent.width
        text: "API TOKEN"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        font.bold: true
        font.letterSpacing: 1.1
      }

      Text {
        visible: root.tokenConfigured
        width: parent.width
        text: "A token is configured for " + root.providerName + "."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      Row {
        width: parent.width
        spacing: Style.space(8)

        TextField {
          id: tokenField
          width: parent.width - saveTokenButton.width - parent.spacing
          password: true
          placeholderText: root.tokenConfigured ? "Replace API token" : "Enter API token"
          enabled: root.settingsEnabled
          opacity: enabled ? 1 : 0.5
          foreground: root.foreground
          accent: Color.accent
          font.family: root.fontFamily
          onAccepted: root.saveToken()
          Keys.onEscapePressed: {
            text = ""
            focus = false
          }
        }

        Button {
          id: saveTokenButton
          anchors.verticalCenter: parent.verticalCenter
          text: tokenProcess.running ? "Saving…" : "Save"
          bordered: true
          focusable: true
          enabled: root.settingsEnabled && tokenField.text.trim() !== ""
          opacity: enabled ? 1 : 0.5
          foreground: root.foreground
          accent: Color.accent
          fontFamily: root.fontFamily
          onClicked: root.saveToken()
        }
      }

      Text {
        visible: !root.settingsAvailable
        width: parent.width
        text: "Restart Milevox to load post-processing controls."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      Column {
        visible: root.partialTranscript !== "" || root.transcript !== ""
        width: parent.width
        spacing: Style.space(5)

        Text {
          width: parent.width
          text: root.state === "idle" ? "LATEST TRANSCRIPT" : "LIVE TRANSCRIPT"
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          font.bold: true
          font.letterSpacing: 1.1
        }

        Text {
          width: parent.width
          text: root.partialTranscript !== "" && root.state !== "idle"
            ? root.partialTranscript : root.transcript
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          wrapMode: Text.WordWrap
          maximumLineCount: 4
          elide: Text.ElideRight
        }
      }

      Text {
        width: parent.width
        text: "Left-click the bar icon to open this panel. Right-click it to toggle recording."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }
    }
  }
}
