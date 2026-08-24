function noticeText(notices) {
  return notices.map(function(notice) {
    if (!notice || !notice.text) return ""
    return String(notice.text)
      + (notice.detail ? "\n" + String(notice.detail) : "")
  }).filter(function(text) { return text !== "" }).join("\n")
}

function copyCommand(transcript) {
  return String(transcript || "") === "" ? null : ["record", "copy"]
}

function shouldShowCompletionOverlay(state, transcript, hasWarning, initial) {
  return String(state || "") === "idle"
    && (String(transcript || "") !== "" || Boolean(hasWarning))
    && !initial
}

function applySettings(target, snapshot) {
  if (!snapshot || !snapshot.post_processing) return
  var settings = snapshot.post_processing
  target.postProcessingEnabled = Boolean(settings.enabled)
  target.postProcessingProvider = String(settings.provider || "")
  target.postProcessingModel = String(settings.model || "")
  target.providerOptions = Array.isArray(settings.provider_options)
    ? settings.provider_options : []
  target.modelCatalog = settings.model_catalog || settings.catalog
    || (!Array.isArray(settings.model_options) && settings.model_options) || ({})
  if (Array.isArray(settings.model_options)) {
    var catalog = Object.assign({}, target.modelCatalog)
    catalog[target.postProcessingProvider] = settings.model_options
    target.modelCatalog = catalog
  }
  target.tokenSource = String(settings.token_source
    || (settings.token_configured ? "stored" : "none"))
  target.tokenConfigured = settings.token_configured !== undefined
    ? Boolean(settings.token_configured) : target.tokenSource !== "none"
  target.settingsAvailable = true
}

function disconnect(target, reason) {
  target.state = "unavailable"
  target.message = reason || "The Milevox daemon is not running."
  target.notices = []
  target.delivery = "none"
  target.partialTranscript = ""
  target.transcript = ""
  target.settingsAvailable = false
  target.audioLevel = 0
  target.receivedInitialStatus = false
}

function applyEvent(target, line) {
  try {
    var event = typeof line === "string"
      ? JSON.parse(String(line || "")) : line
    if (!event) return { kind: "ignored" }
    if (event.type === "level") {
      var level = Number(event.level || 0)
      target.audioLevel = isFinite(level)
        ? Math.max(0, Math.min(1, level)) : 0
      return { kind: "level" }
    }
    if (event.type !== "state") return { kind: "ignored" }

    var result = {
      kind: "state",
      event: event,
      previousState: target.state,
      initial: !target.receivedInitialStatus
    }
    target.receivedInitialStatus = true
    target.state = String(event.state || "unavailable")
    target.message = String(event.message || "")
    target.notices = Array.isArray(event.notices) ? event.notices : []
    target.delivery = String(event.delivery || "none")
    target.partialTranscript = String(event.partial_transcript || "")
    target.transcript = String(event.transcript || "")
    if (target.state !== "recording") target.audioLevel = 0
    applySettings(target, event.settings)
    return result
  } catch (error) {
    disconnect(target, "Milevox returned invalid status data.")
    return { kind: "error" }
  }
}
