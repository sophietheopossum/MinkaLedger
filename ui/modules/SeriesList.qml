pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// The recurring payments that already exist, what they are called, and where to stop them.
//
// WHY THIS EXISTS AT ALL: until now nothing in the app listed your recurring payments. You could
// create one and see its occurrences in UPCOMING, but never the rules themselves — so a series
// entered without an end date could not be bounded afterwards, and an unbounded rule projects
// into every forecast you ever draw.
//
// Ending is not deleting. until_on stops future occurrences and leaves every real transaction the
// series already generated exactly where it is, which is what you want for a gym membership you
// cancelled in March. There is deliberately no delete: a series a transaction points at cannot go
// without orphaning it.
//
// Renaming is not retrospective either. The description here is what the NEXT projection will be
// called; occurrences already written keep the wording they were written with, because that copy
// is the record of what happened and may already have been reconciled against a statement.
//
// A recurring chain is several rows here, one per leg, marked ⛓ and indented after the first. It
// is bounded and renamed as one, because its legs are one commitment; the core does that, the row
// only says so.
Rectangle {
    id: root

    property var series: []
    property string today: ""
    signal changed

    color: "transparent"
    property int editing: -1     // series whose end date is being set
    property int renaming: -1    // series whose description is being retyped
    property string note: ""

    // The row height, and what the rename editor ADDS to it, PUBLISHED rather than kept private:
    // the caller sizes this panel from them.
    //
    // shell.qml used to restate the 30 in its own formula and know nothing about the 52, which
    // clipped the rename editor outright at ONE series -- the state every new book passes through,
    // and the only state in which the panel appears at all with a single rule. The ListView viewport
    // was 32px while the delegate grew to 52, so the description field's box, its focus border and
    // the bottom of its text line fell outside it, and because contentHeight then exceeded the
    // viewport with nowhere to scroll to, no amount of dragging could bring them back.
    readonly property int rowHeight: 30
    readonly property int namingHeight: 52
    readonly property int extraHeight: root.renaming >= 0
                                       ? root.namingHeight - root.rowHeight : 0

    function reload() {
        Ledger.request("series.list", {}, (r, e) => {
            // Scenario rows are what-ifs and belong to the scenario panel, not here.
            if (!e) root.series = (r || []).filter(x => !x.scenario_id);
        });
    }

    Component.onCompleted: root.reload()
    Connections {
        target: Ledger
        function onRevisionChanged() { root.reload(); }
    }

    function setEnd(id, date) {
        const params = { id: id };
        if (date && date.length === 10)
            params.until_on = date;
        Ledger.write("series.end", params, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) { root.editing = -1; root.reload(); root.changed(); }
        });
    }

    function rename(id, text) {
        const description = text.trim();
        if (description.length === 0) {
            root.note = "a recurring payment needs a description";
            return;
        }
        Ledger.write("series.rename", { id: id, description: description }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) { root.renaming = -1; root.reload(); root.changed(); }
        });
    }

    // The rule as a phrase. The RRULE is shown too, but "monthly on the 1st" is what tells you
    // whether the rule is the one you meant.
    function describe(rrule) {
        const r = (rrule || "").toUpperCase();
        const day = /BYMONTHDAY=(-?\d+)/.exec(r);
        const wd = /BYDAY=([A-Z,]+)/.exec(r);
        if (r.indexOf("FREQ=MONTHLY") >= 0) {
            if (day && day[1] === "-1") return "monthly, last day";
            if (r.indexOf("BYSETPOS=-1") >= 0) return "monthly, last working day";
            if (day) return "monthly on the " + day[1];
            return "monthly";
        }
        if (r.indexOf("FREQ=WEEKLY") >= 0) {
            const every = /INTERVAL=(\d+)/.exec(r);
            const n = every ? parseInt(every[1]) : 1;
            const on = wd ? " on " + wd[1] : "";
            return n === 1 ? "weekly" + on
                 : n === 2 ? "fortnightly" + on
                 : n + "-weekly" + on;
        }
        if (r.indexOf("FREQ=YEARLY") >= 0) return "yearly";
        return rrule;
    }

    Column {
        anchors.fill: parent
        spacing: 3

        Text {
            text: "RECURRING PAYMENTS"
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
        Text {
            visible: root.series.length === 0
            text: "none yet"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }

        ListView {
            width: parent.width
            height: Math.max(0, root.height - 20)
            clip: true
            spacing: 2
            model: root.series
            delegate: Rectangle {
                id: srow
                required property var modelData
                readonly property bool naming: root.renaming === srow.modelData.id
                readonly property bool ending: root.editing === srow.modelData.id
                width: ListView.view.width
                // Grows for the name editor: the description field is full width, so leaving it to
                // overhang a 30px row the way the narrow date field does would put it across the
                // neighbouring rows' text. Both numbers come from root, which is also what the
                // caller sizes the panel by -- stating either one twice is what clipped the editor.
                implicitHeight: srow.naming ? root.namingHeight : root.rowHeight
                radius: 3
                color: shov.containsMouse || srow.ending || srow.naming
                       ? Theme.surfaceRaised : "transparent"
                MouseArea { id: shov; anchors.fill: parent; hoverEnabled: true }

                readonly property bool chained: srow.modelData.chain_id !== null
                                                && srow.modelData.chain_id !== undefined
                readonly property int legs: srow.chained ? srow.modelData.chain_len : 1

                Row {
                    anchors.fill: parent
                    // Later legs sit in from the edge, so the eye reads "belongs to the one above".
                    anchors.leftMargin: srow.chained && srow.modelData.chain_seq > 0 ? 18 : 6
                    anchors.rightMargin: 6
                    spacing: 8

                    Column {
                        // Reserves room for the WIDEST case: the rename button, "change end" and
                        // "clear". Sizing for "set end" alone pushed clear off the right edge as
                        // soon as a series had an end date.
                        width: parent.width - 420
                        visible: !srow.naming
                        anchors.verticalCenter: parent.verticalCenter
                        spacing: 0
                        Text {
                            width: parent.width
                            elide: Text.ElideRight
                            text: srow.modelData.description
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Row {
                            width: parent.width
                            spacing: 4
                            Text {
                                id: legMark
                                visible: srow.chained
                                text: "⛓ " + (srow.modelData.chain_seq + 1) + "/" + srow.legs
                                color: Theme.purple
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Text {
                                width: parent.width - (legMark.visible ? legMark.width + 4 : 0)
                                elide: Text.ElideRight
                                // While a chain's end is being set, the line says what the date
                                // will do, because the field beside it has no room to.
                                text: srow.ending && srow.chained
                                      ? "ends all " + srow.legs + " legs together"
                                      : !srow.chained
                                      ? root.describe(srow.modelData.rrule) + " · from " + srow.modelData.dtstart
                                      : srow.modelData.from_account + " → " + srow.modelData.to_account + " · "
                                        + (srow.modelData.chain_seq === 0
                                           ? root.describe(srow.modelData.rrule) + " · from " + srow.modelData.dtstart
                                           : "same rule")
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                    }
                    // In the name's own place, so it is edited where it is read.
                    Field {
                        id: nameEdit
                        visible: srow.naming
                        width: parent.width - 420
                        anchors.verticalCenter: parent.verticalCenter
                        label: srow.chained ? "description, all " + srow.legs + " legs" : "description"
                        placeholder: "what this payment is"
                        onAccepted: root.rename(srow.modelData.id, nameEdit.text)
                    }

                    Text {
                        width: 74
                        horizontalAlignment: Text.AlignRight
                        anchors.verticalCenter: parent.verticalCenter
                        text: srow.modelData.amount_minor === null ? ""
                              : Money.format(Math.abs(srow.modelData.amount_minor),
                                             srow.modelData.currency)
                                + " " + (srow.modelData.currency || "")
                        color: Theme.textMuted
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 3
                    }

                    // The state that matters: bounded, or running forever.
                    Text {
                        width: 96
                        anchors.verticalCenter: parent.verticalCenter
                        visible: !srow.ending
                        text: srow.modelData.until_on
                              ? "ends " + srow.modelData.until_on : "no end date"
                        color: srow.modelData.until_on ? Theme.text : Theme.warnAmber
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Field {
                        id: endEdit
                        visible: srow.ending
                        width: 116
                        anchors.verticalCenter: parent.verticalCenter
                        label: "last payment"
                        numeric: true
                        placeholder: "YYYY-MM-DD"
                    }

                    // One row, one edit at a time: each editor hides the other's controls so the
                    // buttons on screen always belong to the thing being changed.
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: srow.naming ? 44 : 26
                        implicitHeight: 22
                        visible: !srow.ending
                        label: srow.naming ? "save" : "✎"
                        primary: srow.naming
                        onClicked: {
                            if (srow.naming)
                                root.rename(srow.modelData.id, nameEdit.text);
                            else {
                                root.renaming = srow.modelData.id;
                                nameEdit.text = srow.modelData.description;
                                nameEdit.focusInput();
                            }
                        }
                    }
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: 22
                        implicitHeight: 22
                        visible: srow.naming
                        label: "×"
                        onClicked: root.renaming = -1
                    }

                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitHeight: 22
                        visible: !srow.naming
                        // "set end" only reads right the first time; once a series is bounded
                        // the action is to move the date, and the button should say so.
                        label: srow.ending ? "save"
                             : srow.modelData.until_on ? "change end" : "set end"
                        primary: srow.ending
                        onClicked: {
                            if (srow.ending)
                                root.setEnd(srow.modelData.id, endEdit.text);
                            else {
                                root.editing = srow.modelData.id;
                                endEdit.text = srow.modelData.until_on || root.today;
                            }
                        }
                    }
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitHeight: 22
                        visible: srow.modelData.until_on !== null
                                 && !srow.ending && !srow.naming
                        label: "clear"
                        onClicked: root.setEnd(srow.modelData.id, "")
                    }
                }
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.note.length > 0
            text: root.note
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
    }
}
