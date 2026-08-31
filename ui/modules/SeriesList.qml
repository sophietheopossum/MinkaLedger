pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// The recurring payments that already exist, and where to stop them.
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
Rectangle {
    id: root

    property var series: []
    property string today: ""
    signal changed

    color: "transparent"
    property int editing: -1
    property string note: ""

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
                width: ListView.view.width
                implicitHeight: 30
                radius: 3
                color: shov.containsMouse || root.editing === srow.modelData.id
                       ? Theme.surfaceRaised : "transparent"
                MouseArea { id: shov; anchors.fill: parent; hoverEnabled: true }

                Row {
                    anchors.fill: parent
                    anchors.leftMargin: 6
                    anchors.rightMargin: 6
                    spacing: 8

                    Column {
                        // Reserves room for the WIDEST case: "change end" plus "clear". Sizing
                        // for "set end" alone pushed clear off the right edge as soon as a series
                        // had an end date.
                        width: parent.width - 380
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
                        Text {
                            width: parent.width
                            elide: Text.ElideRight
                            text: root.describe(srow.modelData.rrule)
                                + " · from " + srow.modelData.dtstart
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
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
                        visible: root.editing !== srow.modelData.id
                        text: srow.modelData.until_on
                              ? "ends " + srow.modelData.until_on : "no end date"
                        color: srow.modelData.until_on ? Theme.text : Theme.warnAmber
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Field {
                        id: endEdit
                        visible: root.editing === srow.modelData.id
                        width: 116
                        anchors.verticalCenter: parent.verticalCenter
                        label: "last payment"
                        numeric: true
                        placeholder: "YYYY-MM-DD"
                    }

                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitHeight: 22
                        // "set end" only reads right the first time; once a series is bounded
                        // the action is to move the date, and the button should say so.
                        label: root.editing === srow.modelData.id ? "save"
                             : srow.modelData.until_on ? "change end" : "set end"
                        primary: root.editing === srow.modelData.id
                        onClicked: {
                            if (root.editing === srow.modelData.id)
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
                                 && root.editing !== srow.modelData.id
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
