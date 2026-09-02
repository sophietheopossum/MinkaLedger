pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Browse payments, rename them, and link any of them into a chain.
//
// TWO MODES, ONE PANEL. Browsing and following a thread are the same activity a minute apart: you
// find a payment, then you want to know what it is connected to. Splitting them across two screens
// would mean searching twice.
//
// A LINK IS AN ASSERTION, NOT A CONTAINER. Ticking two payments and pressing Link writes one row
// and changes neither payment. There is no chain to name, no order to declare and no role to pick
// — those are the journey model, which suits a transfer planned in advance. This suits noticing,
// afterwards, that two payments were the same movement.
//
// Direction is recorded (older -> newer) so the thread can be drawn with arrows, but following it
// ignores direction: starting anywhere in a chain shows the whole chain.
Rectangle {
    id: root

    property var accounts: []
    signal done

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property var rows: []
    property int total: 0
    property var picked: []            // txn ids ticked for linking
    property int following: -1         // txn id whose chain is being shown
    property int renaming: -1          // txn id whose description is being retyped
    property var chain: null
    property string note: ""
    property int accountFilter: -1

    // A payment that was renamed OUT of the active search: { id, at, row }.
    //
    // Renaming re-runs the browse, and the payment can then fail the search that found it — you
    // rename "TESCO STORES 4711" to "Weekly shop" while the search box still says "tesco". Letting
    // it drop out would make the only feedback for a successful rename its DISAPPEARANCE, which
    // reads as a delete. So the renamed payment is kept on screen, in the place it already held,
    // marked as outside the search, and it stays there until the search text or the account filter
    // is changed by hand — at which point the filter is honest again because the operator, not the
    // panel, decided what to look at. The alternative, silently clearing the search box, throws
    // away the filter being worked in and reshuffles every other row on screen to explain one.
    //
    // One at a time: renaming a second payment retires the first, because the exemption exists to
    // show you the edit you just made, not to accumulate a private list beside the search.
    property var kept: null

    // What the list actually shows: the core's answer, plus a kept renamed payment put back at the
    // index it held. Whether the row needs keeping is decided by LOOKING at the new answer rather
    // than by re-implementing the core's matching (which spans description and payee) here.
    readonly property var listRows: root.withKept(root.rows, root.kept)

    function withKept(rows, kept) {
        if (!kept)
            return rows;
        for (const r of rows)
            if (r.id === kept.id) return rows;   // still matches; nothing to keep
        const out = rows.slice();
        out.splice(Math.min(kept.at, out.length), 0, kept.row);
        return out;
    }
    function isKept(id) {
        if (!root.kept || root.kept.id !== id)
            return false;
        for (const r of root.rows)
            if (r.id === id) return false;
        return true;
    }
    // Changing what is being looked at retires the exemption: the filter should mean what it says.
    function refilter() {
        root.kept = null;
        root.search();
    }

    Component.onCompleted: if (root.visible) root.search()
    // Reopening the panel is a fresh look at the book, so it retires a kept row too: an amber
    // "not in this search" against an edit made before the panel was closed explains nothing.
    onVisibleChanged: if (root.visible) root.refilter()
    Connections {
        target: Ledger
        function onRevisionChanged() { if (root.visible) root.refreshCurrent(); }
    }

    function refreshCurrent() {
        root.search();
        if (root.following >= 0)
            root.follow(root.following);
    }

    function search() {
        const params = { limit: 60 };
        if (searchField.text.trim().length > 0)
            params.search = searchField.text.trim();
        if (root.accountFilter >= 0)
            params.account_id = root.accountFilter;
        Ledger.request("txn.browse", params, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.rows = r.rows || [];
            root.total = r.total;
            root.note = "";
        });
    }

    // Relabelling a payment, not restating it: the date, the accounts and the amounts are what the
    // money did and are edited elsewhere. Only the words are yours.
    function rename(id, text) {
        const description = text.trim();
        if (description.length === 0) {
            root.note = "a payment needs a description";
            return;
        }
        Ledger.write("txn.rename", { id: id, description: description }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.note = "";
            root.renaming = -1;
            // The core trims before it writes, so what it echoes back is what is stored — the
            // typed string is not necessarily it.
            const at = root.listRows.findIndex(x => x.id === id);
            root.kept = at < 0 ? null
                      : { id: id, at: at,
                          row: Object.assign({}, root.listRows[at],
                                             { description: r.description }) };
            root.refreshCurrent();
        });
    }

    function isPicked(id) { return root.picked.indexOf(id) >= 0; }
    function toggle(id) {
        const next = root.picked.slice();
        const at = next.indexOf(id);
        if (at >= 0) next.splice(at, 1); else next.push(id);
        root.picked = next;
    }

    // Chains the picked payments in date order rather than pick order: the order they were
    // TICKED is an artefact of scrolling, while the order money moved is a fact about them.
    function linkPicked() {
        if (root.picked.length < 2)
            return;
        // listRows, not rows: a payment kept on screen after a rename is tickable like any other,
        // and looking it up here is what keeps it in date order rather than falling back to id.
        const byId = {};
        for (const r of root.listRows) byId[r.id] = r;
        const ordered = root.picked.slice().sort((a, b) => {
            const ra = byId[a], rb = byId[b];
            if (!ra || !rb) return a - b;
            return ra.occurred_on === rb.occurred_on ? a - b
                 : (ra.occurred_on < rb.occurred_on ? -1 : 1);
        });
        let remaining = ordered.length - 1;
        let failed = 0;
        for (let i = 0; i < ordered.length - 1; i++) {
            Ledger.write("link.create",
                         { from_txn: ordered[i], to_txn: ordered[i + 1] }, (r, e) => {
                // "already linked" is not a failure here: chaining A-B-C when A-B exists should
                // add B-C and say nothing about the pair that was already true.
                if (e && e.code !== "already_linked")
                    failed++;
                if (--remaining === 0) {
                    root.note = failed > 0 ? (failed + " link(s) could not be made") : "";
                    root.picked = [];
                    root.refreshCurrent();
                }
            });
        }
    }

    function follow(id) {
        root.following = id;
        Ledger.request("link.chain", { txn_id: id }, (r, e) => {
            root.chain = e ? null : r;
            if (e) root.note = e.message;
        });
    }

    function unlink(a, b) {
        Ledger.write("link.delete", { from_txn: a, to_txn: b }, (r, e) => {
            root.note = e ? e.message : "";
            if (!e) root.refreshCurrent();
        });
    }

    function money(t) {
        // The headline is the biggest leg: for an ordinary payment that is the amount, and for a
        // conversion it is the side that left, which is the one worth showing in a list.
        let best = null;
        for (const p of (t.postings || []))
            if (!best || Math.abs(p.amount_minor) > Math.abs(best.amount_minor)) best = p;
        return best ? Money.format(Math.abs(best.amount_minor), best.currency)
                      + " " + best.currency : "";
    }
    function route(t) {
        const ps = (t.postings || []).slice().sort((a, b) => a.amount_minor - b.amount_minor);
        if (ps.length < 2) return "";
        return ps[0].account + " → " + ps[ps.length - 1].account;
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
                text: "PAYMENTS"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Field {
                id: searchField
                width: 190
                label: "search"
                placeholder: "description or payee"
                onEdited: root.refilter()
            }
            AccountPicker {
                width: 170
                label: "account"
                accounts: root.accounts
                onPicked: id => { root.accountFilter = id; root.refilter(); }
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: root.rows.length + " of " + root.total
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
            }
            // Why the list is one longer than the count beside it.
            Text {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.listRows.length > root.rows.length
                text: "+1 renamed"
                color: Theme.warnAmber
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: root.picked.length < 2
                       ? "tick 2+ to link" : "Link " + root.picked.length
                primary: root.picked.length >= 2
                enabled: root.picked.length >= 2
                onClicked: root.linkPicked()
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "Done"
                onClicked: root.done()
            }
        }

        Row {
            width: parent.width
            height: parent.height - 60
            spacing: 10

            // ---- the ledger ----
            Rectangle {
                width: root.following >= 0 ? parent.width * 0.56 : parent.width
                height: parent.height
                color: Theme.ground
                radius: 5
                border.width: 1
                border.color: Theme.line

                ListView {
                    anchors.fill: parent
                    anchors.margins: 4
                    clip: true
                    model: root.listRows
                    delegate: Rectangle {
                        id: prow
                        required property var modelData
                        readonly property bool held: root.isKept(prow.modelData.id)
                        readonly property bool editing: root.renaming === prow.modelData.id
                        width: ListView.view.width
                        // Grows to carry the rename editor beneath the row it belongs to, so the
                        // payment stays readable while its words are being retyped.
                        height: prow.editing ? 86 : 34
                        radius: 3
                        color: root.isPicked(prow.modelData.id) ? Theme.purpleDim
                             : phov.containsMouse ? Theme.surface : "transparent"
                        border.width: root.following === prow.modelData.id || prow.held ? 1 : 0
                        border.color: root.following === prow.modelData.id
                                      ? Theme.purple : Theme.warnAmber

                        // Top strip only: the row GROWS when the editor opens beneath it, and a
                        // filling hit area would make a click into the editor tick the payment.
                        MouseArea {
                            id: phov
                            anchors.top: parent.top
                            anchors.left: parent.left
                            anchors.right: parent.right
                            height: 34
                            hoverEnabled: true
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.toggle(prow.modelData.id)
                        }

                        Row {
                            anchors.top: parent.top
                            anchors.left: parent.left
                            anchors.right: parent.right
                            anchors.leftMargin: 6
                            anchors.rightMargin: 6
                            height: 34
                            spacing: 8
                            Text {
                                width: 14
                                anchors.verticalCenter: parent.verticalCenter
                                text: root.isPicked(prow.modelData.id) ? "☑" : "☐"
                                color: root.isPicked(prow.modelData.id) ? Theme.purple : Theme.textFaint
                                font.pixelSize: Theme.fontSize
                            }
                            Text {
                                width: 74
                                anchors.verticalCenter: parent.verticalCenter
                                text: prow.modelData.occurred_on
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 3
                            }
                            Column {
                                width: parent.width - 284
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 0
                                Text {
                                    width: parent.width
                                    elide: Text.ElideRight
                                    text: prow.modelData.description
                                    color: Theme.text
                                    font.family: Theme.fontFamily
                                    font.pixelSize: Theme.fontSize - 2
                                }
                                Row {
                                    width: parent.width
                                    spacing: 6
                                    Text {
                                        width: parent.width - (prow.held ? 146 : 0)
                                        elide: Text.ElideRight
                                        text: root.route(prow.modelData)
                                        color: Theme.textFaint
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 4
                                    }
                                    // Says why a row the search no longer matches is still here.
                                    Text {
                                        width: 140
                                        visible: prow.held
                                        elide: Text.ElideRight
                                        text: "· renamed, not in this search"
                                        color: Theme.warnAmber
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 4
                                    }
                                }
                            }
                            Text {
                                width: 84
                                horizontalAlignment: Text.AlignRight
                                anchors.verticalCenter: parent.verticalCenter
                                text: root.money(prow.modelData)
                                color: Theme.text
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                            PushButton {
                                anchors.verticalCenter: parent.verticalCenter
                                implicitWidth: 26
                                implicitHeight: 22
                                label: "✎"
                                primary: prow.editing
                                onClicked: {
                                    if (prow.editing) {
                                        root.renaming = -1;
                                    } else {
                                        root.renaming = prow.modelData.id;
                                        nameEdit.text = prow.modelData.description;
                                        nameEdit.focusInput();
                                    }
                                }
                            }
                            // The chain marker doubles as the way in: a payment that is already
                            // threaded says so, and pressing it follows the thread.
                            Rectangle {
                                width: 34
                                height: 20
                                anchors.verticalCenter: parent.verticalCenter
                                radius: 3
                                color: lhov.containsMouse ? Theme.surfaceRaised : "transparent"
                                Text {
                                    anchors.centerIn: parent
                                    text: prow.modelData.links > 0
                                          ? "⛓ " + prow.modelData.links : "⛓"
                                    color: prow.modelData.links > 0 ? Theme.purple : Theme.textFaint
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 3
                                }
                                MouseArea {
                                    id: lhov
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: root.follow(prow.modelData.id)
                                }
                            }
                        }

                        // The rename editor, below the payment it renames.
                        Row {
                            visible: prow.editing
                            anchors.left: parent.left
                            anchors.leftMargin: 28
                            anchors.right: parent.right
                            anchors.rightMargin: 6
                            anchors.bottom: parent.bottom
                            anchors.bottomMargin: 4
                            spacing: 6
                            Field {
                                id: nameEdit
                                width: parent.width - 100
                                label: "description"
                                placeholder: "what this payment was"
                                onAccepted: root.rename(prow.modelData.id, nameEdit.text)
                            }
                            PushButton {
                                anchors.verticalCenter: parent.verticalCenter
                                implicitWidth: 44
                                implicitHeight: 22
                                label: "save"
                                primary: true
                                onClicked: root.rename(prow.modelData.id, nameEdit.text)
                            }
                            PushButton {
                                anchors.verticalCenter: parent.verticalCenter
                                implicitWidth: 44
                                implicitHeight: 22
                                label: "cancel"
                                onClicked: root.renaming = -1
                            }
                        }
                    }
                }
            }

            // ---- following the thread ----
            Rectangle {
                visible: root.following >= 0
                width: parent.width * 0.44 - 10
                height: parent.height
                color: Theme.ground
                radius: 5
                border.width: 1
                border.color: Theme.purple

                Column {
                    anchors.fill: parent
                    anchors.margins: 8
                    spacing: 4

                    Row {
                        width: parent.width
                        spacing: 6
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: root.chain
                                  ? "THREAD — " + root.chain.nodes.length + " payment"
                                    + (root.chain.nodes.length === 1 ? "" : "s")
                                  : "THREAD"
                            color: Theme.purple
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                        Item { width: parent.width - 190; height: 1 }
                        PushButton {
                            implicitHeight: 22
                            label: "close"
                            onClicked: { root.following = -1; root.chain = null; }
                        }
                    }

                    Text {
                        visible: root.chain !== null && root.chain.nodes.length === 1
                        width: parent.width
                        wrapMode: Text.Wrap
                        text: "Nothing linked to this one yet. Tick it and another payment in the "
                            + "list, then press Link."
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }

                    // Where the thread's money actually ended up. The subtlety worth stating: an
                    // account money passed straight THROUGH nets to zero and is absent, so this
                    // is not a list of everything the chain touched -- it is what is left.
                    Rectangle {
                        visible: root.chain !== null && root.chain.residual !== undefined
                                 && root.chain.residual.length > 0
                        width: parent.width
                        implicitHeight: netCol.implicitHeight + 10
                        radius: 4
                        color: Theme.surfaceRaised
                        border.width: 1
                        border.color: Theme.line
                        Column {
                            id: netCol
                            anchors.left: parent.left
                            anchors.right: parent.right
                            anchors.top: parent.top
                            anchors.margins: 5
                            spacing: 1
                            Text {
                                text: "WHERE IT ENDED UP"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Repeater {
                                model: root.chain ? root.chain.residual : []
                                Row {
                                    id: net
                                    required property var modelData
                                    width: parent.width
                                    spacing: 6
                                    Text {
                                        width: 78
                                        horizontalAlignment: Text.AlignRight
                                        text: Money.format(net.modelData.amount_minor,
                                                           net.modelData.currency)
                                        color: net.modelData.amount_minor < 0
                                               ? Theme.red : Theme.okGreen
                                        font.family: Theme.monoFamily
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                    Text {
                                        text: net.modelData.currency + "  " + net.modelData.account
                                        color: Theme.text
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                }
                            }
                            Text {
                                width: parent.width
                                wrapMode: Text.Wrap
                                text: "an account the money passed straight through nets to zero "
                                    + "and is not listed"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 5
                            }
                        }
                    }

                    // Ordered by date, not by hop count: the thread is a story about money moving
                    // and the reader wants it chronological. `depth` is kept as the marker of how
                    // far each payment sits from the one being followed.
                    Repeater {
                        model: root.chain
                               ? root.chain.nodes.slice().sort((a, b) =>
                                   a.occurred_on === b.occurred_on ? a.id - b.id
                                 : (a.occurred_on < b.occurred_on ? -1 : 1))
                               : []
                        Row {
                            id: node
                            required property var modelData
                            required property int index
                            width: parent.width
                            spacing: 6
                            Text {
                                width: 12
                                text: node.modelData.is_root ? "◉" : "○"
                                color: node.modelData.is_root ? Theme.purple : Theme.textFaint
                                font.pixelSize: Theme.fontSize - 2
                            }
                            Text {
                                width: 66
                                text: node.modelData.occurred_on
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Text {
                                width: parent.width - 160
                                elide: Text.ElideRight
                                text: node.modelData.description
                                color: node.modelData.is_root ? Theme.text : Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                            }
                            Text {
                                width: 62
                                horizontalAlignment: Text.AlignRight
                                text: root.money(node.modelData)
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                    }

                    Text {
                        visible: root.chain !== null && root.chain.edges.length > 0
                        text: "LINKS"
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                    Repeater {
                        model: root.chain ? root.chain.edges : []
                        Row {
                            id: edge
                            required property var modelData
                            spacing: 6
                            Text {
                                text: "#" + edge.modelData.from + " → #" + edge.modelData.to
                                    + (edge.modelData.note ? "  " + edge.modelData.note : "")
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Text {
                                text: "unlink"
                                color: uhov.containsMouse ? Theme.red : Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                                MouseArea {
                                    id: uhov
                                    anchors.fill: parent
                                    anchors.margins: -3
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: root.unlink(edge.modelData.from, edge.modelData.to)
                                }
                            }
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
