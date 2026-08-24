import QtQuick
import QtTest
import "../../guis/omarchy/MilevoxStatusLogic.js" as StatusLogic

TestCase {
  name: "StatusBehavior"

  QtObject {
    id: status
    property string state: "unavailable"
    property string message: ""
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
  }

  function init() {
    StatusLogic.disconnect(status, "")
  }

  function stateEvent(stateName, transcript, partialTranscript) {
    return {
      type: "state",
      state: stateName,
      message: null,
      notices: [],
      delivery: null,
      transcript: transcript,
      partial_transcript: partialTranscript
    }
  }

  function test_snapshots_replace_transcripts() {
    var first = StatusLogic.applyEvent(status,
      stateEvent("recording", "old final", "old partial"))
    compare(first.kind, "state")
    compare(first.initial, true)
    compare(status.transcript, "old final")
    compare(status.partialTranscript, "old partial")

    var second = StatusLogic.applyEvent(status,
      stateEvent("idle", null, null))
    compare(second.initial, false)
    compare(status.transcript, "")
    compare(status.partialTranscript, "")
  }

  function test_disconnect_makes_reconnect_initial() {
    StatusLogic.applyEvent(status, stateEvent("idle", "Troy", null))
    StatusLogic.disconnect(status, "connection lost")
    compare(status.state, "unavailable")
    compare(status.transcript, "")
    compare(status.partialTranscript, "")
    compare(status.receivedInitialStatus, false)

    var reconnected = StatusLogic.applyEvent(status,
      stateEvent("idle", "Abed", null))
    compare(reconnected.initial, true)
  }

  function test_reconnect_does_not_replay_an_old_completion_overlay() {
    StatusLogic.applyEvent(status, stateEvent("recording", null, "Troy"))
    StatusLogic.disconnect(status, "connection lost")

    var reconnected = StatusLogic.applyEvent(status,
      stateEvent("idle", "Troy and Abed", null))
    compare(reconnected.initial, true)
    verify(!StatusLogic.shouldShowCompletionOverlay(
      status.state, status.transcript, false, reconnected.initial))

    var completed = StatusLogic.applyEvent(status,
      stateEvent("idle", "Troy and Abed in the morning", null))
    compare(completed.initial, false)
    verify(StatusLogic.shouldShowCompletionOverlay(
      status.state, status.transcript, false, completed.initial))
  }

  function test_malformed_input_disconnects() {
    StatusLogic.applyEvent(status, stateEvent("idle", "Greendale", null))
    var result = StatusLogic.applyEvent(status, "not json")
    compare(result.kind, "error")
    compare(status.state, "unavailable")
    compare(status.transcript, "")
    compare(status.receivedInitialStatus, false)
  }

  function test_notice_details_are_preserved() {
    compare(StatusLogic.noticeText([{
      text: "Provider failed",
      detail: "HTTP 503 from Greendale"
    }]), "Provider failed\nHTTP 503 from Greendale")
  }

  function test_copy_action_uses_the_daemon_owned_transcript() {
    var transcript = "<b>Troy & Abed</b> — cool, cool cool cool."
    var command = StatusLogic.copyCommand(transcript)

    compare(JSON.stringify(command), JSON.stringify(["record", "copy"]))
    verify(command.indexOf(transcript) === -1)
    compare(StatusLogic.copyCommand(""), null)
  }

  function test_level_event_changes_only_the_meter() {
    StatusLogic.applyEvent(status, stateEvent("recording", "Troy", "Abed"))
    var result = StatusLogic.applyEvent(status, {
      type: "level",
      level: 1.5
    })
    compare(result.kind, "level")
    compare(status.audioLevel, 1)
    compare(status.state, "recording")
    compare(status.transcript, "Troy")
    compare(status.partialTranscript, "Abed")
  }

  function test_completion_payload_survives_loading_to_idle() {
    var loading = stateEvent("loading", "Troy and Abed", null)
    loading.delivery = "clipboard"
    loading.notices = [{
      level: "warning",
      code: "post_processing_fallback",
      text: "Using the original transcript"
    }]
    StatusLogic.applyEvent(status, loading)

    var ready = stateEvent("idle", "Troy and Abed", null)
    ready.delivery = "clipboard"
    ready.notices = loading.notices
    StatusLogic.applyEvent(status, ready)

    compare(status.state, "idle")
    compare(status.transcript, "Troy and Abed")
    compare(status.delivery, "clipboard")
    compare(status.notices.length, 1)
    compare(status.notices[0].code, "post_processing_fallback")
  }

  function test_configuration_snapshot_replaces_settings() {
    var event = stateEvent("idle", null, null)
    event.settings = {
      post_processing: {
        enabled: true,
        provider: "openrouter",
        model: "greendale-model",
        provider_options: [{ value: "openrouter", label: "OpenRouter" }],
        model_catalog: { openrouter: [{ value: "greendale-model" }] },
        token_source: "stored",
        token_configured: true
      }
    }
    StatusLogic.applyEvent(status, event)
    compare(status.settingsAvailable, true)
    compare(status.postProcessingEnabled, true)
    compare(status.postProcessingProvider, "openrouter")
    compare(status.postProcessingModel, "greendale-model")
    compare(status.tokenSource, "stored")
  }
}
