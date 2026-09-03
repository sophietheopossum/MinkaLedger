pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Scenarios — requirement 8, the "what if" half of the forecast.
//
// A SCENARIO IS A SET OF CHANGES, NOT A SECOND BOOK. Nothing here writes to history. A scenario
// holds series rows of its own, and `forecast.project` is handed the ids of the ones switched on;
// switch them off and the projection is the baseline again, with nothing to undo. That is why
// trying one is cheap, and why deleting one is safe: ON DELETE CASCADE takes its rows and the
// baseline was never touched.
//
// TWO KINDS OF CHANGE, one mechanism. A scenario row with postings ADDS a payment. A scenario row
// that names `supersedes_id` and has NO postings CANCELS a baseline one — the projection skips the
// superseded series entirely. Cancelling is therefore whole-series, not "from a date", and the UI
// says so rather than offering a date field that would do nothing.
Rectangle {
    id: root

    property var scenarios: []
    property var series: []
    property var active: []

    signal toggled(int scenarioId)
    signal addPaymentRequested(int scenarioId, string scenarioName)
    signal changed

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property int selected: -1
    property bool creating: false
    property bool cancelling: false
    property string note: ""
    // Deleting a scenario CASCADES to its series, and the button sits next to the on/off toggle
    // -- the one you press casually. So it arms first: one click asks, the second does it.
    property int armedForDelete: -1

    function reload() {
        Ledger.request("scenario.list", {}, (r, e) => { if (!e) root.scenarios = r || []; });
        Ledger.request("series.list", {}, (r, e) => { if (!e) root.series = r || []; });
    }

    Component.onCompleted: root.reload()
    Connections {
        target: Ledger
        function onRevisionChanged() { root.reload(); }
    }

    // A recurring chain is one commitment in several series; it is listed once, by its first leg.
    // Cancelling that leg cancels the chain -- the projection treats a cancel of any leg that way.
    function firstLeg(s) {
        return s.chain_id === null || s.chain_id === undefined || s.chain_seq === 0;
    }
    function changesIn(scenarioId) {
        return (root.series || []).filter(s => s.scenario_id === scenarioId && root.firstLeg(s));
    }
    // Only baseline series can be cancelled: superseding a scenario row would be a change to a
    // change, which the projection has no notion of.
    function baselineSeries() {
        return (root.series || []).filter(s => (s.scenario_id === null || s.scenario_id === undefined)
                                               && root.firstLeg(s));
    }
    function isActive(id) { return (root.active || []).indexOf(id) >= 0; }

    function create(name) {
        Ledger.write("scenario.create", { name: name }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.creating = false;
            root.selected = r.id;
            root.note = "";
            root.changed();
        });
    }

    function remove(id) {
        root.armedForDelete = -1;
        Ledger.write("scenario.delete", { id: id }, (r, e) => {
            if (e) { root.note = e.message; return; }
            if (root.selected === id) root.selected = -1;
            // Switch it off on the way out, or the forecast keeps an id that no longer exists.
            if (root.isActive(id)) root.toggled(id);
            root.changed();
        });
    }

    function cancelSeries(s) {
        Ledger.write("series.create", {
            description: "cancelled: " + s.description,
            rrule: s.rrule,
            dtstart: s.dtstart,
            scenario_id: root.selected,
            supersedes_id: s.id,
            postings: []          // no postings: this row moves no money, it only suppresses
        }, (r, e) => {
            root.note = e ? e.message : "";
            root.cancelling = false;
            if (!e) root.changed();
        });
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Row {
            width: parent.width
            spacing: 8
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "SCENARIOS"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "switch one on and the forecast changes; nothing is written"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 3
            }
            PushButton {
                label: root.creating ? "Cancel" : "+ scenario"
                primary: !root.creating && root.scenarios.length === 0
                onClicked: { root.creating = !root.creating; root.cancelling = false; }
            }
        }

        Row {
            visible: root.creating
            width: parent.width
            spacing: 6
            Field {
                id: nameField
                width: 260
                label: "name"
                placeholder: "Cancel Netflix, Move flat…"
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "Create"
                primary: true
                enabled: nameField.text.length > 0
                onClicked: { root.create(nameField.text); nameField.clear(); }
            }
        }

        // ---- the scenarios ----
        Column {
            width: parent.width
            spacing: 4
            Repeater {
                model: root.scenarios
                Rectangle {
                    id: scRow
                    required property var modelData
                    width: parent.width
                    implicitHeight: body.implicitHeight + 12
                    radius: 5
                    color: root.selected === scRow.modelData.id ? Theme.surfaceRaised : "transparent"
                    border.width: 1
                    border.color: root.isActive(scRow.modelData.id) ? Theme.purple : Theme.line

                    MouseArea {
                        anchors.fill: parent
                        cursorShape: Qt.PointingHandCursor
                        onClicked: {
                            root.armedForDelete = -1;
                            root.selected = root.selected === scRow.modelData.id
                                            ? -1 : scRow.modelData.id;
                        }
                    }

                    Column {
                        id: body
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.top: parent.top
                        anchors.margins: 6
                        spacing: 3

                        Row {
                            width: parent.width
                            spacing: 8
                            Text {
                                anchors.verticalCenter: parent.verticalCenter
                                text: scRow.modelData.name
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize
                            }
                            Text {
                                anchors.verticalCenter: parent.verticalCenter
                                // An empty scenario switched on looks identical to one switched
                                // off, so the count is the only thing that distinguishes them.
                                text: scRow.modelData.series_count === 0
                                      ? "no changes yet — it will do nothing"
                                      : scRow.modelData.series_count + " change"
                                        + (scRow.modelData.series_count === 1 ? "" : "s")
                                color: scRow.modelData.series_count === 0
                                       ? Theme.warnAmber : Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                            }
                            Item { width: 1; height: 1 }
                        }

                        Repeater {
                            model: root.changesIn(scRow.modelData.id)
                            Row {
                                id: chg
                                required property var modelData
                                spacing: 6
                                Text {
                                    width: 14
                                    text: chg.modelData.supersedes_id ? "−" : "+"
                                    color: chg.modelData.supersedes_id ? Theme.red : Theme.okGreen
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 2
                                }
                                Text {
                                    text: chg.modelData.supersedes_id
                                          ? "cancels " + chg.modelData.supersedes
                                          : chg.modelData.description
                                    color: Theme.textMuted
                                    font.family: Theme.fontFamily
                                    font.pixelSize: Theme.fontSize - 3
                                }
                                Text {
                                    visible: chg.modelData.amount_minor !== null
                                    text: chg.modelData.amount_minor === null
                                          ? "" : Money.format(chg.modelData.amount_minor,
                                                                    chg.modelData.currency)
                                    color: Theme.textFaint
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 3
                                }
                            }
                        }
                    }

                    Row {
                        anchors.right: parent.right
                        anchors.top: parent.top
                        anchors.margins: 5
                        spacing: 5
                        PushButton {
                            label: root.isActive(scRow.modelData.id) ? "on" : "off"
                            primary: root.isActive(scRow.modelData.id)
                            onClicked: { root.armedForDelete = -1;
                                         root.toggled(scRow.modelData.id); }
                        }
                        PushButton {
                            label: root.armedForDelete === scRow.modelData.id ? "delete?" : "×"
                            primary: root.armedForDelete === scRow.modelData.id
                            onClicked: {
                                if (root.armedForDelete === scRow.modelData.id)
                                    root.remove(scRow.modelData.id);
                                else
                                    root.armedForDelete = scRow.modelData.id;
                            }
                        }
                    }
                }
            }
        }

        // ---- acting on the selected scenario ----
        Row {
            visible: root.selected >= 0
            spacing: 8
            PushButton {
                label: "Add a payment"
                primary: true
                onClicked: {
                    root.cancelling = false;
                    const sc = root.scenarios.find(s => s.id === root.selected);
                    root.addPaymentRequested(root.selected, sc ? sc.name : "");
                }
            }
            PushButton {
                label: root.cancelling ? "Close" : "Cancel a payment"
                onClicked: root.cancelling = !root.cancelling
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "changes apply only while this scenario is on"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 3
            }
        }

        // ---- pick a baseline series to cancel ----
        Column {
            visible: root.cancelling && root.selected >= 0
            width: parent.width
            spacing: 3
            Text {
                // Suppression is per SERIES, not per date: forecast() skips the superseded series
                // wherever the scenario is on. Offering a date here would be a field that does
                // nothing, so it says the truth instead.
                text: "which recurring payment goes away entirely?"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 3
            }
            Repeater {
                model: root.baselineSeries()
                Rectangle {
                    id: pick
                    required property var modelData
                    width: parent.width
                    height: 22
                    radius: 3
                    color: pickHover.containsMouse ? Theme.surfaceRaised : "transparent"
                    Row {
                        anchors.fill: parent
                        anchors.leftMargin: 6
                        spacing: 8
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: pick.modelData.description
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: pick.modelData.amount_minor === null
                                  ? "" : Money.format(pick.modelData.amount_minor,
                                                            pick.modelData.currency)
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                    }
                    MouseArea {
                        id: pickHover
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.cancelSeries(pick.modelData)
                    }
                }
            }
            Text {
                visible: root.baselineSeries().length === 0
                text: "no recurring payments to cancel yet"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 3
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.note.length > 0
            text: root.note
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
    }
}
