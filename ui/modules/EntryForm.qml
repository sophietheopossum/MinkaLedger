import QtQuick
import "../services"

// Record a payment. The daily action, so it is the one form that must be quick.
//
// TWO POSTINGS ONLY. A split transaction is expressible in the core but not here: the common case
// is money moving between two places, and making the form handle N legs would slow down the case
// that happens fifty times a month to serve the one that happens twice a year.
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
    // conversion postings through a trading account, which is what txn.convert builds.
    readonly property bool crossCurrency: root.fromCurrency.length > 0
                                          && root.toCurrency.length > 0
                                          && root.fromCurrency !== root.toCurrency

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
                root.amountMinor = r.minor;
                root.amountOk = r.minor !== 0;
                amountField.invalid = false;
                status.text = "";
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
                root.toOk = r.minor !== 0;
                toField.invalid = false;
                status.text = "";
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

    readonly property bool complete: root.amountOk && root.fromAccount >= 0
                                     && root.toAccount >= 0 && root.fromAccount !== root.toAccount
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
            text: "RECORD A PAYMENT"
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

        Field {
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
                label: "Record"
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
                visible: root.fromAccount >= 0 && root.fromAccount === root.toAccount
                text: "from and to must differ"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }
    }
}
