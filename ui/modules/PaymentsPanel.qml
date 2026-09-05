pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Browse payments, correct them, and link any of them into a chain.
//
// TWO MODES, ONE PANEL. Browsing and following a thread are the same activity a minute apart: you
// find a payment, then you want to know what it is connected to. Splitting them across two screens
// would mean searching twice. Editing is the same again: you find the payment that is wrong, and
// the editor opens beside the list in the place the thread would, so the list is never lost.
//
// THE EDITOR IS THE ENTRY FORM'S SHAPE, NOT THE WHOLE RECORD. Date, description, from, to and
// amount, with the arriving amount when the two accounts hold different currencies -- the same
// things the form asks for, so a payment reads the same going in and being corrected. A payment
// with more than two legs of its own (a split written by the core or an agent) is not that shape,
// and the editor says so and offers the date and description only rather than a form that would
// have to invent a way of showing legs it cannot edit. The core does the rest: txn.update keeps
// the id, so the links, the import key and the series slot that hang off it survive the edit.
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
    property int editing: -1           // txn id open in the editor
    property var chain: null
    property string note: ""
    property int accountFilter: -1

    // One side pane at a time: the thread and the editor share the space beside the list.
    readonly property bool sideOpen: root.following >= 0 || root.editing >= 0

    // A payment that was edited OUT of the active search: { id, at, row }.
    //
    // An edit re-runs the browse, and the payment can then fail the search that found it — you
    // rename "TESCO STORES 4711" to "Weekly shop" while the search box still says "tesco", or move
    // it off the account the list is filtered to. Letting it drop out would make the only feedback
    // for a successful edit its DISAPPEARANCE, which reads as a delete. So the edited payment is
    // kept on screen, in the place it already held, marked as outside the search, and it stays
    // there until the search text or the account filter is changed by hand — at which point the
    // filter is honest again because the operator, not the panel, decided what to look at. The
    // alternative, silently clearing the search box, throws away the filter being worked in and
    // reshuffles every other row on screen to explain one.
    //
    // One at a time: editing a second payment retires the first, because the exemption exists to
    // show you the edit you just made, not to accumulate a private list beside the search.
    property var kept: null

    // What the list actually shows: the core's answer, plus a kept edited payment put back at the
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

    // ---- the editor's state ----
    // Loaded from the row when the editor opens; the fields below are bound to nothing else, so a
    // payment picked while another is half-edited simply replaces it.
    property int editFrom: -1
    property int editTo: -1
    property int editAmountMinor: 0
    property bool editAmountOk: false
    property int editToMinor: 0
    property bool editToOk: false
    // Whether the payment is the form's shape -- one leg out, one leg in, plus a conversion's own
    // legs if it has them -- and so gets the amount and account editors, or only the words.
    property bool editSimple: true
    property int editLegs: 0
    property string editNote: ""

    function currencyOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.currency : "";
    }
    readonly property string editFromCurrency: root.currencyOf(root.editFrom)
    readonly property string editToCurrency: root.currencyOf(root.editTo)
    // Two currencies make it a conversion, whatever it was before: the core builds the legs, as
    // the entry form has txn.convert build them for a new one.
    readonly property bool editCross: root.editSimple
                                      && root.editFromCurrency.length > 0
                                      && root.editToCurrency.length > 0
                                      && root.editFromCurrency !== root.editToCurrency
    readonly property bool editComplete: root.editing >= 0
                                         && editDate.text.length === 10
                                         && editDesc.text.trim().length > 0
                                         && (!root.editSimple
                                             || (root.editAmountOk && root.editFrom >= 0 && root.editTo >= 0
                                                 && root.editFrom !== root.editTo
                                                 && (!root.editCross || root.editToOk)))
    // Shown, never stored: the two real amounts are the rate.
    readonly property string editRate: {
        if (!root.editCross || !root.editAmountOk || !root.editToOk)
            return "";
        const f = root.editAmountMinor / Math.pow(10, Money.digits(root.editFromCurrency));
        const t = root.editToMinor / Math.pow(10, Money.digits(root.editToCurrency));
        return f === 0 ? "" : "1 " + root.editFromCurrency + " = " + (t / f).toFixed(4) + " " + root.editToCurrency;
    }

    // Open a payment in the editor, loaded from the row the list already holds: summarise carries
    // the account ids for exactly this, so there is no round trip before the fields fill.
    function edit(row) {
        root.following = -1;
        root.chain = null;
        root.editing = row.id;
        root.editNote = "";
        editDate.text = row.occurred_on;
        editDesc.text = row.description;
        // A conversion's own legs sit in the conversion accounts; the real ones are the rest.
        const real = (row.postings || []).filter(p => p.kind !== "conversion");
        const from = real.find(p => p.amount_minor < 0);
        const to = real.find(p => p.amount_minor > 0);
        root.editLegs = real.length;
        root.editSimple = real.length === 2 && from !== undefined && to !== undefined;
        root.editFrom = root.editSimple ? from.account_id : -1;
        root.editTo = root.editSimple ? to.account_id : -1;
        fromEdit.selected = root.editFrom;
        toEdit.selected = root.editTo;
        root.editAmountMinor = root.editSimple ? -from.amount_minor : 0;
        root.editAmountOk = root.editSimple;
        root.editToMinor = root.editSimple ? to.amount_minor : 0;
        root.editToOk = root.editSimple;
        amountEdit.text = root.editSimple ? Money.format(-from.amount_minor, from.currency) : "";
        arrivesEdit.text = root.editSimple ? Money.format(to.amount_minor, to.currency) : "";
        amountEdit.invalid = false;
        arrivesEdit.invalid = false;
    }
    function closeEditor() {
        root.editing = -1;
        root.editNote = "";
    }

    // The amount means what the core says it means, at the FROM account's scale -- the same rule
    // and the same call as the entry form, so an edit cannot accept what a new payment refuses.
    function validateEditAmount(text) {
        if (text.trim().length === 0) {
            root.editAmountOk = false;
            amountEdit.invalid = false;
            return;
        }
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.editFromCurrency) }, (r, e) => {
            if (amountEdit.text !== text)
                return; // she kept typing; a later answer is on its way
            if (e) {
                root.editAmountOk = false;
                amountEdit.invalid = true;
                root.editNote = e.message;
            } else {
                root.editAmountMinor = r.minor;
                root.editAmountOk = r.minor > 0;
                amountEdit.invalid = r.minor <= 0;
                root.editNote = r.minor <= 0 ? "an amount is always positive — swap from and to to send it the other way" : "";
            }
        });
    }
    function validateEditArrives(text) {
        if (text.trim().length === 0) {
            root.editToOk = false;
            arrivesEdit.invalid = false;
            return;
        }
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.editToCurrency) }, (r, e) => {
            if (arrivesEdit.text !== text)
                return;
            if (e) {
                root.editToOk = false;
                arrivesEdit.invalid = true;
                root.editNote = e.message;
            } else {
                root.editToMinor = r.minor;
                root.editToOk = r.minor > 0;
                arrivesEdit.invalid = r.minor <= 0;
                root.editNote = r.minor <= 0 ? "an amount is always positive" : "";
            }
        });
    }
    // The amounts were parsed at one scale; a different account may mean a different one.
    onEditFromCurrencyChanged: if (root.editing >= 0 && amountEdit.text.trim().length > 0) root.validateEditAmount(amountEdit.text)
    onEditToCurrencyChanged: if (root.editing >= 0 && arrivesEdit.text.trim().length > 0) root.validateEditArrives(arrivesEdit.text)

    // One call, whatever changed: the core applies what is given and leaves the rest. The legs
    // are restated the way the entry form states them -- money LEAVES from and ARRIVES at to --
    // or, across currencies, as the two real amounts for the core to build the conversion from.
    function saveEdit() {
        if (!root.editComplete)
            return;
        const id = root.editing;
        const params = { id: id, occurred_on: editDate.text, description: editDesc.text.trim() };
        if (root.editSimple && root.editCross) {
            params.conversion = { from_account: root.editFrom, from_minor: root.editAmountMinor,
                                  to_account: root.editTo, to_minor: root.editToMinor };
        } else if (root.editSimple) {
            params.postings = [
                { account_id: root.editFrom, amount_minor: -root.editAmountMinor },
                { account_id: root.editTo, amount_minor: root.editAmountMinor }
            ];
        }
        Ledger.write("txn.update", params, (r, e) => {
            if (e) { root.editNote = e.message; return; }
            root.closeEditor();
            root.note = "";
            // The core answers with the payment as stored -- trimmed, legs rebuilt -- so the kept
            // row shows what is in the book, not what was typed.
            const at = root.listRows.findIndex(x => x.id === id);
            root.kept = at < 0 ? null
                      : { id: id, at: at,
                          row: Object.assign({}, root.listRows[at],
                                             { occurred_on: r.occurred_on, description: r.description,
                                               postings: r.postings }) };
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
        root.closeEditor();
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

    // The legs that say where money went: a conversion's own legs sit in the conversion accounts
    // and only make each currency balance, so they are left out. Sorted so the leg that left is
    // first and the leg that arrived is last; a split's smaller arrivals sit in between.
    function ends(t) {
        return (t.postings || []).filter(p => p.kind !== "conversion")
                                 .sort((a, b) => a.amount_minor - b.amount_minor);
    }
    function money(t) {
        // The headline is the side that left: for an ordinary payment that is the amount, and for
        // a conversion it is what was sent, in the currency it was sent in. Picking the biggest leg
        // instead used to show a GBP to EUR conversion as its EUR side, because at any rate above
        // par the arriving number is the larger one.
        const ps = root.ends(t);
        if (ps.length === 0) return "";
        return Money.format(Math.abs(ps[0].amount_minor), ps[0].currency) + " " + ps[0].currency;
    }
    function route(t) {
        const ps = root.ends(t);
        if (ps.length < 2) return "";
        const from = ps[0], to = ps[ps.length - 1];
        const line = from.account + " → " + to.account;
        // Across currencies the route says so: the accounts alone do not.
        return from.currency === to.currency
               ? line : line + " (" + from.currency + ">" + to.currency + ")";
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
                text: "+1 edited"
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
                width: root.sideOpen ? parent.width * 0.56 : parent.width
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
                        readonly property bool editing: root.editing === prow.modelData.id
                        readonly property bool marked: root.following === prow.modelData.id || prow.editing
                        width: ListView.view.width
                        height: 34
                        radius: 3
                        color: root.isPicked(prow.modelData.id) ? Theme.purpleDim
                             : phov.containsMouse ? Theme.surface : "transparent"
                        border.width: prow.marked || prow.held ? 1 : 0
                        border.color: prow.marked ? Theme.purple : Theme.warnAmber

                        MouseArea {
                            id: phov
                            anchors.fill: parent
                            hoverEnabled: true
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.toggle(prow.modelData.id)
                        }

                        Row {
                            anchors.fill: parent
                            anchors.leftMargin: 6
                            anchors.rightMargin: 6
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
                                        text: "· edited, not in this search"
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
                                    if (prow.editing)
                                        root.closeEditor();
                                    else
                                        root.edit(prow.modelData);
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
                    }
                }
            }

            // ---- editing a payment ----
            // Always instantiated, so the fields exist for edit() to fill: hidden rather than
            // absent, in the space the thread pane otherwise takes.
            Rectangle {
                visible: root.editing >= 0
                width: parent.width * 0.44 - 10
                height: parent.height
                color: Theme.ground
                radius: 5
                border.width: 1
                border.color: Theme.purple

                Column {
                    id: editCol
                    anchors.fill: parent
                    anchors.margins: 8
                    spacing: 6

                    Row {
                        width: parent.width
                        spacing: 6
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: "EDIT PAYMENT #" + root.editing
                            color: Theme.purple
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                        Item { width: parent.width - 190; height: 1 }
                        PushButton {
                            implicitHeight: 22
                            label: "close"
                            onClicked: root.closeEditor()
                        }
                    }

                    Row {
                        id: editTop
                        width: parent.width
                        spacing: 6
                        Field {
                            id: editDate
                            width: 118
                            label: "date"
                            placeholder: "YYYY-MM-DD"
                            numeric: true
                            onAccepted: root.saveEdit()
                        }
                        Field {
                            id: amountEdit
                            visible: root.editSimple
                            width: editTop.width - 118 - 6
                            label: root.editFromCurrency.length > 0
                                   ? "amount (" + root.editFromCurrency + ")" : "amount"
                            placeholder: "0.00"
                            numeric: true
                            onEdited: value => root.validateEditAmount(value)
                            onAccepted: root.saveEdit()
                        }
                    }

                    DescriptionPicker {
                        id: editDesc
                        width: parent.width
                        label: "description"
                        placeholder: "what was it?"
                        onAccepted: root.saveEdit()
                    }

                    Row {
                        id: editPickers
                        visible: root.editSimple
                        width: parent.width
                        spacing: 6
                        AccountPicker {
                            id: fromEdit
                            width: (editPickers.width - 6) / 2
                            label: "from"
                            accounts: root.accounts
                            onPicked: id => root.editFrom = id
                        }
                        AccountPicker {
                            id: toEdit
                            width: (editPickers.width - 6) / 2
                            label: "to"
                            accounts: root.accounts
                            onPicked: id => root.editTo = id
                        }
                    }

                    // Only across currencies. What arrived is asked for, not calculated: the
                    // rate that matters is the one the bank gave, and only the two amounts know it.
                    Row {
                        id: editFx
                        visible: root.editCross
                        width: parent.width
                        spacing: 6
                        Field {
                            id: arrivesEdit
                            width: 118
                            label: "arrives as (" + root.editToCurrency + ")"
                            placeholder: "0.00"
                            numeric: true
                            onEdited: value => root.validateEditArrives(value)
                            onAccepted: root.saveEdit()
                        }
                        Column {
                            anchors.verticalCenter: parent.verticalCenter
                            width: editFx.width - 118 - 6
                            spacing: 1
                            Text {
                                width: parent.width
                                elide: Text.ElideRight
                                text: root.editRate.length > 0 ? root.editRate : "enter what arrived"
                                color: root.editRate.length > 0 ? Theme.text : Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 3
                            }
                            Text {
                                width: parent.width
                                elide: Text.ElideRight
                                text: "a conversion: two real amounts, not a stored rate"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                    }

                    // A split is not the form's shape, and the editor does not pretend it is.
                    Text {
                        visible: !root.editSimple
                        width: parent.width
                        wrapMode: Text.Wrap
                        text: "This payment has " + root.editLegs + " legs of its own, so only its "
                            + "date and description can be changed here."
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }

                    Text {
                        visible: root.editNote.length > 0
                        width: parent.width
                        wrapMode: Text.Wrap
                        text: root.editNote
                        color: Theme.red
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }

                    Row {
                        spacing: 6
                        PushButton {
                            label: "Save"
                            primary: true
                            enabled: root.editComplete
                            onClicked: root.saveEdit()
                        }
                        PushButton {
                            label: "Cancel"
                            onClicked: root.closeEditor()
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            visible: root.editSimple && root.editFrom >= 0 && root.editFrom === root.editTo
                            text: "from and to must differ"
                            color: Theme.warnAmber
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
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
