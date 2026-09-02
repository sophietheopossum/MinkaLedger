pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Managing accounts — rename, hide, restore, and (rarely) delete.
//
// REMOVING AN ACCOUNT HAS TWO ANSWERS. An account with history is HIDDEN, never deleted: its
// postings are half of every transaction it took part in, so destroying them would unbalance the
// book permanently. Hiding is reversible and keeps every figure intact. An account with no
// references at all — a typo, a category made and never used — can genuinely go, and loses nothing.
//
// RENAMING is safe in a way removal is not: postings, rules, series and imports all reach an account
// by id — posting, series_posting, interest_rule, payment_rule, import_row, import_rule and
// txn_import_key every one of them — so no posting moves and no balance changes.
//
// That was NOT the whole story until recently, and the difference was invisible. The core creates
// three accounts for itself (the FX trading accounts, the opening-balance counterweight, the bucket
// the importer files uncategorised rows into) and used to find them again BY NAME, so a rename could
// break them in either direction — away from the name and the lookup grew a duplicate, onto the name
// and an ordinary account started receiving the core's postings. Both corrupted silently, because
// the book went on balancing either way. src/roles.rs resolves all three by identity now, and the
// core refuses to hand one of those names to an ordinary account. Three refusals come back as a
// message to show: a duplicate name, a system account, and a reserved one.
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
    // The two inline editors are MUTUALLY EXCLUSIVE: a row only grows tall enough for one, so
    // opening either closes the other rather than letting the height rule pick a winner.
    property int editingOpening: -1
    property int editingName: -1

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
        root.editingName = -1;
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

    // Opening either editor CLOSES the other: the row is only tall enough for one, and a
    // height rule with two claimants picks a winner instead of laying both out. Mirrors
    // loadOpening, which does the same in reverse.
    //
    // The field is filled here rather than bound to the name: typing into a TextInput breaks a
    // binding on its text, so a bound field would hand back an abandoned draft the next time the
    // row was opened instead of the name as it now stands.
    function startRename(a, field) {
        root.editingOpening = -1;
        field.text = a.name;
        field.invalid = false;
        root.editingName = a.id;
        root.note = "";
        field.focusInput();
    }

    // Takes the Field, not just its text: a name the core refuses — a duplicate — has to land back
    // on the input that produced it, and `invalid` is how Field marks a rejected value.
    function saveName(a, field) {
        const name = field.text.trim();
        // Refused HERE rather than sent for the core to refuse, so the typing survives: the editor
        // stays open with the text untouched and only the field is marked. (The core would answer
        // bad_params for the same input; this just spares the round trip and the wording.)
        if (name.length === 0) {
            field.invalid = true;
            root.note = "An account needs a name.";
            return;
        }
        Ledger.write("account.rename", { id: a.id, name: name }, (r, e) => {
            root.note = e ? e.message : "";
            field.invalid = !!e;
            // Renaming to the name it already has is a normal success, not an error, so an
            // unchanged field simply closes.
            if (!e) { root.editingName = -1; root.changed(); }
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

    // A LIST, not an unbounded Column, and a footer pinned to the panel instead of trailing the rows.
    //
    // The rows used to be a Column that simply overflowed the panel box, carrying the note — the ONLY
    // surface an `already_exists` / `system_account` / `reserved_name` message has — out of the box
    // with them and underneath the opaque UPCOMING panel below. Opening the rename editor made that
    // worse by 52px, so from ten accounts a refused rename showed the operator nothing but the
    // Field's red border, which says a name was rejected but not which of the three reasons it was.
    ListView {
        id: list
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: footer.top
        anchors.bottomMargin: 3
        clip: true
        spacing: 3

        // System accounts (the Conversion:* trading accounts) are the book's own machinery and
        // are not the operator's to remove OR to rename — entry.rs already refuses postings to
        // them, and this is the same rule about the same accounts. The core refuses both, and
        // filtering them out here is why neither button ever appears on one.
        model: (root.accounts || []).filter(a => !a.system)
        delegate: Rectangle {
            id: row
            required property var modelData
            readonly property bool editorOpen: root.editingOpening === row.modelData.id
                                               || root.editingName === row.modelData.id
            width: ListView.view.width
            // 26 for the row itself plus a 46px Field and its margins. Guessing 58 left
            // the editor overlapping the name it belongs to. Both editors are one Field
            // tall, so one number serves either.
            height: row.editorOpen ? 78 : 26
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

            // Top strip only. The row GROWS when an editor opens beneath it, and filling the
            // expanded height would centre this over the editor instead.
            Row {
                id: strip
                anchors.top: parent.top
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.leftMargin: 4
                anchors.rightMargin: 4
                height: 26
                spacing: 4

                Column {
                    // MEASURED, not a reserved constant: a fourth button made the old
                    // `parent.width - 92` too small and ran the delete button off the edge of
                    // a 260px sidebar. Asking the button group how wide it is also tracks the
                    // £ button disappearing on income/expense and × widening to "sure?".
                    width: strip.width - btns.width - strip.spacing
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

                Row {
                    id: btns
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 4

                    PushButton {
                        implicitWidth: 42
                        implicitHeight: 22
                        label: "name"
                        primary: root.editingName === row.modelData.id
                        onClicked: {
                            if (root.editingName === row.modelData.id)
                                root.editingName = -1;
                            else
                                root.startRename(row.modelData, nameField);
                        }
                    }
                    // Only for the kinds that can hold a balance, same as the create form.
                    PushButton {
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
                        implicitWidth: 44
                        implicitHeight: 22
                        label: row.modelData.closed ? "show" : "hide"
                        onClicked: root.setClosed(row.modelData, !row.modelData.closed)
                    }
                    PushButton {
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

            // The rename editor, below the row it belongs to. One field, so it is anchored to
            // BOTH edges and the field takes whatever the save button leaves — nothing to
            // hand-size against the 260px sidebar.
            Row {
                id: nameEditor
                visible: root.editingName === row.modelData.id
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.leftMargin: 6
                anchors.rightMargin: 6
                anchors.bottom: parent.bottom
                anchors.bottomMargin: 3
                spacing: 6
                Field {
                    id: nameField
                    width: nameEditor.width - 46   // the save button plus this Row's spacing
                    label: "name"
                    placeholder: "Current, Rent, Salary…"
                    onAccepted: root.saveName(row.modelData, nameField)
                }
                PushButton {
                    anchors.verticalCenter: parent.verticalCenter
                    implicitWidth: 40
                    implicitHeight: 22
                    label: "save"
                    primary: true
                    onClicked: root.saveName(row.modelData, nameField)
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

    Column {
        id: footer
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        spacing: 3

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
                  : "Hiding keeps every figure and can be undone. Only an account nothing refers to can be deleted. name relabels an account and its history and balances follow it; £ sets an opening balance, and blank removes it."
            color: root.note.length > 0 ? Theme.red : Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }
    }
}
