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
    property string today: ""
    signal changed
    signal emptyBookRequested

    color: "transparent"

    property int armed: -1
    // account id -> { txn_id, occurred_on, amount_minor } | null, fetched lazily when a row is
    // opened. Not part of account.list: most rows never need it and it is a query each.
    property var openings: ({})
    property int editingOpening: -1

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

    function loadOpening(a) {
        Ledger.request("account.opening", { id: a.id }, (r, e) => {
            if (e)
                return;
            const next = root.openings;
            next[a.id] = r;
            root.openings = next;
            root.editingOpening = a.id;
        });
    }

    function saveOpening(a, text, date) {
        const commit = (minor) => {
            Ledger.write("account.set_opening",
                         { id: a.id, amount_minor: minor, occurred_on: date }, (r, e) => {
                root.note = e ? e.message : "";
                if (!e) { root.editingOpening = -1; root.changed(); }
            });
        };
        if (text.trim().length === 0) {
            commit(0);   // blank means "no opening balance", which removes the transaction
            return;
        }
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(a.currency) }, (pr, pe) => {
            if (pe) { root.note = pe.message; return; }
            // Same convention as the create form: a liability is stated as what is owed.
            commit(a.kind === "liability" ? -Math.abs(pr.minor) : pr.minor);
        });
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
                // 26 for the row itself plus a 46px Field and its margins. Guessing 58 left
                // the editor overlapping the name it belongs to.
                height: root.editingOpening === row.modelData.id ? 78 : 26
                radius: 3
                color: hov.containsMouse ? Theme.surfaceRaised : "transparent"
                opacity: row.modelData.closed ? 0.55 : 1.0

                MouseArea {
                    id: hov
                    anchors.top: parent.top
                    anchors.left: parent.left
                    anchors.right: parent.right
                    height: 26
                    hoverEnabled: true
                }

                // Top strip only. The row GROWS when the opening editor opens beneath it, and
                // filling the expanded height would centre this over the editor instead.
                Row {
                    anchors.top: parent.top
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.leftMargin: 4
                    anchors.rightMargin: 4
                    height: 26
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

                    // Only for the kinds that can hold a balance, same as the create form.
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: 30
                        implicitHeight: 22
                        visible: row.modelData.kind === "asset"
                                 || row.modelData.kind === "liability"
                        label: "£"
                        primary: root.editingOpening === row.modelData.id
                        onClicked: {
                            if (root.editingOpening === row.modelData.id)
                                root.editingOpening = -1;
                            else
                                root.loadOpening(row.modelData);
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

                // The opening-balance editor, below the row it belongs to. Blank means "none",
                // which removes the transaction rather than leaving a zero one behind.
                Row {
                    visible: root.editingOpening === row.modelData.id
                    anchors.left: parent.left
                    anchors.leftMargin: 6
                    anchors.bottom: parent.bottom
                    anchors.bottomMargin: 3
                    spacing: 6
                    Field {
                        // Sized for a 260px sidebar: amount + date + save must fit on one line,
                        // and the "blank removes it" hint lives in the footer note instead.
                        id: obAmount
                        width: 82
                        numeric: true
                        label: row.modelData.kind === "liability" ? "owed" : "balance"
                        placeholder: "none"
                        text: {
                            const o = root.openings[row.modelData.id];
                            return o ? Money.format(Math.abs(o.amount_minor),
                                                    row.modelData.currency) : "";
                        }
                    }
                    Field {
                        id: obDate
                        width: 104
                        numeric: true
                        label: "as at"
                        placeholder: "YYYY-MM-DD"
                        text: {
                            const o = root.openings[row.modelData.id];
                            return o ? o.occurred_on : root.today;
                        }
                    }
                    PushButton {
                        anchors.verticalCenter: parent.verticalCenter
                        implicitWidth: 40
                        implicitHeight: 22
                        label: "save"
                        primary: true
                        onClicked: root.saveOpening(row.modelData, obAmount.text, obDate.text)
                    }
                }
            }
        }

        // The way to empty the book lives here rather than on the toolbar: reaching it takes a
        // deliberate trip into edit mode, and it only OPENS a screen -- nothing is destroyed by
        // pressing it.
        Text {
            width: parent.width
            horizontalAlignment: Text.AlignRight
            text: "empty this book…"
            color: dangerHover.containsMouse ? Theme.red : Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
            MouseArea {
                id: dangerHover
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.emptyBookRequested()
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: root.note.length > 0
                  ? root.note
                  : "Hiding keeps every figure and can be undone. Only an account nothing refers to can be deleted. £ sets an opening balance; blank removes it."
            color: root.note.length > 0 ? Theme.red : Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }
    }
}
