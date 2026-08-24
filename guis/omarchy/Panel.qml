import QtQuick
import Quickshell
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "io.github.rselbach.milevox"
  ipcTarget: "milevox"

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property var status: MilevoxStatus
  readonly property bool settingsEnabled: status.settingsAvailable && !status.recording
    && !status.busy && !status.actionRunning
  readonly property string primaryLabel: !status.available ? "Restart"
    : status.recording ? "Stop" : status.busy ? "Cancel" : "Start"
  readonly property string providerName: {
    for (var i = 0; i < status.providerOptions.length; ++i)
      if (status.providerOptions[i].value === status.postProcessingProvider)
        return status.providerOptions[i].label
    return status.postProcessingProvider
  }

  implicitWidth: barButton.implicitWidth
  implicitHeight: barButton.implicitHeight

  function updateSettings(args) {
    if (settingsEnabled) status.run(["settings", "set"].concat(args))
  }

  Connections {
    target: status
    function onTokenSaved() { tokenField.text = "" }
  }

  BarIconButton {
    id: barButton
    anchors.fill: parent
    bar: root.bar
    text: status.recording ? "󰍬" : (status.busy ? "󰔟" : "󰍭")
    active: status.recording || (status.hasWarning && status.state === "idle")
    activeColor: status.hasWarning && status.state === "idle" ? root.urgent : Color.accent
    dimmed: !status.available
    tooltipText: status.hasWarning ? "Milevox: finished with warning"
      : "Milevox. Right-click to " + root.primaryLabel.toLowerCase()
    Accessible.role: Accessible.Button
    Accessible.name: status.recording ? "Milevox, recording"
      : status.busy ? "Milevox, processing" : "Milevox"
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton)
        status.primaryAction()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: barButton
    owner: root
    bar: root.bar
    open: root.opened
    contentWidth: panel.fittedContentWidth(Math.min(Style.space(420), Screen.width - Style.space(32)))
    contentHeight: panel.fittedContentHeight(content.implicitHeight, Screen.height - Style.space(64))
    focusTarget: primaryButton

    Column {
      id: content
      width: parent.width
      spacing: Style.space(12)

      PanelHero {
        width: parent.width
        title: "Milevox"
        meta: status.hasWarning && status.state === "idle" ? "Finished with warning"
          : status.state.charAt(0).toUpperCase() + status.state.slice(1)
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      Text {
        visible: status.displayMessage !== ""
        width: parent.width
        text: status.displayMessage
        textFormat: Text.PlainText
        color: status.state === "error" || !status.available || status.hasWarning ? root.urgent : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      Button {
        id: primaryButton
        width: parent.width
        text: root.primaryLabel
        bordered: true
        focusable: true
        enabled: !status.actionRunning
        foreground: root.foreground
        accent: status.state === "error" ? root.urgent : Color.accent
        fontFamily: root.fontFamily
        Accessible.role: Accessible.Button
        Accessible.name: root.primaryLabel + " Milevox dictation"
        onClicked: status.primaryAction()
        Keys.onEscapePressed: {
          if (status.recording || status.busy) status.run(["record", "cancel"])
          else root.opened = false
        }
      }

      Text {
        visible: !status.available
        width: parent.width
        text: "If restart fails, inspect: journalctl --user -u milevox.service -n 100"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WrapAnywhere
      }

      Rectangle { width: parent.width; height: Math.max(1, Style.space(1)); color: Util.alpha(Color.popups.border, 0.7) }
      Text { width: parent.width; text: "POST-PROCESSING"; color: root.dim; font.family: root.fontFamily; font.pixelSize: Style.font.caption; font.bold: true; font.letterSpacing: 1.1 }

      Text {
        width: parent.width
        text: "Privacy: cleanup sends transcript text to the selected provider. Audio stays local."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      Toggle {
        width: parent.width
        label: "Clean up transcript"
        description: "Apply formatting, corrections, and punctuation."
        checked: status.postProcessingEnabled
        enabled: root.settingsEnabled
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        accent: Color.accent
        fontFamily: root.fontFamily
        Accessible.name: "Clean up transcript"
        Accessible.role: Accessible.CheckBox
        onClicked: root.updateSettings(["--enabled", status.postProcessingEnabled ? "false" : "true"])
      }

      Dropdown {
        width: parent.width
        label: "Provider"
        value: status.postProcessingProvider
        options: status.providerOptions
        enabled: root.settingsEnabled && status.postProcessingEnabled && options.length > 0
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        fontFamily: root.fontFamily
        Accessible.name: "Post-processing provider"
        Accessible.role: Accessible.ComboBox
        onChanged: function(value) { root.updateSettings(["--provider", value]) }
      }

      Dropdown {
        width: parent.width
        label: "Model"
        value: status.postProcessingModel
        options: status.optionsForProvider(status.postProcessingProvider)
        enabled: root.settingsEnabled && status.postProcessingEnabled && options.length > 0
        opacity: enabled ? 1 : 0.5
        foreground: root.foreground
        fontFamily: root.fontFamily
        Accessible.name: "Post-processing model"
        Accessible.role: Accessible.ComboBox
        onChanged: function(value) { root.updateSettings(["--model", value]) }
      }

      Text { width: parent.width; text: "API TOKEN"; color: root.dim; font.family: root.fontFamily; font.pixelSize: Style.font.caption; font.bold: true; font.letterSpacing: 1.1 }
      Text {
        width: parent.width
        visible: status.tokenConfigured
        text: status.tokenSource === "environment"
          ? "Token supplied by the environment. Change it in the service environment; Milevox cannot remove it."
          : "A stored token is configured for " + root.providerName + "."
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
          placeholderText: status.tokenConfigured ? "Replace API token" : "Enter API token"
          enabled: root.settingsEnabled && status.pendingToken === ""
          opacity: enabled ? 1 : 0.5
          foreground: root.foreground
          accent: Color.accent
          font.family: root.fontFamily
          Accessible.name: "API token for " + root.providerName
          Accessible.role: Accessible.EditableText
          onAccepted: status.saveToken(text)
          Keys.onEscapePressed: { text = ""; focus = false }
        }
        Button {
          id: saveTokenButton
          anchors.verticalCenter: parent.verticalCenter
          text: status.pendingToken !== "" ? "Saving…" : "Save"
          bordered: true
          focusable: true
          enabled: root.settingsEnabled && status.pendingToken === "" && tokenField.text.trim() !== ""
          foreground: root.foreground
          accent: Color.accent
          fontFamily: root.fontFamily
          Accessible.role: Accessible.Button
          Accessible.name: "Save API token"
          onClicked: status.saveToken(tokenField.text)
        }
      }

      Button {
        visible: status.tokenSource === "stored"
        text: "Remove stored token"
        bordered: true
        focusable: true
        enabled: root.settingsEnabled
        foreground: root.foreground
        accent: root.urgent
        fontFamily: root.fontFamily
        Accessible.role: Accessible.Button
        Accessible.name: "Remove stored API token"
        onClicked: status.removeToken()
      }

      Column {
        visible: status.partialTranscript !== "" || status.transcript !== ""
        width: parent.width
        spacing: Style.space(5)
        Text { width: parent.width; text: status.state === "idle" ? "LATEST TRANSCRIPT" : "LIVE TRANSCRIPT"; color: root.dim; font.family: root.fontFamily; font.pixelSize: Style.font.caption; font.bold: true }
        Text {
          width: parent.width
          text: status.partialTranscript !== "" && status.state !== "idle" ? status.partialTranscript : status.transcript
          textFormat: Text.PlainText
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          wrapMode: Text.WordWrap
          maximumLineCount: 4
          elide: Text.ElideRight
        }
        Button {
          visible: status.transcript !== ""
          text: "Copy transcript"
          bordered: true
          focusable: true
          enabled: !status.actionRunning
          foreground: root.foreground
          accent: Color.accent
          fontFamily: root.fontFamily
          Accessible.role: Accessible.Button
          Accessible.name: "Copy latest transcript"
          onClicked: status.copyTranscript()
        }
      }

      Text { width: parent.width; text: "Left-click opens this panel; right-click toggles recording. Tab and Shift+Tab move focus; Enter or Space activates buttons. Escape clears a token field, closes while idle, or cancels active work."; color: root.dim; font.family: root.fontFamily; font.pixelSize: Style.font.bodySmall; wrapMode: Text.WordWrap }
    }
  }
}
