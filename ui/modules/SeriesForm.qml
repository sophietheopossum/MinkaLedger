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
//
// A CHAIN, as in the payment form: stops the money passes through on its way, each becoming its own
// recurring payment on the same rule, sharing the description, tied together so ending, renaming or
// skipping any one of them does it to all. Only the amount is per leg -- a leg is bound to the
// rule's date, so "lands a day later" is an override on the occurrence, not a field here.
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
    // Content-driven for the same reason as EntryForm: the form GROWS when "ends" switches to a
    // date or a count, and a fixed height clips its own Create button.
    implicitHeight: form.implicitHeight + 24

    property int fromAccount: -1
    property int toAccount: -1
    // The stops the money passes THROUGH, in order: { account, amountText, amountMinor, amountOk,
    // amountBad }, `amountText` blank meaning "same as the leg above". Same split as the payment
    // form: `stopCount` is the Repeater's model and rebuilds the rows, `stops` is what they show.
    property var stops: []
    property int stopCount: 0
    property int amountMinor: 0
    property bool amountOk: false
    property string preset: "monthly_day"
    property int dayOfMonth: 1
    property string weekday: "MO"
    property string weekendRule: "none"
    property var previewDates: []
    // "never" | "on" | "after". Most recurring payments do not recur forever, and a rule with no
    // bound quietly projects one into every forecast you ever draw.
    property string endMode: "never"
    property int endCount: 12
    property string computedUntil: ""

    function currencyOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.currency : "";
    }
    // The series is denominated in the account the money leaves.
    readonly property string fromCurrency: root.currencyOf(root.fromAccount)

    // Every account the money visits, in order.
    readonly property var route: [root.fromAccount].concat(root.stops.map(s => s.account), [root.toAccount])
    readonly property bool routeChosen: root.route.every(id => id >= 0)
    readonly property bool routeMoves: root.route.every((id, i) => i === 0 || id !== root.route[i - 1])
    readonly property bool routeOneCurrency: root.route.every(id => id < 0 || root.currencyOf(id) === root.fromCurrency)
    readonly property bool stopsFilledIn: root.stops.every(s => s.amountOk)
    readonly property var passThroughAccounts: (root.accounts || []).filter(a => a.kind === "asset" || a.kind === "liability")

    function nameOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.name : "…";
    }
    readonly property string routeText: root.route.map(id => root.nameOf(id)).join(" → ")

    function addStop() {
        root.stops = root.stops.concat([{ account: -1, amountText: "", amountMinor: 0, amountOk: true, amountBad: false }]);
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
        root.stopCount = s.length;
        root.stops = s;
        status.text = "";
    }
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
                return;
            if (e) {
                status.text = e.message;
                root.updateStop(i, { amountBad: true });
            } else {
                status.text = r.minor > 0 ? "" : "an amount is always positive";
                root.updateStop(i, { amountMinor: r.minor, amountOk: r.minor > 0, amountBad: r.minor <= 0 });
            }
        });
    }
    onFromCurrencyChanged: {
        root.stops.forEach((s, i) => {
            if (s.amountText.trim().length > 0)
                root.validateStopAmount(i, s.amountText);
        });
    }
    // One hop per leg: the headline amount for the first, then each stop's onward amount or the
    // same again.
    function chainHops() {
        let amount = root.amountMinor;
        const targets = root.stops.map(s => s.account).concat([root.toAccount]);
        const hops = [];
        for (let i = 0; i < targets.length; i++) {
            if (i > 0 && root.stops[i - 1].amountText.trim().length > 0)
                amount = root.stops[i - 1].amountMinor;
            hops.push({ to_account: targets[i], amount_minor: amount });
        }
        return hops;
    }

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
        fromPicker.selected = -1;
        toPicker.selected = -1;
        root.stops = [];
        root.stopCount = 0;
        root.previewDates = [];
        root.endMode = "never";
        root.computedUntil = "";
        endField.clear();
        status.text = "";
    }

    function validateAmount(text) {
        if (text.trim().length === 0) {
            root.amountOk = false;
            return;
        }
        Ledger.request("money.parse",
                       { text: text, minor_digits: Money.digits(root.fromCurrency) }, (r, e) => {
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

    onRruleChanged: { root.refreshPreview(); root.refreshEnd(); }
    onWeekendRuleChanged: root.refreshPreview()
    onEndModeChanged: root.refreshEnd()
    onEndCountChanged: root.refreshEnd()

    // "after N payments" is answered by EXPANDING the rule and taking the Nth slot, then storing
    // that as a date. RFC 5545 COUNT is banned in the schema because it counts occurrences BEFORE
    // EXDATE removal -- so skipping one instalment of "12 payments" silently yields 11. Asking the
    // question by count is natural; storing the answer as a count is not.
    function refreshEnd() {
        if (root.endMode !== "after" || root.rrule.length === 0
            || startField.text.length !== 10) {
            root.computedUntil = "";
            return;
        }
        Ledger.request("series.preview", {
            rrule: root.rrule, dtstart: startField.text,
            count: root.endCount, weekend_rule: "none"
        }, (r, e) => {
            if (e || !r.dates || r.dates.length === 0) {
                root.computedUntil = "";
                return;
            }
            // The SLOT, not the weekend-adjusted value date: until_on bounds the expansion, which
            // happens before any adjustment.
            root.computedUntil = r.dates[r.dates.length - 1].occurrence_on;
        });
    }

    // Empty means unbounded, which is what the core wants for "no end".
    readonly property string endsOn: {
        if (root.endMode === "on")
            return endField.text.length === 10 ? endField.text : "";
        if (root.endMode === "after")
            return root.computedUntil;
        return "";
    }

    readonly property bool complete: root.amountOk && root.routeChosen && root.routeMoves
                                     && (root.stopCount > 0 || root.fromAccount !== root.toAccount)
                                     && (root.stopCount === 0 || (root.routeOneCurrency && root.stopsFilledIn))
                                     && descField.text.length > 0 && root.previewDates.length > 0

    function save() {
        if (!root.complete)
            return;
        if (root.stopCount > 0) {
            const chain = {
                description: descField.text,
                rrule: root.rrule,
                dtstart: startField.text,
                weekend_rule: root.weekendRule,
                from_account: root.fromAccount,
                hops: root.chainHops()
            };
            if (root.scenarioId >= 0)
                chain.scenario_id = root.scenarioId;
            if (root.endsOn.length === 10)
                chain.until_on = root.endsOn;
            Ledger.write("series.create_chain", chain, (r, e) => {
                if (e)
                    status.text = e.message;
                else {
                    root.reset();
                    root.saved();
                }
            });
            return;
        }
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
        if (root.endsOn.length === 10)
            params.until_on = root.endsOn;
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
        id: form
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 12
        spacing: 8

        Text {
            text: root.scenarioId < 0
                  ? (root.stopCount > 0 ? "NEW RECURRING CHAIN" : "NEW RECURRING PAYMENT")
                  : "HYPOTHETICAL " + (root.stopCount > 0 ? "CHAIN" : "PAYMENT")
                    + " — only in “" + root.scenarioName + "”"
            color: root.scenarioId < 0 ? Theme.textMuted : Theme.purple
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            id: r1
            spacing: 8
            width: parent.width
            // The same suggestion list the one-off form has, and if anything it belongs here more:
            // these are the descriptions that repeat by definition. A drop-in for the Field it
            // replaces -- text, clear(), label and placeholder are the whole of what this form
            // asks of it.
            DescriptionPicker {
                id: descField
                width: (r1.width - 8) * 0.6
                label: "description"
                placeholder: "Rent, Salary, Netflix…"
            }
            Field {
                id: amountField
                width: (r1.width - 8) * 0.4
                label: root.fromCurrency.length > 0
                       ? "amount (" + root.fromCurrency + ")" : "amount"
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
                id: fromPicker
                width: (r2.width - 150 - addStopButton.width - 24) / 2
                label: "from"
                accounts: root.accounts
                onPicked: id => root.fromAccount = id
            }
            AccountPicker {
                id: toPicker
                width: (r2.width - 150 - addStopButton.width - 24) / 2
                label: "to"
                accounts: root.accounts
                onPicked: id => root.toAccount = id
            }
            // In this row rather than on one of its own: this form is already the tallest panel
            // and the plain case must not pay a line for the chain it is not making.
            PushButton {
                id: addStopButton
                anchors.verticalCenter: parent.verticalCenter
                label: "+ stop"
                onClicked: root.addStop()
            }
        }

        // One row per stop, rebuilt from `stops` when the count changes.
        Repeater {
            model: root.stopCount
            delegate: Row {
                id: stopRow
                required property int index
                readonly property var stop: root.stops[stopRow.index]
                                            ?? ({ account: -1, amountText: "", amountMinor: 0, amountOk: true, amountBad: false })
                width: form.width
                spacing: 8
                AccountPicker {
                    width: (stopRow.width - 16) * 0.55
                    label: "via"
                    accounts: root.passThroughAccounts
                    selected: stopRow.stop.account
                    onPicked: id => root.updateStop(stopRow.index, { account: id })
                }
                Field {
                    width: (stopRow.width - 16) * 0.30
                    label: root.fromCurrency.length > 0 ? "then sends (" + root.fromCurrency + ")" : "then sends"
                    placeholder: "same amount"
                    numeric: true
                    text: stopRow.stop.amountText
                    onEdited: value => root.validateStopAmount(stopRow.index, value)
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

        Column {
            width: parent.width
            visible: root.stopCount > 0
            spacing: 1
            Text {
                width: parent.width
                elide: Text.ElideRight
                text: root.routeText
                color: Theme.text
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                width: parent.width
                elide: Text.ElideRight
                text: (root.stopCount + 1) + " recurring payments, one for each leg, sharing the description and the rule"
                      + " — every leg lands on the rule's date; a blank amount on a stop reuses the one above"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 4
            }
            Text {
                width: parent.width
                elide: Text.ElideRight
                visible: root.routeChosen && root.fromAccount === root.toAccount
                text: "a round trip: leaves and comes back to " + root.nameOf(root.fromAccount)
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 4
            }
        }

        // when it stops
        Row {
            spacing: 6
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "ends:"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Repeater {
                model: [
                    { key: "never", text: "never" },
                    { key: "on",    text: "on a date" },
                    { key: "after", text: "after N payments" }
                ]
                PushButton {
                    required property var modelData
                    label: modelData.text
                    primary: root.endMode === modelData.key
                    onClicked: root.endMode = modelData.key
                }
            }
            Field {
                visible: root.endMode === "on"
                id: endField
                width: 130
                label: "last payment"
                numeric: true
                placeholder: "YYYY-MM-DD"
            }
            Field {
                visible: root.endMode === "after"
                width: 70
                label: "payments"
                numeric: true
                text: String(root.endCount)
                onEdited: value => {
                    const n = parseInt(value);
                    if (n >= 1 && n <= 600)
                        root.endCount = n;
                }
            }
            Text {
                visible: root.endMode === "after"
                anchors.verticalCenter: parent.verticalCenter
                text: root.computedUntil.length > 0
                      ? "last one falls on " + root.computedUntil
                      : "…"
                color: root.computedUntil.length > 0 ? Theme.okGreen : Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
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
                label: root.stopCount > 0 ? "Create " + (root.stopCount + 1) + " payments" : "Create"
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
                text: "a chain stays in one currency"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }
    }
}
