pragma Singleton

import QtQuick
import Quickshell.Io
import "MilevoxStatusLogic.js" as StatusLogic

Item {
  id: root

  property string state: "unavailable"
  property string message: "The Milevox daemon is not running."
  property string transcript: ""
  property string partialTranscript: ""
  property real audioLevel: 0
  property var notices: []
  property string delivery: "none"
  property bool settingsAvailable: false
  property bool postProcessingEnabled: false
  property string postProcessingProvider: ""
  property string postProcessingModel: ""
  property var providerOptions: []
  property var modelCatalog: ({})
  property string tokenSource: "none"
  property bool tokenConfigured: false
  property bool receivedInitialStatus: false
  property string pendingToken: ""

  readonly property bool recording: state === "recording"
  readonly property bool busy: state === "transcribing" || state === "refining"
    || state === "canceling"
  readonly property bool available: state !== "unavailable"
  readonly property bool actionRunning: actionProcess.running
  readonly property bool hasWarning: notices.some(function(notice) {
    return notice && (notice.level === "warning" || notice.level === "error")
  }) || delivery === "clipboard_fallback"
    || (state === "idle" && message !== "" && transcript !== "")
  readonly property string noticeText: StatusLogic.noticeText(notices)
  readonly property string displayMessage: noticeText !== "" ? noticeText : message

  signal eventReceived(var event, string previousState, bool initial)
  signal tokenSaved()
  signal tokenSaveFailed(string reason)

  function optionsForProvider(provider) {
    var value = modelCatalog && modelCatalog[provider]
    if (value && value.options) value = value.options
    return Array.isArray(value) ? value : []
  }

  function updateStatus(line) {
    var result = StatusLogic.applyEvent(root, line)
    if (result.kind === "state")
      eventReceived(result.event, result.previousState, result.initial)
  }

  function disconnect(reason) {
    StatusLogic.disconnect(root, reason)
  }

  function run(args) {
    if (actionProcess.running) return false
    actionProcess.command = ["milevox"].concat(args)
    actionProcess.running = true
    return true
  }

  function primaryAction() {
    if (!available) {
      if (actionProcess.running) return false
      actionProcess.command = ["systemctl", "--user", "restart", "milevox.service"]
      actionProcess.running = true
      return true
    }
    if (busy) return run(["record", "cancel"])
    return run(["record", "toggle"])
  }

  function saveToken(token) {
    if (!settingsAvailable || pendingToken !== "" || String(token).trim() === "") return
    pendingToken = String(token)
    tokenProcess.command = ["milevox", "settings", "token", "--provider", postProcessingProvider]
    tokenProcess.running = true
  }

  function removeToken() {
    if (tokenSource === "stored")
      run(["settings", "token", "remove", "--provider", postProcessingProvider])
  }

  function copyTranscript() {
    var command = StatusLogic.copyCommand(transcript)
    return command ? run(command) : false
  }

  Process {
    id: statusProcess
    command: ["milevox", "status", "--follow", "--levels"]
    running: true
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    onExited: {
      root.disconnect("")
      reconnectTimer.restart()
    }
  }

  Process {
    id: actionProcess
    command: []
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: if (String(text || "").trim() !== "") root.message = String(text).trim()
    }
    onExited: if (!statusProcess.running) reconnectTimer.restart()
  }

  Process {
    id: tokenProcess
    command: []
    stdinEnabled: true
    stdout: SplitParser { onRead: function(line) { root.updateStatus(line) } }
    stderr: StdioCollector {
      id: tokenError
      waitForEnd: true
    }
    onStarted: tokenProcess.write(root.pendingToken + "\n")
    onExited: function(exitCode) {
      var reason = String(tokenError.text || "").trim()
      if (exitCode === 0 && reason === "") {
        root.pendingToken = ""
        root.tokenSaved()
      } else {
        root.pendingToken = ""
        root.message = reason || "Could not save the token."
        root.tokenSaveFailed(root.message)
      }
      if (!statusProcess.running) reconnectTimer.restart()
    }
  }

  Timer {
    id: reconnectTimer
    interval: 2000
    onTriggered: if (!statusProcess.running) statusProcess.running = true
  }
}
