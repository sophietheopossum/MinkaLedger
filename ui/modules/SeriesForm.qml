pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Create a recurring payment.
//
// THE POINT OF THIS FORM is that nobody should have to write
// `FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1` to say "payday". The presets cover what UK
// personal finance actually does; the generated rule is shown rather than hidden, so it teaches
// rather than conceals, and `custom` is there for anything the presets miss.
//
// AND IT PREVIEWS. `series.preview` expands the rule without saving, so you see the real dates --
// including where a weekend rule will move one -- before committing to twelve months of it. Getting
// a recurrence subtly wrong is otherwise something you discover a month later.
Rectangle {
    id: root

    property var accounts: []
    property string defaultDate: ""
    // When set, the series is created INSIDE a scenario rather than as baseline: it then only
    // affects the forecast while that scenario is switched on. -1 is baseline.
    property int scenarioId: -1
    property string scenarioName: ""

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
    property string preset: "monthly_day"
    property int dayOfMonth: 1
    property string weekday: "MO"
    property string weekendRule: "none"
    property var previewDates: []

    // Each preset knows how to render itself as an RRULE. Kept as one function so the mapping from
    // "what a person means" to "what RFC 5545 says" lives in a single readable place.
    readonly property string rrule: {
        switch (preset) {
        case "monthly_day":   return "FREQ=MONTHLY;BYMONTHDAY=" + dayOfMonth;
        case "monthly_last":  return "FREQ=MONTHLY;BYMONTHDAY=-1";
        case "payday":        return "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1";
        case "weekly":        return "FREQ=WEEKLY;BYDAY=" + weekday;
        case "fortnightly":   return "FREQ=WEEKLY;INTERVAL=2;BYDAY=" + weekday;
        case "four_weekly":   return "FREQ=WEEKLY;INTERVAL=4;BYDAY=" + weekday;
        case "yearly":        return "FREQ=YEARLY";
        default:              return customRule.text;
        }
    }

    readonly property bool needsDay: preset === "monthly_day"
    readonly property bool needsWeekday: preset === "weekly" || preset === "fortnightly"
                                         || preset === "four_weekly"

    function reset() {
        descField.clear();
        amountField.clear();
        startField.text = root.defaultDate;
        root.amountMinor = 0;
        root.amountOk = false;
        root.fromAccount = -1;
        root.toAccount = -1;
        root.previewDates = [];
        status.text = "";
    }

    function validateAmount(text) {
        if (text.trim().length === 0) {
            root.amountOk = false;
            return;
        }
        Ledger.request("money.parse", { text: text, minor_digits: 2 }, (r, e) => {
            if (e) {
                root.amountOk = false;
                amountField.invalid = true;
                status.text = e.message;
            } else {
                root.amountMinor = Math.abs(r.minor);
                root.amountOk = r.minor !== 0;
                amountField.invalid = false;
                status.text = "";
            }
        });
    }

    function refreshPreview() {
        if (root.rrule.length === 0 || startField.text.length !== 10) {
            root.previewDates = [];
            return;
        }
        Ledger.request("series.preview", {
            rrule: root.rrule, dtstart: startField.text,
            count: 5, weekend_rule: root.weekendRule
        }, (r, e) => {
            if (e) {
                root.previewDates = [];
                status.text = e.message;
            } else {
                root.previewDates = r.dates;
                status.text = "";
            }
        });
    }

    onRruleChanged: root.refreshPreview()
    onWeekendRuleChanged: root.refreshPreview()

    readonly property bool complete: root.amountOk && root.fromAccount >= 0 && root.toAccount >= 0
                                     && root.fromAccount !== root.toAccount
                                     && descField.text.length > 0 && root.previewDates.length > 0

    function save() {
        if (!root.complete)
            return;
        const params = {
            description: descField.text,
            rrule: root.rrule,
            dtstart: startField.text,
            weekend_rule: root.weekendRule,
            postings: [
                { account_id: root.fromAccount, amount_minor: -root.amountMinor, role: "primary" },
                { account_id: root.toAccount, amount_minor: root.amountMinor, role: "balancing" }
            ]
        };
        if (root.scenarioId >= 0)
            params.scenario_id = root.scenarioId;
        Ledger.write("series.create", params, (r, e) => {
            if (e)
                status.text = e.message;
            else {
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
            text: root.scenarioId < 0
                  ? "NEW RECURRING PAYMENT"
                  : "HYPOTHETICAL PAYMENT — only in “" + root.scenarioName + "”"
            color: root.scenarioId < 0 ? Theme.textMuted : Theme.purple
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            id: r1
            spacing: 8
            width: parent.width
            Field {
                id: descField
                width: (r1.width - 8) * 0.6
                label: "description"
                placeholder: "Rent, Salary, Netflix…"
            }
            Field {
                id: amountField
                width: (r1.width - 8) * 0.4
                label: "amount"
                numeric: true
                placeholder: "0.00"
                onEdited: value => root.validateAmount(value)
            }
        }

        // how often
        Flow {
            width: parent.width
            spacing: 6
            Repeater {
                model: [
                    { key: "monthly_day",  text: "monthly" },
                    { key: "payday",       text: "last working day" },
                    { key: "monthly_last", text: "last of month" },
                    { key: "weekly",       text: "weekly" },
                    { key: "fortnightly",  text: "fortnightly" },
                    { key: "four_weekly",  text: "4-weekly" },
                    { key: "yearly",       text: "yearly" },
                    { key: "custom",       text: "custom…" }
                ]
                PushButton {
                    required property var modelData
                    label: modelData.text
                    primary: root.preset === modelData.key
                    onClicked: root.preset = modelData.key
                }
            }
        }

        Row {
            spacing: 6
            width: parent.width
            visible: root.needsDay || root.needsWeekday

            Field {
                visible: root.needsDay
                width: 90
                label: "day"
                numeric: true
                text: String(root.dayOfMonth)
                onEdited: value => {
                    const n = parseInt(value);
                    if (n >= 1 && n <= 31)
                        root.dayOfMonth = n;
                }
            }
            Repeater {
                model: root.needsWeekday ? ["MO", "TU", "WE", "TH", "FR", "SA", "SU"] : []
                PushButton {
                    required property var modelData
                    label: modelData
                    primary: root.weekday === modelData
                    onClicked: root.weekday = modelData
                }
            }
        }

        Field {
            id: customRule
            visible: root.preset === "custom"
            width: parent.width
            label: "RRULE (RFC 5545)"
            numeric: true
            placeholder: "FREQ=MONTHLY;BYMONTHDAY=15"
            onEdited: root.refreshPreview()
        }

        Row {
            id: r2
            spacing: 8
            width: parent.width
            Field {
                id: startField
                width: 150
                label: "starting"
                numeric: true
                text: root.defaultDate
                onEdited: root.refreshPreview()
            }
            AccountPicker {
                width: (r2.width - 166) / 2
                label: "from"
                accounts: root.accounts
                onPicked: id => root.fromAccount = id
            }
            AccountPicker {
                width: (r2.width - 166) / 2
                label: "to"
                accounts: root.accounts
                onPicked: id => root.toAccount = id
            }
        }

        // if it lands on a weekend
        Row {
            spacing: 6
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "on a weekend:"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Repeater {
                model: [
                    { key: "none",           text: "leave it" },
                    { key: "before",         text: "move earlier" },
                    { key: "after",          text: "move later" },
                    { key: "modified_after", text: "later, same month" }
                ]
                PushButton {
                    required property var modelData
                    label: modelData.text
                    primary: root.weekendRule === modelData.key
                    onClicked: root.weekendRule = modelData.key
                }
            }
        }

        // The rule, and what it actually does.
        Rectangle {
            width: parent.width
            implicitHeight: 54
            color: Theme.surfaceRaised
            radius: 5
            border.width: 1
            border.color: Theme.line

            Column {
                anchors.fill: parent
                anchors.margins: 6
                spacing: 2
                Text {
                    text: root.rrule
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 3
                    elide: Text.ElideRight
                    width: parent.width
                }
                Text {
                    text: root.previewDates.length === 0
                          ? "no dates — check the rule"
                          : "next: " + root.previewDates.map(d =>
                                d.moved ? (d.value_on + "*") : d.value_on).join("   ")
                    color: root.previewDates.length === 0 ? Theme.warnAmber : Theme.text
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 2
                    elide: Text.ElideRight
                    width: parent.width
                }
                Text {
                    visible: root.previewDates.some(d => d.moved)
                    text: "* moved off a weekend"
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
                label: "Create"
                primary: true
                enabled: root.complete
                onClicked: root.save()
            }
            PushButton {
                label: "Cancel"
                onClicked: {
                    root.reset();
                    root.cancelled();
                }
            }
        }
    }
}
