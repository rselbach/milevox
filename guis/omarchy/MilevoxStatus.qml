import QtQuick
import Quickshell.Io

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
  readonly property bool available: state !== "unavailable"
  readonly property bool actionRunning: actionProcess.running
  readonly property bool hasWarning: notices.some(function(notice) {
    return notice && (notice.level === "warning" || notice.level === "error")
  }) || delivery === "clipboard_fallback"
    || (state === "idle" && message !== "" && transcript !== "")
  readonly property string noticeText: notices.map(function(notice) {
    return notice && notice.text ? String(notice.text) : ""
  }).filter(function(text) { return text !== "" }).join("\n")
  readonly property string displayMessage: noticeText !== "" ? noticeText : message

  signal eventReceived(var event, string previousState, bool initial)
  signal tokenSaved()
  signal tokenSaveFailed(string reason)

  function optionsForProvider(provider) {
    var value = modelCatalog && modelCatalog[provider]
    if (value && value.options) value = value.options
    return Array.isArray(value) ? value : []
  }

  function applySettings(snapshot) {
    if (!snapshot || !snapshot.post_processing) return
    var settings = snapshot.post_processing
    postProcessingEnabled = Boolean(settings.enabled)
    postProcessingProvider = String(settings.provider || "")
    postProcessingModel = String(settings.model || "")
    providerOptions = Array.isArray(settings.provider_options) ? settings.provider_options : []
    modelCatalog = settings.model_catalog || settings.catalog
      || (!Array.isArray(settings.model_options) && settings.model_options) || ({})
    if (Array.isArray(settings.model_options)) {
      var catalog = Object.assign({}, modelCatalog)
      catalog[postProcessingProvider] = settings.model_options
      modelCatalog = catalog
    }
    tokenSource = String(settings.token_source || (settings.token_configured ? "stored" : "none"))
    tokenConfigured = settings.token_configured !== undefined
      ? Boolean(settings.token_configured) : tokenSource !== "none"
    settingsAvailable = true
  }

  function updateStatus(line) {
    try {
      var event = typeof line === "string" ? JSON.parse(String(line || "")) : line
      if (!event || event.type !== "state") return
      var initial = !receivedInitialStatus
      var previous = state
      receivedInitialStatus = true
      state = String(event.state || "unavailable")
      message = String(event.message || "")
      notices = Array.isArray(event.notices) ? event.notices : []
      delivery = String(event.delivery || "none")
      audioLevel = event.level === undefined ? 0
        : Math.max(0, Math.min(1, Number(event.level)))

      if (state === "recording" && previous !== "recording") {
        partialTranscript = ""
        transcript = ""
      }
      if (event.partial_transcript !== undefined)
        partialTranscript = String(event.partial_transcript || "")
      if (event.transcript !== undefined)
        transcript = String(event.transcript || "")
      if (state === "idle") partialTranscript = ""
      applySettings(event.settings)
      eventReceived(event, previous, initial)
    } catch (error) {
      disconnect("Milevox returned invalid status data.")
    }
  }

  function disconnect(reason) {
    state = "unavailable"
    message = reason || "The Milevox daemon is not running."
    notices = []
    delivery = "none"
    partialTranscript = ""
    settingsAvailable = false
    audioLevel = 0
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

  Process {
    id: statusProcess
    command: ["milevox", "status", "--follow"]
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
