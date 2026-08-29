pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Managing accounts — hide, restore, and (rarely) delete.
//
// REMOVING AN ACCOUNT HAS TWO ANSWERS. An account with history is HIDDEN, never deleted: its
// postings are half of every transaction it took part in, so destroying them would unbalance the
// book permanently. Hiding is reversible and keeps every figure intact. An account with no
// references at all — a typo, a category made and never used — can genuinely go, and loses nothing.
//
// The list shows BOTH open and hidden accounts, which the ordinary sidebar cannot: account.balances
// filters closed ones out, so without this panel hiding would be a one-way door with no way back.
Rectangle {
    id: root

    property var accounts: []
    signal changed

    color: "transparent"

    property int armed: -1

    function removable(a) {
        return a.postings === 0 && a.series === 0 && a.rules === 0
            && a.imports === 0 && a.children === 0;
    }
    // Only ever shown when delete is unavailable, so it explains rather than decorates.
    function why(a) {
        const bits = [];
        if (a.postings > 0) bits.push(a.postings + " txn");
        if (a.series > 0) bits.push(a.series + " recurring");
        if (a.rules > 0) bits.push(a.rules + " rule");
        if (a.imports > 0) bits.push(a.imports + " import");
        if (a.children > 0) bits.push(a.children + " child");
        return bits.join(", ");
    }

    function setClosed(a, closed) {
        root.armed = -1;
        Ledger.write("account.close", { id: a.id, closed: closed }, (r, e) => {
            if (!e) root.changed();
        });
    }
    function remove(a) {
        root.armed = -1;
        Ledger.write("account.delete", { id: a.id }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) root.changed();
        });
    }
    property string note: ""

    Column {
        anchors.fill: parent
        spacing: 3

        Repeater {
            // System accounts (the Conversion:* trading accounts) are the book's own machinery and
            // are not the operator's to remove, so they are not offered at all.
            model: (root.accounts || []).filter(a => !a.system)
            Rectangle {
                id: row
                required property var modelData
                width: parent.width
                height: 26
                radius: 3
                color: hov.containsMouse ? Theme.surfaceRaised : "transparent"
                opacity: row.modelData.closed ? 0.55 : 1.0

                MouseArea { id: hov; anchors.fill: parent; hoverEnabled: true }

                Row {
                    anchors.fill: parent
                    anchors.leftMargin: 4
                    anchors.rightMargin: 4
                    spacing: 4

                    Column {
                        width: parent.width - 92
                        anchors.verticalCenter: parent.verticalCenter
                        spacing: 0
                        Text {
                            width: parent.width
                            elide: Text.ElideRight
                            text: row.modelData.name
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 1
                            font.strikeout: row.modelData.closed
                        }
                        Text {
                            width: parent.width
                            elide: Text.ElideRight
                            text: root.removable(row.modelData)
                                  ? "unused" : root.why(row.modelData)
                            color: root.removable(row.modelData) ? Theme.okGreen : Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 5
                        }
                    }

                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: 44
                        implicitHeight: 22
                        label: row.modelData.closed ? "show" : "hide"
                        onClicked: root.setClosed(row.modelData, !row.modelData.closed)
                    }
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: root.armed === row.modelData.id ? 44 : 22
                        implicitHeight: 22
                        // Disabled rather than hidden: "why can't I delete this" is answered by
                        // the count beside the name, not by an absent control.
                        enabled: root.removable(row.modelData)
                        label: root.armed === row.modelData.id ? "sure?" : "×"
                        primary: root.armed === row.modelData.id
                        onClicked: {
                            if (root.armed === row.modelData.id)
                                root.remove(row.modelData);
                            else
                                root.armed = row.modelData.id;
                        }
                    }
                }
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: root.note.length > 0
                  ? root.note
                  : "Hiding keeps every figure and can be undone. Only an account nothing refers to can be deleted."
            color: root.note.length > 0 ? Theme.red : Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }
    }
}
