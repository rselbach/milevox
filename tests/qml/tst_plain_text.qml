import QtQuick
import QtTest

TestCase {
  id: testCase
  name: "DaemonPlainText"

  readonly property string networkProbeUrl:
    "http://127.0.0.1:0/milevox-network-probe.png"
  property var fakeEvent: ({ "message": "" })

  Text {
    id: daemonText
    text: testCase.fakeEvent.message
    textFormat: Text.PlainText
  }

  Text {
    id: comparisonText
    text: "Greendale"
    textFormat: Text.PlainText
  }

  Component {
    id: networkProbeComponent

    Text {
      textFormat: Text.RichText
    }
  }

  function init() {
    fakeEvent = { "message": "" }
  }

  function test_html_is_rendered_literally() {
    fakeEvent = { "message": "<b>Greendale</b>" }
    verify(daemonText.contentWidth > comparisonText.contentWidth)
  }

  function test_control_characters_remain_plain_text() {
    const payload = "Troy\u001b[31m and Abed\u202e"

    fakeEvent = { "message": payload }

    compare(daemonText.textFormat, Text.PlainText)
    compare(daemonText.text, payload)
  }

  function test_image_like_event_makes_no_network_request() {
    failOnWarning(/.*QML Text: (Unknown error|Connection refused|Host not found).*/)

    fakeEvent = {
      "message": "<img src=\"" + networkProbeUrl + "\">"
    }

    compare(daemonText.textFormat, Text.PlainText)
    compare(daemonText.text, fakeEvent.message)
    verify(daemonText.contentWidth > 0)
    wait(250)
  }

  function test_network_probe_detects_rich_text_request() {
    ignoreWarning(/.*QML Text: (Unknown error|Connection refused|Host not found).*/)

    const probe = createTemporaryObject(networkProbeComponent, testCase)
    verify(probe !== null)
    probe.text = "<img src=\"" + networkProbeUrl + "\">"
    wait(1000)
  }
}
