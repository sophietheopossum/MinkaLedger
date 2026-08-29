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
Rectangle {
    id: root

    property var occurrence: null
    readonly property bool active: occurrence !== null

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
        amountField.text = (Math.abs(occurrence.amount_minor) / 100).toFixed(2);
        root.newAmountMinor = Math.abs(occurrence.amount_minor);
        root.amountOk = true;
        status.text = "";
    }

    function validateAmount(text) {
        Ledger.request("money.parse", { text: text, minor_digits: 2 }, (r, e) => {
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
            text: root.active ? ("occurrence " + root.occurrence.occurrence_on
                                 + " · one instance only") : ""
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
                label: "move to"
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
                label: "Apply"
                primary: true
                enabled: root.amountOk
                onClicked: root.apply("amend")
            }
            PushButton {
                label: "Skip this one"
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
