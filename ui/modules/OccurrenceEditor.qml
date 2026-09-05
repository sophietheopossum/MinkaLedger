import QtQuick
import "../services"

// Alter ONE occurrence of a recurring payment — requirement 4.
//
// This is the feature that most needs a GUI. Skipping a single month's rent, or moving it because
// payday landed late, is a small thing you do while looking at the forecast; expressing it as a
// hand-written override row is absurd. So this opens from the row it applies to and closes again.
//
// THE SLOT IS NOT THE DATE. `occurrence_on` identifies which occurrence this is and never changes;
// `moved_to` changes when the money actually moves. Keeping them apart is what stops a moved
// payment from being projected twice, and it is why the header shows the slot even when the value
// date differs.
//
// IT HAPPENED is the other thing you say while looking at the forecast. "Record as paid" writes
// the real payment from what the fields say -- the date and the amount that actually moved --
// and claims the slot, so the projection stops showing it and the balance walk stops counting
// it twice. For a chain that is every hop of the wave, linked hop to hop, as the entry form
// would have recorded them.
Rectangle {
    id: root

    property var occurrence: null
    readonly property bool active: occurrence !== null
    // A leg of a recurring chain is edited as the chain: a month with no payment is a month with
    // no payment end to end. The core fans it out; this only says so.
    readonly property bool chained: root.active && root.occurrence.chain_len !== undefined
                                    && root.occurrence.chain_len !== null
    // A what-if's occurrence can be moved or skipped like any other, but it is a question, not
    // money that moved, so it cannot be recorded as paid.
    readonly property bool hypothetical: root.active && root.occurrence.scenario_id !== undefined
                                         && root.occurrence.scenario_id !== null

    signal changed
    signal dismissed

    visible: active
    color: Theme.surfaceRaised
    border.width: 1
    border.color: Theme.purple
    radius: 8

    property int newAmountMinor: 0
    property bool amountOk: false

    onOccurrenceChanged: {
        if (!occurrence)
            return;
        moveField.text = occurrence.value_on;
        amountField.text = Money.format(Math.abs(occurrence.amount_minor),
                                        occurrence.currency);
        root.newAmountMinor = Math.abs(occurrence.amount_minor);
        root.amountOk = true;
        status.text = "";
    }

    function validateAmount(text) {
        // The occurrence carries its own currency, so the scale is never assumed.
        Ledger.request("money.parse",
                       { text: text,
                         minor_digits: Money.digits(root.occurrence.currency) }, (r, e) => {
            if (e) {
                root.amountOk = false;
                amountField.invalid = true;
                status.text = e.message;
            } else {
                root.newAmountMinor = Math.abs(r.minor);
                root.amountOk = true;
                amountField.invalid = false;
                status.text = "";
            }
        });
    }

    // The sign belongs to the series, not to the operator: a rent occurrence stays an outgoing one
    // whatever number is typed, so the magnitude is edited and the direction preserved.
    function signedAmount() {
        return root.occurrence.amount_minor < 0 ? -root.newAmountMinor : root.newAmountMinor;
    }

    function apply(action) {
        const params = {
            series_id: root.occurrence.series_id,
            occurrence_on: root.occurrence.occurrence_on,
            action: action
        };
        if (action === "amend") {
            if (moveField.text !== root.occurrence.occurrence_on)
                params.moved_to = moveField.text;
            params.amount_minor = root.signedAmount();
        }
        Ledger.write("series.override", params, (r, e) => {
            if (e)
                status.text = e.message;
            else {
                root.occurrence = null;
                root.changed();
            }
        });
    }

    // The occurrence became a real payment: write it from the fields and claim the slot.
    function record() {
        Ledger.write("series.record", {
            series_id: root.occurrence.series_id,
            occurrence_on: root.occurrence.occurrence_on,
            occurred_on: moveField.text,
            amount_minor: root.signedAmount(),
            whole_chain: root.chained
        }, (r, e) => {
            if (e)
                status.text = e.message;
            else {
                root.occurrence = null;
                root.changed();
            }
        });
    }

    function clearOverride() {
        Ledger.write("series.clear_override", {
            series_id: root.occurrence.series_id,
            occurrence_on: root.occurrence.occurrence_on
        }, (r, e) => {
            if (e)
                status.text = e.message;
            else {
                root.occurrence = null;
                root.changed();
            }
        });
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Text {
            width: parent.width
            elide: Text.ElideRight
            text: root.active ? root.occurrence.description : ""
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize
        }
        Text {
            text: !root.active ? ""
                  : "occurrence " + root.occurrence.occurrence_on
                    + (root.chained
                       ? " · all " + root.occurrence.chain_len + " legs of this chain"
                       : " · one instance only")
            color: Theme.textFaint
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 3
        }

        Row {
            id: fields
            spacing: 8
            width: parent.width
            Field {
                id: moveField
                width: (fields.width - 8) / 2
                label: "date"
                numeric: true
                placeholder: "YYYY-MM-DD"
            }
            Field {
                id: amountField
                width: (fields.width - 8) / 2
                label: "amount"
                numeric: true
                onEdited: value => root.validateAmount(value)
            }
        }

        Text {
            visible: root.hypothetical
            width: parent.width
            wrapMode: Text.Wrap
            text: "a what-if: it can be moved or skipped here, but not recorded as paid"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }
        Text {
            visible: root.chained
            width: parent.width
            wrapMode: Text.Wrap
            text: "the amount change carries through every leg as a difference, so a fee stays a fee"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
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
                visible: !root.hypothetical
                label: root.chained ? "Record all " + root.occurrence.chain_len + " legs as paid"
                                    : "Record as paid"
                primary: true
                enabled: root.amountOk && moveField.text.length === 10
                onClicked: root.record()
            }
            PushButton {
                label: "Apply"
                enabled: root.amountOk
                onClicked: root.apply("amend")
            }
            PushButton {
                label: root.chained ? "Skip all " + root.occurrence.chain_len + " legs" : "Skip this one"
                onClicked: root.apply("skip")
            }
            PushButton {
                label: "Reset"
                onClicked: root.clearOverride()
            }
            PushButton {
                label: "Close"
                onClicked: {
                    root.occurrence = null;
                    root.dismissed();
                }
            }
        }
    }
}
