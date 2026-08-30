pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Payment chains — requirement 10.
//
// A JOURNEY IS AN ORDERED SET OF REAL TRANSACTIONS, not a new kind of record. Moving money from
// one bank to another through an intermediary is two or three separate transactions days apart;
// each is already correct on its own, and the journey is only the thread that says they are the
// same movement. So attaching and detaching never touches a transaction — it writes one row in
// journey_member and nothing else can go wrong.
//
// THE RESIDUAL IS THE POINT. Sum every posting across the chain, per account: a finished journey
// leaves its intermediate accounts at zero, and whatever is left is money still in flight (or a
// fee nobody recorded). That is the "where is my money right now" answer a list of transactions
// cannot give you, and it is why this is worth having over a tag.
Rectangle {
    id: root

    property string today: ""
    signal done
    signal changed

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property var journeys: []
    property int selected: -1
    property var detail: null
    property var recent: []
    property bool attaching: false
    property string pendingRole: "leg"
    property string note: ""

    Component.onCompleted: if (root.visible) root.reload()
    onVisibleChanged: if (root.visible) root.reload()
    Connections {
        target: Ledger
        function onRevisionChanged() { if (root.visible) root.reload(); }
    }

    function reload() {
        Ledger.request("journey.list", {}, (r, e) => {
            if (!e) root.journeys = r || [];
        });
        if (root.selected >= 0)
            root.open(root.selected);
    }

    function open(id) {
        root.selected = id;
        Ledger.request("journey.get", { id: id }, (r, e) => {
            root.detail = e ? null : r;
            if (e) root.note = e.message;
        });
    }

    function create(label) {
        Ledger.write("journey.create", { label: label, opened_on: root.today }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.note = "";
            newLabel.clear();
            root.reload();
            root.open(r.id);
        });
    }

    // seq is auto-assigned rather than asked for: the operator knows the ORDER money moved in,
    // which is what dragging a row would express, not an integer. A UNIQUE(journey_id, seq)
    // index means a collision is an error rather than a silent reorder, so it must be right.
    function nextSeq() {
        const legs = (root.detail && root.detail.legs) || [];
        let max = -1;
        for (const l of legs)
            if (l.seq > max) max = l.seq;
        return max + 1;
    }

    function loadRecent() {
        Ledger.request("txn.list", { limit: 40 }, (r, e) => {
            if (!e) root.recent = r || [];
        });
    }

    function attach(txn) {
        Ledger.write("journey.attach", {
            journey_id: root.selected, txn_id: txn.id,
            seq: root.nextSeq(), role: root.pendingRole
        }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) { root.attaching = false; root.open(root.selected); root.changed(); }
        });
    }

    function detach(leg) {
        Ledger.write("journey.detach", { journey_id: root.selected, txn_id: leg.txn_id }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) { root.open(root.selected); root.changed(); }
        });
    }

    function close() {
        Ledger.write("journey.close", { id: root.selected, on: root.today }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) { root.reload(); root.open(root.selected); }
        });
    }

    // Completion is the `arrival` ROLE, not an empty residual. The residual sums every posting
    // across the chain, so a finished transfer still shows its source (negative), its destination
    // (positive) and any fee -- it is only the INTERMEDIATE accounts that fall to zero and drop
    // out. Treating "residual empty" as done would never once have been true.
    readonly property bool arrived: {
        if (!root.detail)
            return false;
        for (const l of (root.detail.legs || []))
            if (l.role === "arrival")
                return true;
        return false;
    }

    Row {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 12

        // ---- the chains ----
        Column {
            width: 230
            height: parent.height
            spacing: 6

            Text {
                text: "PAYMENT CHAINS"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Row {
                spacing: 6
                width: parent.width
                Field {
                    id: newLabel
                    width: 140
                    label: "new chain"
                    placeholder: "Wise → Revolut"
                }
                PushButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "+"
                    primary: newLabel.text.length > 0
                    enabled: newLabel.text.length > 0
                    onClicked: root.create(newLabel.text)
                }
            }

            Column {
                width: parent.width
                spacing: 2
                Repeater {
                    model: root.journeys
                    Rectangle {
                        id: jrow
                        required property var modelData
                        width: parent.width
                        height: 34
                        radius: 4
                        color: root.selected === jrow.modelData.id ? Theme.surfaceRaised
                             : jhov.containsMouse ? Theme.ground : "transparent"
                        Column {
                            anchors.left: parent.left
                            anchors.leftMargin: 6
                            anchors.verticalCenter: parent.verticalCenter
                            spacing: 0
                            Text {
                                text: jrow.modelData.label
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                text: jrow.modelData.legs + " step"
                                    + (jrow.modelData.legs === 1 ? "" : "s")
                                    + (jrow.modelData.closed_on
                                       ? " · closed " + jrow.modelData.closed_on : "")
                                color: jrow.modelData.closed_on ? Theme.textFaint : Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                        MouseArea {
                            id: jhov
                            anchors.fill: parent
                            hoverEnabled: true
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.open(jrow.modelData.id)
                        }
                    }
                }
            }
        }

        // ---- the selected chain ----
        Column {
            width: parent.width - 254
            height: parent.height
            spacing: 6

            Row {
                width: parent.width
                spacing: 8
                Text {
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.detail ? root.detail.label : "pick a chain, or make one"
                    color: Theme.text
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize
                }
                PushButton {
                    visible: root.detail !== null && !root.detail.closed_on
                    label: root.attaching ? "Cancel" : "+ step"
                    primary: !root.attaching
                    onClicked: {
                        root.attaching = !root.attaching;
                        if (root.attaching) root.loadRecent();
                    }
                }
                PushButton {
                    visible: root.detail !== null && !root.detail.closed_on
                    label: "Close chain"
                    onClicked: root.close()
                }
                PushButton { label: "Done"; onClicked: root.done() }
            }

            // where the money actually is
            Rectangle {
                visible: root.detail !== null
                width: parent.width
                implicitHeight: resid.implicitHeight + 12
                radius: 5
                color: Theme.surfaceRaised
                border.width: 1
                border.color: root.arrived ? Theme.okGreen : Theme.warnAmber
                Column {
                    id: resid
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.margins: 6
                    spacing: 1
                    Text {
                        text: root.arrived
                              ? "WHERE THIS CHAIN'S MONEY WENT — an arrival step is recorded"
                              : "WHERE THIS CHAIN'S MONEY IS — no arrival step yet"
                        color: root.arrived ? Theme.okGreen : Theme.warnAmber
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Repeater {
                        model: root.detail ? root.detail.residual : []
                        Text {
                            required property var modelData
                            text: Money.format(modelData.amount_minor, modelData.currency)
                                + " " + modelData.currency + "  in  " + modelData.account
                            color: Theme.text
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                    }
                    Text {
                        visible: root.detail !== null && (root.detail.legs || []).length > 0
                        // The genuinely useful reading, and not an obvious one.
                        text: "an account that passed the money straight through nets to zero "
                            + "and is not listed"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                    Text {
                        visible: root.detail !== null && (root.detail.legs || []).length === 0
                        text: "no steps yet"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                }
            }

            // the chain itself
            Column {
                width: parent.width
                spacing: 2
                visible: !root.attaching
                Repeater {
                    model: root.detail ? root.detail.legs : []
                    Row {
                        id: leg
                        required property var modelData
                        required property int index
                        width: parent.width
                        spacing: 8
                        Text {
                            width: 16
                            // The thread, drawn: every step but the last continues downward.
                            text: leg.index === (root.detail.legs.length - 1) ? "└" : "├"
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 1
                        }
                        Text {
                            width: 78
                            text: leg.modelData.occurred_on
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            width: 58
                            text: leg.modelData.role
                            color: leg.modelData.role === "arrival" ? Theme.okGreen
                                 : leg.modelData.role === "fee" ? Theme.warnAmber : Theme.textMuted
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                        Text {
                            width: parent.width - 220
                            elide: Text.ElideRight
                            text: leg.modelData.description
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        PushButton {
                            visible: root.detail !== null && !root.detail.closed_on
                            implicitWidth: 22
                            implicitHeight: 20
                            label: "×"
                            onClicked: root.detach(leg.modelData)
                        }
                    }
                }
            }

            // ---- attach a transaction ----
            Column {
                visible: root.attaching
                width: parent.width
                spacing: 4
                Row {
                    spacing: 6
                    Text {
                        anchors.verticalCenter: parent.verticalCenter
                        text: "as a"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Repeater {
                        model: ["source", "leg", "fee", "arrival"]
                        PushButton {
                            required property var modelData
                            implicitHeight: 24
                            label: modelData
                            primary: root.pendingRole === modelData
                            onClicked: root.pendingRole = modelData
                        }
                    }
                    Text {
                        anchors.verticalCenter: parent.verticalCenter
                        text: "then pick the payment it belongs to"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                }
                Rectangle {
                    width: parent.width
                    height: 150
                    color: Theme.ground
                    radius: 4
                    border.width: 1
                    border.color: Theme.line
                    ListView {
                        anchors.fill: parent
                        anchors.margins: 4
                        clip: true
                        model: root.recent
                        delegate: Rectangle {
                            id: cand
                            required property var modelData
                            width: ListView.view.width
                            height: 20
                            color: chov.containsMouse ? Theme.surfaceRaised : "transparent"
                            Row {
                                anchors.fill: parent
                                anchors.leftMargin: 4
                                spacing: 8
                                Text {
                                    width: 78
                                    anchors.verticalCenter: parent.verticalCenter
                                    text: cand.modelData.occurred_on
                                    color: Theme.textFaint
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 3
                                }
                                Text {
                                    anchors.verticalCenter: parent.verticalCenter
                                    text: cand.modelData.description
                                    color: Theme.text
                                    font.family: Theme.fontFamily
                                    font.pixelSize: Theme.fontSize - 2
                                }
                            }
                            MouseArea {
                                id: chov
                                anchors.fill: parent
                                hoverEnabled: true
                                cursorShape: Qt.PointingHandCursor
                                onClicked: root.attach(cand.modelData)
                            }
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
                font.pixelSize: Theme.fontSize - 2
            }
        }
    }
}
