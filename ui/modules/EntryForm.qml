pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Record a payment. The daily action, so it is the one form that must be quick.
//
// TWO POSTINGS ONLY. A split transaction is expressible in the core but not here: the common case
// is money moving between two places, and making the form handle N legs would slow down the case
// that happens fifty times a month to serve the one that happens twice a year.
//
// A CHAIN is not an exception to that. Money that passes through somewhere on the way -- current
// account to a friend to a bookmaker -- is entered here as the stops it passes through, and the core
// records one plain two-posting payment PER LEG, each linked to the one before so the payments
// panel shows them as one thread. Only the description is shared: each leg has its own date and
// amount, defaulting to the one above so the common case types nothing extra, and a fee on the way
// or a leg that lands days later is just a stop that says so.
//
// The AMOUNT IS PARSED BY THE CORE, not here. `money.parse` exists precisely so the rounding rule
// lives in one place -- a QML reimplementation would be a second rule waiting to disagree at the
// half-penny, and it would accept "1.005" that the core refuses.
Rectangle {
    id: root

    property var accounts: []
    property string defaultDate: ""

    signal saved
    signal cancelled

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8
    // Content-driven, because the form GROWS: the cross-currency row appears only when the two
    // accounts differ, and a fixed height that fits without it clips the buttons with it.
    implicitHeight: form.implicitHeight + 24

    property int fromAccount: -1
    property int toAccount: -1
    // The stops the money passes THROUGH, in order. Each is { account, date, amountText,
    // amountMinor, amountOk, amountBad }: `account` is -1 until chosen; `date` and `amountText`
    // describe the leg LEAVING that stop and are blank to mean "same as the leg above"; `amountOk`
    // is whether Record may go ahead (blank counts) and `amountBad` whether the core has refused
    // what was typed. Kept apart from
    // `stopCount`, the Repeater's model, so that editing a stop reassigns this array and
    // re-evaluates every binding without rebuilding the rows; only adding or removing a stop
    // changes the count, and that rebuilds every row from this array.
    property var stops: []
    property int stopCount: 0
    property int amountMinor: 0
    property bool amountOk: false
    // The second leg, only used when the two accounts are in different currencies.
    property int toMinor: 0
    property bool toOk: false

    function currencyOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.currency : "";
    }
    readonly property string fromCurrency: root.currencyOf(root.fromAccount)
    readonly property string toCurrency: root.currencyOf(root.toAccount)
    // A transaction must balance PER CURRENCY, so two currencies cannot be two postings: it needs
    // conversion postings through a trading account, which is what txn.convert builds. A chain
    // never takes this path: the core refuses a chain that changes currency, so the conversion
    // hop is recorded on its own, as the different shape of transaction it is.
    readonly property bool crossCurrency: root.stopCount === 0
                                          && root.fromCurrency.length > 0
                                          && root.toCurrency.length > 0
                                          && root.fromCurrency !== root.toCurrency

    // Every account the money visits, in order.
    readonly property var route: [root.fromAccount].concat(root.stops.map(s => s.account), [root.toAccount])
    readonly property bool routeChosen: root.route.every(id => id >= 0)
    // Consecutive accounts must differ; from and to may coincide in a chain -- money that goes out
    // through a friend and comes back is a real thing to record.
    readonly property bool routeMoves: root.route.every((id, i) => i === 0 || id !== root.route[i - 1])
    readonly property bool routeOneCurrency: root.route.every(id => id < 0 || root.currencyOf(id) === root.fromCurrency)
    // A blank date or amount on a stop means "same as above"; a typed one must be whole.
    readonly property bool stopDatesOk: root.stops.every(s => s.date.length === 0 || s.date.length === 10)
    readonly property bool stopsFilledIn: root.stopDatesOk && root.stops.every(s => s.amountOk)
    // Money can only pass through somewhere it can sit. The core refuses the rest; this just
    // keeps them out of the via list.
    readonly property var passThroughAccounts: (root.accounts || []).filter(a => a.kind === "asset" || a.kind === "liability")

    function nameOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.name : "…";
    }
    // "Current → Sam → Bookmaker", for the line that says what Record will do.
    readonly property string routeText: root.route.map(id => root.nameOf(id)).join(" → ")

    function addStop() {
        root.stops = root.stops.concat([{ account: -1, date: "", amountText: "", amountMinor: 0,
                                          amountOk: true, amountBad: false }]);
        root.stopCount = root.stops.length;
    }
    function updateStop(i, patch) {
        const s = root.stops.slice();
        s[i] = Object.assign({}, s[i], patch);
        root.stops = s;
    }
    function removeStop(i) {
        const s = root.stops.slice();
        s.splice(i, 1);
        // Count first: the rows are rebuilt from the count, and shrinking the array while the last
        // row still exists would have it read past the end.
        root.stopCount = s.length;
        root.stops = s;
        status.text = "";
    }
    // A stop's onward amount, parsed by the core like the headline one. Blank means "same as
    // above" and is fine; anything else must parse to a positive amount.
    function validateStopAmount(i, text) {
        if (text.trim().length === 0) {
            root.updateStop(i, { amountText: text, amountMinor: 0, amountOk: true, amountBad: false });
            status.text = "";
            return;
        }
        root.updateStop(i, { amountText: text, amountOk: false, amountBad: false });
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.fromCurrency) }, (r, e) => {
            if (i >= root.stops.length || root.stops[i].amountText !== text)
                return; // the row went, or she kept typing: a later answer is on its way
            if (e) {
                status.text = e.message;
                root.updateStop(i, { amountBad: true });
            } else {
                status.text = r.minor > 0 ? "" : "an amount is always positive";
                root.updateStop(i, { amountMinor: r.minor, amountOk: r.minor > 0, amountBad: r.minor <= 0 });
            }
        });
    }
    // Stop amounts were parsed at the from account's scale; a different from account means a
    // different scale, so ask again.
    onFromCurrencyChanged: {
        root.stops.forEach((s, i) => {
            if (s.amountText.trim().length > 0)
                root.validateStopAmount(i, s.amountText);
        });
    }
    // The payments to record, one per leg: the headline date and amount for the first, and then
    // whatever each stop says about the leg leaving it, or the same again if it says nothing.
    function chainHops() {
        let date = dateField.text;
        let amount = root.amountMinor;
        const targets = root.stops.map(s => s.account).concat([root.toAccount]);
        const hops = [];
        for (let i = 0; i < targets.length; i++) {
            if (i > 0) {
                const stop = root.stops[i - 1];
                if (stop.date.length > 0)
                    date = stop.date;
                if (stop.amountText.trim().length > 0)
                    amount = stop.amountMinor;
            }
            hops.push({ to_account: targets[i], occurred_on: date, amount_minor: amount });
        }
        return hops;
    }

    function reset() {
        dateField.text = root.defaultDate;
        descField.clear();
        amountField.clear();
        toField.clear();
        root.amountMinor = 0;
        root.amountOk = false;
        root.toMinor = 0;
        root.toOk = false;
        root.fromAccount = -1;
        root.toAccount = -1;
        // The pickers keep their own `selected` once chosen, so clearing the ids alone left the
        // old names on screen with a Record button that would not press.
        fromPicker.selected = -1;
        toPicker.selected = -1;
        root.stops = [];
        root.stopCount = 0;
        status.text = "";
    }

    // Ask the core what the typed amount means. Its answer is the only truth about the value.
    function validateAmount(text) {
        if (text.trim().length === 0) {
            root.amountOk = false;
            amountField.invalid = false;
            return;
        }
        // At the FROM account's scale, not a hardcoded 2: typing 1000 into a JPY account must
        // record 1000 yen, not 10.
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.fromCurrency) }, (r, e) => {
            if (e) {
                root.amountOk = false;
                amountField.invalid = true;
                status.text = e.message;
            } else {
                // Positive on every path. The form decides the direction from `from` and `to`, so
                // a minus sign would be a second, contradicting way of saying which way it went.
                root.amountMinor = r.minor;
                root.amountOk = r.minor > 0;
                amountField.invalid = r.minor < 0;
                status.text = r.minor < 0 ? "an amount is always positive — swap from and to to send it the other way" : "";
            }
        });
    }

    function validateTo(text) {
        if (text.trim().length === 0) {
            root.toOk = false;
            toField.invalid = false;
            return;
        }
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.toCurrency) }, (r, e) => {
            if (e) {
                root.toOk = false;
                toField.invalid = true;
                status.text = e.message;
            } else {
                root.toMinor = r.minor;
                root.toOk = r.minor > 0;
                toField.invalid = r.minor < 0;
                status.text = r.minor < 0 ? "an amount is always positive" : "";
            }
        });
    }

    // Offered as a STARTING POINT only. What gets stored is the two amounts as typed, because the
    // executed rate is the ratio of the legs that actually moved -- a bank's rate, with its spread
    // -- and is never the mid-market rate the book happens to know.
    function suggest() {
        if (!root.crossCurrency || !root.amountOk)
            return;
        Ledger.request("fx.convert", {
            amount_minor: Math.abs(root.amountMinor),
            from: root.fromCurrency, to: root.toCurrency, on: dateField.text
        }, (r, e) => {
            if (e) {
                status.text = e.message + " — enter the amount received yourself";
                return;
            }
            toField.text = Money.format(r.minor, root.toCurrency);
            root.validateTo(toField.text);
        });
    }

    readonly property bool complete: root.amountOk && root.routeChosen && root.routeMoves
                                     && (root.stopCount > 0 || root.fromAccount !== root.toAccount)
                                     && (root.stopCount === 0 || (root.routeOneCurrency && root.stopsFilledIn))
                                     && dateField.text.length === 10 && descField.text.length > 0
                                     && (!root.crossCurrency || root.toOk)

    // Shown, never stored. Two integer legs ARE the rate; deriving a decimal from them for display
    // is fine, storing one would lose the exactness the legs already have.
    readonly property string impliedRate: {
        if (!root.crossCurrency || !root.amountOk || !root.toOk)
            return "";
        const f = Math.abs(root.amountMinor) / Math.pow(10, Money.digits(root.fromCurrency));
        const t = Math.abs(root.toMinor) / Math.pow(10, Money.digits(root.toCurrency));
        if (f === 0)
            return "";
        return "1 " + root.fromCurrency + " = " + (t / f).toFixed(4) + " " + root.toCurrency;
    }

    function save() {
        if (!root.complete)
            return;
        // A chain is one core call, not one txn.create per leg from here: the core writes every
        // leg or none, links each to the one before, and copies the description to all of them.
        // Doing the loop in the form would leave half a chain behind on the first refusal.
        if (root.stopCount > 0) {
            Ledger.write("txn.create_chain", {
                description: descField.text,
                from_account: root.fromAccount,
                hops: root.chainHops()
            }, (r, e) => {
                if (e)
                    status.text = e.message;
                else {
                    root.reset();
                    root.saved();
                }
            });
            return;
        }
        // Two currencies need conversion postings through a trading account, which is a different
        // shape of transaction entirely -- so it is a different core call, not extra postings
        // assembled here. build_conversion owns that shape and this form does not restate it.
        if (root.crossCurrency) {
            Ledger.write("txn.convert", {
                occurred_on: dateField.text,
                description: descField.text,
                from_account: root.fromAccount,
                from_minor: Math.abs(root.amountMinor),
                to_account: root.toAccount,
                to_minor: Math.abs(root.toMinor)
            }, (r, e) => {
                if (e)
                    status.text = e.message;
                else {
                    root.reset();
                    root.saved();
                }
            });
            return;
        }
        // Double entry, made by the form rather than asked of the operator: money LEAVES `from`
        // and ARRIVES at `to`, so the signs are not a decision anyone should have to make.
        Ledger.write("txn.create", {
            occurred_on: dateField.text,
            description: descField.text,
            postings: [
                { account_id: root.fromAccount, amount_minor: -root.amountMinor },
                { account_id: root.toAccount, amount_minor: root.amountMinor }
            ]
        }, (r, e) => {
            if (e) {
                status.text = e.message;
            } else {
                root.reset();
                root.saved();
            }
        });
    }

    Column {
        id: form
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 12
        spacing: 8

        Text {
            text: root.stopCount > 0 ? "RECORD A CHAIN" : "RECORD A PAYMENT"
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            id: topRow
            spacing: 8
            width: parent.width
            Field {
                id: dateField
                width: (topRow.width - 8) * 0.35
                label: "date"
                placeholder: "YYYY-MM-DD"
                numeric: true
                text: root.defaultDate
            }
            Field {
                id: amountField
                width: (topRow.width - 8) * 0.65
                label: root.fromCurrency.length > 0 ? "amount (" + root.fromCurrency + ")" : "amount"
                placeholder: "0.00"
                numeric: true
                onEdited: value => root.validateAmount(value)
            }
        }

        // A Field that also suggests the descriptions already in the book, most-used first. It is
        // still free text -- `complete` and `save()` read `descField.text` exactly as they did
        // when this was a plain Field, because most descriptions are new and the list only exists
        // to save retyping the ones that are not.
        DescriptionPicker {
            id: descField
            width: parent.width
            label: "description"
            placeholder: "what was it?"
        }

        Row {
            id: pickerRow
            spacing: 8
            width: parent.width
            AccountPicker {
                id: fromPicker
                width: (pickerRow.width - 8) / 2
                label: "from"
                accounts: root.accounts
                onPicked: id => root.fromAccount = id
            }
            AccountPicker {
                id: toPicker
                width: (pickerRow.width - 8) / 2
                label: "to"
                accounts: root.accounts
                onPicked: id => root.toAccount = id
            }
        }

        // One row per stop the money passes through, between from and to: the stop itself, and
        // what the leg LEAVING it looks like when it differs from the leg above. Changing the
        // count rebuilds every row from `stops`, which is why each field is bound to the array
        // rather than remembering anything of its own.
        Repeater {
            model: root.stopCount
            delegate: Row {
                id: stopRow
                required property int index
                // Read through a fallback: a row on its way out can evaluate once more against an
                // array that no longer has its entry.
                readonly property var stop: root.stops[stopRow.index]
                                            ?? ({ account: -1, date: "", amountText: "", amountMinor: 0,
                                                  amountOk: true, amountBad: false })
                width: form.width
                spacing: 8
                AccountPicker {
                    width: (stopRow.width - 24) * 0.40
                    label: "via"
                    accounts: root.passThroughAccounts
                    selected: stopRow.stop.account
                    onPicked: id => root.updateStop(stopRow.index, { account: id })
                }
                Field {
                    width: (stopRow.width - 24) * 0.22
                    label: "then on (YYYY-MM-DD)"
                    placeholder: "same day"
                    numeric: true
                    text: stopRow.stop.date
                    onEdited: value => root.updateStop(stopRow.index, { date: value })
                }
                Field {
                    width: (stopRow.width - 24) * 0.22
                    label: root.fromCurrency.length > 0 ? "then sends (" + root.fromCurrency + ")" : "then sends"
                    placeholder: "same amount"
                    numeric: true
                    text: stopRow.stop.amountText
                    onEdited: value => root.validateStopAmount(stopRow.index, value)
                    // Field clears `invalid` itself on every keystroke, which would end a plain
                    // binding for good; a Binding element keeps re-applying the core's verdict.
                    Binding on invalid {
                        value: stopRow.stop.amountBad
                    }
                }
                PushButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "remove"
                    onClicked: root.removeStop(stopRow.index)
                }
            }
        }

        Row {
            id: chainRow
            width: parent.width
            spacing: 12
            PushButton {
                id: addStopButton
                label: "+ stop"
                onClicked: root.addStop()
            }
            Column {
                anchors.verticalCenter: parent.verticalCenter
                width: chainRow.width - addStopButton.width - chainRow.spacing
                spacing: 1
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    text: root.stopCount > 0
                          ? root.routeText
                          : "a chain: add each account the money passes through on the way"
                    color: root.stopCount > 0 ? Theme.text : Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: root.stopCount > 0 ? Theme.fontSize - 2 : Theme.fontSize - 4
                }
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    visible: root.stopCount > 0
                    text: (root.stopCount + 1) + " payments, one for each leg, sharing the description"
                          + " — a blank date or amount on a stop reuses the one above"
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 4
                }
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    visible: root.stopCount > 0 && root.routeChosen && root.fromAccount === root.toAccount
                    text: "a round trip: leaves and comes back to " + root.nameOf(root.fromAccount)
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
        }

        // Only when the two sides are in different currencies. The second amount is asked for
        // rather than calculated, because the rate that matters is the one the bank actually gave
        // -- spread included -- and only the two real amounts know it.
        Row {
            id: fxRow
            visible: root.crossCurrency
            width: parent.width
            spacing: 8
            Field {
                id: toField
                width: 150
                label: "arrives as (" + root.toCurrency + ")"
                numeric: true
                placeholder: "0.00"
                onEdited: value => root.validateTo(value)
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "use today's rate"
                enabled: root.amountOk
                onClicked: root.suggest()
            }
            Column {
                anchors.verticalCenter: parent.verticalCenter
                spacing: 1
                Text {
                    text: root.impliedRate.length > 0
                          ? root.impliedRate
                          : "enter what arrived, or fetch a starting point"
                    color: root.impliedRate.length > 0 ? Theme.text : Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 2
                }
                Text {
                    text: "recorded as two real amounts, not a stored rate"
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
        }

        Text {
            id: status
            width: parent.width
            wrapMode: Text.Wrap
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            spacing: 8
            PushButton {
                label: root.stopCount > 0 ? "Record " + (root.stopCount + 1) + " payments" : "Record"
                primary: true
                enabled: root.complete
                onClicked: root.save()
            }
            PushButton {
                label: "Clear"
                onClicked: root.reset()
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.stopCount === 0 && root.fromAccount >= 0
                         && root.fromAccount === root.toAccount
                text: "from and to must differ"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.stopCount > 0 && root.routeChosen && !root.routeMoves
                text: "the same account twice in a row"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.stopCount > 0 && root.routeChosen && root.routeMoves && !root.routeOneCurrency
                text: "a chain stays in one currency — record the conversion as its own payment"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.stopCount > 0 && root.routeMoves && root.routeOneCurrency && !root.stopDatesOk
                text: "a stop's date needs YYYY-MM-DD"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }
    }
}
