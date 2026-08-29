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

    property int fromAccount: -1
    property int toAccount: -1
    property int amountMinor: 0
    property bool amountOk: false

    function reset() {
        dateField.text = root.defaultDate;
        descField.clear();
        amountField.clear();
        root.amountMinor = 0;
        root.amountOk = false;
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
        Ledger.request("money.parse", { text: text, minor_digits: 2 }, (r, e) => {
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

    readonly property bool complete: root.amountOk && root.fromAccount >= 0
                                     && root.toAccount >= 0 && root.fromAccount !== root.toAccount
                                     && dateField.text.length === 10 && descField.text.length > 0

    function save() {
        if (!root.complete)
            return;
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
        anchors.fill: parent
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
                label: "amount"
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
