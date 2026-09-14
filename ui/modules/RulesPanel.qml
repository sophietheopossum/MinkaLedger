pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Every recurring payment rule in the book, in one place, and the way into editing any of them.
//
// WHY THIS REPLACES THE OLD LIST. The recurring payments used to be listed only underneath the
// create form, four rows high, with a rename and an end date and nothing else: no amount you could
// change, no schedule, no account, no what-ifs, and no way to see what hung off a rule. This is the
// list first, the way the accounts sidebar is: open it to review, click a rule to change it, and
// "+ new" when the rule you want does not exist yet.
//
// WHAT A ROW SAYS, AND WHY IT SAYS SO MUCH. A rule is reviewed for whether it is still right, and
// that needs more than its name and amount: the route the money takes, the schedule in words and
// as written, the next payments and anything due, what has been recorded against it, the
// adjustments ahead, whether it runs forever, and whether a what-if cancels it. Everything the core
// reports is on the row or in its tooltip; nothing is trimmed for tidiness.
//
// ENDED IS NOT GONE. An ended rule stays listed (collapsed) because an end can be moved again, and
// because "did I cancel the gym" is exactly the question this panel exists to answer.
//
// CARD AND LOAN RULES are listed so a "Payment" line in UPCOMING can be traced to where it comes
// from. Nothing in the app creates or edits them yet, and the row says so.
Rectangle {
    id: root

    property string asOf: ""
    property var accounts: []

    signal newRequested
    signal done

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property var review: null
    property string filterKey: "all"
    property string search: ""
    property bool showEnded: false
    property var editing: null
    property var pending: null
    property int endingKey: -1
    property string savedNote: ""
    property string note: ""
    property var hoverRule: null
    property real tipX: 0
    property real tipY: 0

    function reload() {
        if (!root.visible)
            return;
        Ledger.request("series.review", { as_of: root.asOf }, (r, e) => root.reviewed(r, e));
    }
    function reviewed(r, e) {
        if (e) {
            root.note = e.message;
            return;
        }
        root.review = r;
        root.note = "";
        if (root.pending !== null) {
            const found = root.findRule(root.pending.seriesId);
            const on = root.pending.occurrenceOn;
            root.pending = null;
            if (found)
                root.openEditor(found, on);
            else
                root.note = "that payment's rule is not in the book any more";
        }
    }
    onVisibleChanged: {
        if (visible) {
            root.reload();
        } else {
            root.editing = null;
            root.endingKey = -1;
            root.hoverRule = null;
        }
    }
    Connections {
        target: Ledger
        function onRevisionChanged() { root.reload(); }
    }

    /// From UPCOMING: open the rule `seriesId` belongs to, as a change from `occurrenceOn`.
    function openRule(seriesId, occurrenceOn) {
        root.pending = { seriesId: seriesId, occurrenceOn: occurrenceOn || "" };
        root.reload();
    }
    function allRules() {
        if (!root.review)
            return [];
        let out = root.review.rules.slice();
        for (const w of root.review.what_if)
            out = out.concat(w.rules);
        return out;
    }
    function findRule(seriesId) {
        return root.allRules().find(r => r.parts.some(p => p.ids.indexOf(seriesId) >= 0)) || null;
    }
    function openEditor(rule, on) {
        root.hoverRule = null;
        root.endingKey = -1;
        root.editing = rule;
        editor.load(rule, on);
        flick.contentY = 0;
    }
    function backToList(note) {
        root.editing = null;
        root.savedNote = note;
        noteTimer.restart();
        root.reload();
    }
    Timer {
        id: noteTimer
        interval: 12000
        onTriggered: root.savedNote = ""
    }

    // ---- small helpers ----
    function dmy(iso) {
        if (!iso || iso.length !== 10)
            return iso || "";
        return parseInt(iso.substring(8, 10)) + "/" + parseInt(iso.substring(5, 7)) + "/" + iso.substring(0, 4);
    }
    function dm(iso) {
        return iso && iso.length === 10 ? parseInt(iso.substring(8, 10)) + "/" + parseInt(iso.substring(5, 7)) : "";
    }
    function dayBefore(iso) {
        const d = new Date(iso + "T12:00:00Z");
        d.setUTCDate(d.getUTCDate() - 1);
        return d.toISOString().substring(0, 10);
    }
    function weekendPhrase(key) {
        return key === "before" ? "weekends: move earlier"
             : key === "after" ? "weekends: move later"
             : key === "modified_after" ? "weekends: later, same month" : "";
    }
    function money(minor, currency) {
        return Money.format(Math.abs(minor), currency || "");
    }
    function routeLine(rule) {
        if (rule.shape !== "two_leg") {
            return rule.current.legs.map(hop => hop.map(l => l.name + " " + (l.amount_minor < 0 ? "−" : "+")
                                                             + root.money(l.amount_minor, rule.currency)).join(", ")).join(" | ");
        }
        const names = rule.current.route.map(a => a.name + (a.closed ? " (closed)" : ""));
        if (rule.current.hops.length === 1)
            return names.join(" → ");
        let s = names[0];
        for (let i = 0; i < rule.current.hops.length; i++)
            s += " → " + root.money(rule.current.hops[i].amount_minor, rule.currency) + " → " + names[i + 1];
        return s;
    }
    function badges(rule) {
        const out = [];
        if (rule.chain_len)
            out.push("⛓ " + rule.chain_len + " legs");
        if (rule.group === "not_started")
            out.push("starts " + root.dmy(rule.parts[0].dtstart));
        if (rule.parts.length > 1) {
            const prev = rule.parts[rule.parts.length - 2];
            const was = prev.amounts.map(a => root.money(a, rule.currency)).join(" / ");
            const now = rule.current.hops.map(h => root.money(h.amount_minor, rule.currency)).join(" / ");
            out.push((rule.current.dtstart > root.asOf ? "changes " : "changed ") + root.dmy(rule.current.dtstart)
                     + (was !== now ? ": " + was + " → " + now : ""));
        }
        if (rule.replaces)
            out.push("replaces " + rule.replaces.description);
        for (const c of rule.cancelled_in)
            out.push("cancelled in “" + c.scenario + "”");
        return out.join("  ·  ");
    }
    function historyLine(rule) {
        const bits = [];
        if (rule.next.length > 0)
            bits.push("next " + root.dmy(rule.next[0].value_on) + rule.next.slice(1, 4).map(n => " · " + root.dm(n.value_on)).join(""));
        else if (rule.group !== "ended")
            bits.push("nothing due in the next 12 months");
        if (rule.recorded.count > 0)
            bits.push(rule.recorded.count + " recorded, last " + root.dmy(rule.recorded.last.occurrence_on)
                      + (rule.recorded.ahead.length > 0 ? " (" + rule.recorded.ahead.length + " ahead)" : ""));
        if (rule.overrides.length > 0)
            bits.push(rule.overrides.length + " adjustment" + (rule.overrides.length === 1 ? "" : "s") + " ahead");
        return bits.join("  ·  ");
    }
    function warningLine(rule) {
        const bits = [];
        if (rule.due.length > 0)
            bits.push("due " + rule.due.map(d => root.dmy(d.value_on)).join(", "));
        for (const w of rule.warnings)
            bits.push(w.text);
        return bits.join("  ·  ");
    }
    function warningColor(rule) {
        return rule.warnings.some(w => w.level === "error") ? Theme.red : Theme.warnAmber;
    }
    // The sign a person reads: out of the household, into it, or moved between its own accounts.
    // Not the stored sign, which is the primary leg's and says which ACCOUNT the money left.
    function signOf(rule) {
        return rule.direction === "out" ? "−" : rule.direction === "in" ? "+"
             : rule.direction === "transfer" ? "" : rule.next_12m.monthly_minor < 0 ? "−" : "+";
    }
    function amountColor(rule) {
        return rule.direction === "out" ? Theme.red : rule.direction === "in" ? Theme.okGreen : Theme.text;
    }
    function endState(rule) {
        if (!rule.current.until_on)
            return "no end date";
        return (rule.group === "ended" ? "ended " : "ends ") + root.dmy(rule.current.until_on);
    }
    function kindPhrase(c) {
        const cur = c.currency || "";
        let s = c.amount_kind === "fixed" ? "fixed " + root.money(c.fixed_minor || 0, cur)
              : c.amount_kind === "pct_of_balance" ? c.pct + "% of the balance"
              : c.amount_kind === "pct_of_statement" ? c.pct + "% of the statement"
              : c.amount_kind === "interest_fees_plus_pct" ? "interest and fees + " + c.pct + "%"
              : c.amount_kind === "full_statement" ? "the full statement"
              : c.amount_kind === "amortising_level" ? "level " + root.money(c.level_payment_minor || 0, cur) + " over " + c.term_periods + " payments"
              : c.amount_kind;
        if (c.floor_minor !== null)
            s += " (at least " + root.money(c.floor_minor, cur) + ")";
        if (c.cap_minor !== null)
            s += " (at most " + root.money(c.cap_minor, cur) + ")";
        return s;
    }

    function matches(rule) {
        const f = root.filterKey;
        if (f === "no_end" && (rule.group === "ended" || rule.current.until_on || rule.scenario_id !== null))
            return false;
        if (f === "changed" && rule.parts.length < 2)
            return false;
        if (f === "ended" && rule.group !== "ended")
            return false;
        const q = root.search.trim().toLowerCase();
        if (q.length === 0)
            return true;
        const hay = (rule.description + " " + rule.current.phrase + " " + rule.current.rrule + " "
                     + rule.current.route.map(a => a.name).join(" ") + " "
                     + rule.current.legs.map(h => h.map(l => l.name).join(" ")).join(" ")).toLowerCase();
        return hay.indexOf(q) >= 0;
    }
    function countOf(key) {
        if (!root.review)
            return 0;
        const t = root.review.totals;
        return key === "no_end" ? t.no_end : key === "changed" ? t.changed : key === "ended" ? t.ended
             : key === "what_if" ? t.what_if + t.cancels : 0;
    }

    readonly property var rows: root.buildRows()
    function buildRows() {
        const out = [];
        const rv = root.review;
        if (!rv)
            return out;
        const f = root.filterKey;
        if (f !== "what_if") {
            const live = rv.rules.filter(r => r.group !== "ended" && root.matches(r));
            const ended = rv.rules.filter(r => r.group === "ended" && root.matches(r));
            if (f !== "ended") {
                out.push({ type: "header", key: "h-live", text: "RUNNING AND NOT YET STARTED · " + live.length, toggle: false });
                for (const r of live)
                    out.push({ type: "rule", key: "r" + r.lineage_id, rule: r });
            }
            if (ended.length > 0) {
                const open = root.showEnded || f === "ended";
                out.push({ type: "header", key: "h-ended", text: "ENDED · " + ended.length + (f === "ended" ? "" : open ? "  ▾" : "  ▸"), toggle: f !== "ended" });
                if (open)
                    for (const r of ended)
                        out.push({ type: "rule", key: "r" + r.lineage_id, rule: r });
            }
        }
        if (f === "all" || f === "what_if") {
            for (const w of rv.what_if) {
                const rules = w.rules.filter(r => root.matches(r));
                out.push({ type: "header", key: "h-w" + w.scenario_id, text: "WHAT-IF · " + w.name, toggle: false, purple: true });
                for (const r of rules)
                    out.push({ type: "rule", key: "w" + r.lineage_id, rule: r });
                for (const c of w.cancels)
                    out.push({ type: "cancel", key: "c" + c.id, cancel: c });
            }
        }
        if (f === "all" && (rv.card_rules.length > 0 || rv.interest_rules.length > 0)) {
            out.push({ type: "header", key: "h-card", text: "CARD AND LOAN RULES · read-only", toggle: false });
            for (const c of rv.card_rules)
                out.push({ type: "card", key: "p" + c.id, card: c });
            for (const i of rv.interest_rules)
                out.push({ type: "interest", key: "i" + i.id, interest: i });
        }
        return out;
    }

    function summary() {
        const rv = root.review;
        if (!rv)
            return "";
        const t = rv.totals;
        const bits = [t.running + " running"];
        if (t.not_started > 0) bits.push(t.not_started + " not started");
        if (t.ended > 0) bits.push(t.ended + " ended");
        if (t.what_if > 0) bits.push(t.what_if + " what-if");
        if (t.card_rules + t.interest_rules > 0) bits.push((t.card_rules + t.interest_rules) + " card or loan rule" + (t.card_rules + t.interest_rules === 1 ? "" : "s"));
        return bits.join(" · ");
    }
    function monthlyLine() {
        const rv = root.review;
        if (!rv || rv.monthly_by_currency.length === 0)
            return "";
        return "≈ " + rv.monthly_by_currency.map(m => "−" + root.money(m.out_minor, m.currency) + " out · +"
                                                     + root.money(m.in_minor, m.currency) + " in " + m.currency).join("   ")
               + " a month, planned over the next 12 months";
    }

    // In a function of its own rather than the handler: a const inside a nested closure blinds the
    // linter over the whole file.
    function hovered(rule, area, mouse) {
        if (!rule)
            return;
        root.hoverRule = rule;
        const p = area.mapToItem(root, mouse.x, mouse.y);
        root.tipX = p.x + 14;
        root.tipY = p.y + 18;
    }

    // `clear` is the only way to send an end with no date, which REMOVES the end: a half-typed date
    // must never be read as that.
    function endRule(rule, until, clear) {
        const params = { id: rule.current.ids[0] };
        if (!clear) {
            if (!until || until.length !== 10)
                return;
            params.until_on = until;
        }
        Ledger.write("series.end", params, (r, e) => root.ended(rule, clear ? "" : until, e));
    }
    function ended(rule, until, e) {
        if (e) {
            root.note = e.message;
            return;
        }
        root.endingKey = -1;
        root.note = "";
        root.savedNote = rule.description + (until && until.length === 10 ? ": ends " + root.dmy(until) + " — nothing is paid after that" : ": no end date");
        noteTimer.restart();
    }

    // ================= the list =================
    Column {
        id: listView
        anchors.fill: parent
        anchors.margins: 10
        spacing: 6
        visible: root.editing === null

        Row {
            width: parent.width
            spacing: 10
            Column {
                width: parent.width - searchField.width - newButton.width - closeButton.width - 30
                spacing: 1
                Row {
                    spacing: 10
                    Text {
                        text: "RECURRING PAYMENTS"
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        text: root.summary()
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                }
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    text: root.monthlyLine()
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
            Field {
                id: searchField
                width: 190
                label: "search"
                placeholder: "name, account, schedule"
                onEdited: value => root.search = value
            }
            PushButton {
                id: newButton
                anchors.verticalCenter: parent.verticalCenter
                label: "+ new"
                primary: root.review !== null && root.review.rules.length === 0
                onClicked: root.newRequested()
            }
            PushButton {
                id: closeButton
                anchors.verticalCenter: parent.verticalCenter
                label: "Close"
                onClicked: root.done()
            }
        }

        Row {
            spacing: 6
            Repeater {
                model: [
                    { key: "all", text: "all" },
                    { key: "no_end", text: "no end date" },
                    { key: "changed", text: "changed" },
                    { key: "ended", text: "ended" },
                    { key: "what_if", text: "what-if" }
                ]
                PushButton {
                    required property var modelData
                    implicitHeight: 24
                    label: modelData.text + (modelData.key === "all" ? "" : " " + root.countOf(modelData.key))
                    primary: root.filterKey === modelData.key
                    onClicked: root.filterKey = modelData.key
                }
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.savedNote.length > 0
            text: root.savedNote
            color: Theme.okGreen
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
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
        Text {
            visible: root.review !== null && root.review.rules.length === 0 && root.review.what_if.length === 0
            text: "no recurring payments yet — “+ new” makes one"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        ListView {
            id: list
            width: parent.width
            height: listView.height - y
            clip: true
            spacing: 3
            boundsBehavior: Flickable.StopAtBounds
            model: root.rows

            delegate: Item {
                id: row
                required property var modelData
                width: ListView.view.width
                readonly property bool isRule: row.modelData.type === "rule"
                readonly property var rule: row.isRule ? row.modelData.rule : null
                readonly property bool ending: row.isRule && root.endingKey === row.rule.lineage_id
                implicitHeight: row.modelData.type === "header" ? 22
                              : row.modelData.type === "cancel" ? 22
                              : row.isRule ? (row.ending ? 110 : 60)
                              : cardCol.implicitHeight + 10

                // ---- a group header ----
                Text {
                    visible: row.modelData.type === "header"
                    anchors.bottom: parent.bottom
                    text: row.modelData.text || ""
                    color: row.modelData.purple ? Theme.purple : Theme.textMuted
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 3
                    MouseArea {
                        anchors.fill: parent
                        enabled: row.modelData.toggle === true
                        cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
                        onClicked: root.showEnded = !root.showEnded
                    }
                }

                // ---- a what-if cancel ----
                Text {
                    visible: row.modelData.type === "cancel"
                    anchors.verticalCenter: parent.verticalCenter
                    x: 8
                    width: parent.width - 16
                    elide: Text.ElideRight
                    text: row.modelData.type === "cancel"
                          ? "− cancels " + row.modelData.cancel.target
                            + (row.modelData.cancel.chain_len ? " (the whole chain of " + row.modelData.cancel.chain_len + ")" : "")
                            + (row.modelData.cancel.phrase ? " · " + row.modelData.cancel.phrase : "")
                            + " · undone by deleting the what-if"
                          : ""
                    color: Theme.purple
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 3
                }

                // ---- a card or loan rule ----
                Column {
                    id: cardCol
                    visible: row.modelData.type === "card" || row.modelData.type === "interest"
                    x: 8
                    width: parent.width - 16
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 1
                    Text {
                        width: parent.width
                        wrapMode: Text.Wrap
                        text: row.modelData.type === "card"
                              ? "Payment · " + row.modelData.card.from_account + " → " + row.modelData.card.account + " · "
                                + root.kindPhrase(row.modelData.card) + " · " + row.modelData.card.phrase
                                + " · from " + root.dmy(row.modelData.card.dtstart)
                                + (row.modelData.card.until_on ? " · ends " + root.dmy(row.modelData.card.until_on) : "")
                                + (row.modelData.card.due_offset_days !== null ? " · due " + row.modelData.card.due_offset_days + " days after the statement" : "")
                                + (row.modelData.card.scenario ? " · what-if “" + row.modelData.card.scenario + "”" : "")
                              : row.modelData.type === "interest"
                                ? "Interest · " + row.modelData.interest.account + " · " + row.modelData.interest.shape
                                  + (row.modelData.interest.rate_in_force ? ", " + row.modelData.interest.rate_in_force.quoted + "% "
                                     + row.modelData.interest.rate_in_force.basis : "")
                                  + ", " + row.modelData.interest.accrual_freq
                                  + (row.modelData.interest.grace_period ? ", grace period" : "")
                                  + " · statements " + row.modelData.interest.phrase + " from " + root.dmy(row.modelData.interest.capitalise_dtstart)
                                  + " · charged to " + row.modelData.interest.counter_account
                                  + " · accrues on a " + (row.modelData.interest.accrues_on === "negative" ? "debt (negative balance)" : "positive balance")
                                  + (row.modelData.interest.scenario ? " · what-if “" + row.modelData.interest.scenario + "”" : "")
                                : ""
                        color: Theme.text
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Text {
                        width: parent.width
                        elide: Text.ElideRight
                        text: row.modelData.type === "card"
                              ? (row.modelData.card.rule_error ? row.modelData.card.rule_error : "read-only: nothing in the app edits card and loan rules yet")
                              : row.modelData.type === "interest"
                                ? (row.modelData.interest.warning ? row.modelData.interest.warning : "read-only: nothing in the app edits interest rules yet")
                                : ""
                        color: (row.modelData.type === "card" && row.modelData.card.rule_error) || (row.modelData.type === "interest" && row.modelData.interest.warning)
                               ? Theme.warnAmber : Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                }

                // ---- a rule ----
                Rectangle {
                    visible: row.isRule
                    anchors.fill: parent
                    radius: 4
                    color: hover.containsMouse || row.ending ? Theme.surfaceRaised : "transparent"
                    border.width: row.isRule && row.rule.scenario_id !== null ? 1 : 0
                    border.color: Theme.purpleDim

                    MouseArea {
                        id: hover
                        anchors.fill: parent
                        // Off while the end chooser is open, so a click on its gaps, its caption or its
                        // greyed button does not open the editor and throw the typed date away.
                        enabled: !row.ending
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: if (row.isRule) root.openEditor(row.rule, "")
                        onPositionChanged: mouse => root.hovered(row.isRule ? row.rule : null, hover, mouse)
                        onExited: if (root.hoverRule === row.rule) root.hoverRule = null
                    }

                    Column {
                        x: 8
                        y: 6
                        width: parent.width - amountCol.width - actions.width - 32
                        spacing: 1
                        Row {
                            width: parent.width
                            spacing: 8
                            Text {
                                id: nameText
                                text: row.isRule ? row.rule.description : ""
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                width: parent.width - nameText.width - 8
                                elide: Text.ElideRight
                                anchors.verticalCenter: nameText.verticalCenter
                                text: row.isRule ? root.badges(row.rule) + (row.rule.shape === "custom" ? "  ·  custom legs" : "") : ""
                                color: row.isRule && row.rule.shape === "custom" ? Theme.warnAmber : Theme.purple
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                        Text {
                            width: parent.width
                            elide: Text.ElideRight
                            text: row.isRule
                                  ? root.routeLine(row.rule) + "  ·  " + row.rule.current.phrase
                                    + (row.rule.current.phrase !== row.rule.current.rrule ? "  (" + row.rule.current.rrule + ")" : "")
                                    + (root.weekendPhrase(row.rule.current.weekend_rule).length > 0 ? "  ·  " + root.weekendPhrase(row.rule.current.weekend_rule) : "")
                                    + (row.rule.parts[0].dtstart > root.asOf ? "  ·  starts " : "  ·  since ") + root.dmy(row.rule.parts[0].dtstart)
                                  : ""
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Row {
                            width: parent.width
                            spacing: 8
                            Text {
                                id: historyText
                                text: row.isRule ? root.historyLine(row.rule) : ""
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Text {
                                width: parent.width - historyText.width - 8
                                elide: Text.ElideRight
                                text: row.isRule ? root.warningLine(row.rule) : ""
                                color: row.isRule ? root.warningColor(row.rule) : Theme.warnAmber
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                        }
                    }

                    Column {
                        id: amountCol
                        anchors.right: actions.left
                        anchors.rightMargin: 10
                        y: 6
                        width: 150
                        spacing: 1
                        Text {
                            width: parent.width
                            horizontalAlignment: Text.AlignRight
                            text: row.isRule && row.rule.current.hops.length > 0
                                  ? root.money(row.rule.current.hops[0].amount_minor, row.rule.currency) + " " + (row.rule.currency || "")
                                  : ""
                            color: row.isRule ? root.amountColor(row.rule) : Theme.text
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 1
                        }
                        Text {
                            width: parent.width
                            horizontalAlignment: Text.AlignRight
                            visible: row.isRule && row.rule.next_12m.count > 0
                            text: row.isRule ? "≈ " + root.signOf(row.rule)
                                               + root.money(row.rule.next_12m.monthly_minor, row.rule.currency) + "/mo" : ""
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Text {
                            width: parent.width
                            horizontalAlignment: Text.AlignRight
                            text: row.isRule ? root.endState(row.rule) : ""
                            color: row.isRule && !row.rule.current.until_on ? Theme.warnAmber
                                 : row.isRule && row.rule.group === "ended" ? Theme.textFaint : Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                    }

                    Column {
                        id: actions
                        anchors.right: parent.right
                        anchors.rightMargin: 8
                        y: 6
                        width: 60
                        spacing: 4
                        PushButton {
                            width: parent.width
                            implicitHeight: 22
                            label: "edit"
                            onClicked: if (row.isRule) root.openEditor(row.rule, "")
                        }
                        PushButton {
                            width: parent.width
                            implicitHeight: 22
                            label: row.ending ? "×" : "end"
                            onClicked: {
                                if (!row.isRule)
                                    return;
                                if (row.ending) {
                                    root.endingKey = -1;
                                } else {
                                    root.endingKey = row.rule.lineage_id;
                                    endField.text = row.rule.current.until_on || root.asOf;
                                }
                            }
                        }
                    }

                    // ---- the end chooser ----
                    Row {
                        id: endRow
                        visible: row.ending
                        x: 8
                        y: 60
                        spacing: 6
                        readonly property string beforeNext: row.isRule && row.rule.next.length > 0
                                                             ? root.dayBefore(row.rule.next[0].occurrence_on) : ""
                        PushButton {
                            id: beforeNextButton
                            anchors.verticalCenter: parent.verticalCenter
                            implicitHeight: 26
                            visible: endRow.beforeNext.length === 10 && row.isRule && endRow.beforeNext >= row.rule.current.dtstart
                            label: "before the next payment (" + root.dmy(endRow.beforeNext) + ")"
                            onClicked: root.endRule(row.rule, endRow.beforeNext, false)
                        }
                        Field {
                            id: endField
                            width: 130
                            label: "last payment"
                            numeric: true
                            placeholder: "YYYY-MM-DD"
                            onAccepted: if (endField.text.length === 10) root.endRule(row.rule, endField.text, false)
                        }
                        PushButton {
                            anchors.verticalCenter: parent.verticalCenter
                            implicitHeight: 26
                            primary: true
                            label: "end on that date"
                            enabled: endField.text.length === 10
                            onClicked: root.endRule(row.rule, endField.text, false)
                        }
                        PushButton {
                            anchors.verticalCenter: parent.verticalCenter
                            implicitHeight: 26
                            visible: row.isRule && !!row.rule.current.until_on
                            label: "no end"
                            onClicked: root.endRule(row.rule, "", true)
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            visible: row.isRule && row.rule.chain_len > 1
                            text: row.isRule && row.rule.chain_len ? "ends all " + row.rule.chain_len + " legs together" : ""
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                    }
                }
            }
        }
    }

    // ---- everything about the hovered rule, untrimmed ----
    Rectangle {
        id: tip
        visible: root.hoverRule !== null && root.editing === null && root.endingKey < 0
        x: Math.max(4, Math.min(root.tipX, root.width - width - 4))
        y: root.tipY + height > root.height - 4 ? Math.max(4, root.tipY - height - 30) : root.tipY
        z: 10
        width: Math.min(root.width - 8, tipCol.implicitWidth + 16)
        height: tipCol.implicitHeight + 12
        radius: 4
        color: Theme.surfaceRaised
        border.width: 1
        border.color: Theme.line
        Column {
            id: tipCol
            x: 8
            y: 6
            spacing: 2
            Repeater {
                model: root.hoverRule ? root.tipLines(root.hoverRule) : []
                Text {
                    required property var modelData
                    text: modelData.text
                    color: modelData.color
                    font.family: modelData.mono ? Theme.monoFamily : Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
        }
    }
    function tipLines(rule) {
        const out = [];
        const add = (text, color, mono) => out.push({ text: text, color: color || Theme.text, mono: mono === true });
        add(rule.description + (rule.scenario ? "  —  what-if “" + rule.scenario + "”" : ""), Theme.text);
        add(rule.current.rrule + "   (" + rule.current.phrase + ")", Theme.textFaint, true);
        add("rule " + rule.lineage_id + " · series " + rule.parts.map(p => p.ids.join("+")).join(", "), Theme.textFaint, true);
        if (rule.parts.length > 1) {
            add("PARTS", Theme.textMuted);
            for (const p of rule.parts)
                add(root.dmy(p.dtstart) + " → " + (p.until_on ? root.dmy(p.until_on) : "no end") + "  ·  "
                    + p.amounts.map(a => root.money(a, rule.currency)).join(" / ") + "  ·  " + p.phrase
                    + "  ·  " + p.route_names.join(" → ") + "  ·  " + p.recorded + " recorded"
                    + (p.left_behind.length > 0 ? "  ·  left behind: " + p.left_behind.map(l => l.word + " " + root.dmy(l.occurrence_on)).join(", ") : ""),
                    Theme.text, true);
        }
        if (rule.next.length > 0)
            add("next: " + rule.next.map(n => root.dmy(n.value_on) + (n.moved ? "*" : "") + " " + root.money(n.amount_minor, rule.currency)
                                           + (n.amended ? " (own amount)" : "")).join("   "), Theme.text, true);
        if (rule.next_12m.count > 0)
            add("next 12 months: " + rule.next_12m.count + " payments, " + root.signOf(rule)
                + root.money(rule.next_12m.total_minor, rule.currency) + " " + (rule.currency || ""), Theme.textFaint, true);
        if (rule.overrides.length > 0) {
            add("ADJUSTMENTS AHEAD", Theme.textMuted);
            for (const o of rule.overrides)
                add(root.dmy(o.occurrence_on) + "  " + o.word + (o.moved_to ? " → " + root.dmy(o.moved_to) : "")
                    + (o.amounts.some(a => a !== null) ? "  " + o.amounts.map(a => a === null ? "rule" : root.money(a, rule.currency)).join(" / ") : "")
                    + (o.description ? "  “" + o.description + "”" : ""), Theme.text, true);
        }
        if (rule.recorded.count > 0)
            add("recorded: " + rule.recorded.count + ", last " + root.dmy(rule.recorded.last.occurrence_on)
                + " paid " + root.dmy(rule.recorded.last.occurred_on)
                + (rule.recorded.last.amount_minor !== null ? " " + root.money(rule.recorded.last.amount_minor, rule.currency) : ""),
                Theme.textFaint, true);
        for (const a of rule.recorded.ahead)
            add("recorded ahead: " + root.dmy(a.occurrence_on) + " (paid " + root.dmy(a.occurred_on) + ")", Theme.textFaint, true);
        for (const s of rule.stray)
            add("stray " + s.kind + ": " + root.dmy(s.occurrence_on) + " is not a date the rule produces", Theme.warnAmber, true);
        for (const c of rule.cancelled_in)
            add("cancelled in what-if “" + c.scenario + "”", Theme.purple);
        for (const w of rule.warnings)
            add(w.text, w.level === "error" ? Theme.red : Theme.warnAmber);
        return out;
    }

    // ================= the editor =================
    Column {
        anchors.fill: parent
        anchors.margins: 10
        spacing: 6
        visible: root.editing !== null

        Column {
            id: editorHeader
            width: parent.width
            spacing: 2
            Row {
                spacing: 10
                PushButton {
                    implicitHeight: 24
                    label: "← all rules"
                    onClicked: root.backToList("")
                }
                Text {
                    anchors.verticalCenter: parent.verticalCenter
                    width: root.width - 140
                    wrapMode: Text.Wrap
                    text: root.editing ? root.editing.description
                                         + (root.editing.parts[0].dtstart > root.asOf ? "  ·  starts " : "  ·  started ") + root.dmy(root.editing.parts[0].dtstart)
                                         + (root.editing.recorded.count > 0 ? "  ·  " + root.editing.recorded.count + " recorded, last "
                                            + root.dmy(root.editing.recorded.last.occurrence_on) : "")
                                         + (root.editing.overrides.length > 0 ? "  ·  " + root.editing.overrides.length
                                            + (root.editing.overrides.length === 1 ? " adjustment ahead" : " adjustments ahead") : "")
                                         + (root.editing.cancelled_in.length > 0 ? "  ·  cancelled in "
                                            + root.editing.cancelled_in.map(c => "“" + c.scenario + "”").join(", ") : "")
                                       : ""
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 3
                }
            }
            // The terms the rule had before each change, which the form below does not show.
            Repeater {
                model: root.editing ? root.editing.parts.slice(0, root.editing.parts.length - 1) : []
                Text {
                    required property var modelData
                    width: editorHeader.width
                    wrapMode: Text.Wrap
                    text: root.editing ? "earlier: " + root.dmy(modelData.dtstart) + " → " + (modelData.until_on ? root.dmy(modelData.until_on) : "no end")
                                         + "  ·  " + modelData.amounts.map(a => root.money(a, root.editing.currency)).join(" / ")
                                         + "  ·  " + modelData.phrase
                                         + (modelData.route_names.length > 0 ? "  ·  " + modelData.route_names.join(" → ") : "")
                                         + (root.weekendPhrase(modelData.weekend_rule).length > 0 ? "  ·  " + root.weekendPhrase(modelData.weekend_rule) : "")
                                         + "  ·  " + modelData.recorded + " recorded"
                                         + (modelData.left_behind.length > 0 ? "  ·  left behind: "
                                            + modelData.left_behind.map(l => l.word + " " + root.dmy(l.occurrence_on)).join(", ") : "")
                                       : ""
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
        }

        Flickable {
            id: flick
            width: parent.width
            height: parent.height - editorHeader.height - 6
            clip: true
            contentHeight: editor.implicitHeight
            boundsBehavior: Flickable.StopAtBounds
            RuleEditor {
                id: editor
                width: flick.width - 10
                asOf: root.asOf
                accounts: root.editorAccounts
                onSaved: note => root.backToList(note)
                onCancelled: root.backToList("")
            }
        }
    }
    // The open accounts, plus any closed one the rule already uses, so its picker still shows it.
    readonly property var editorAccounts: {
        const list = (root.accounts || []).slice();
        if (root.editing && root.editing.shape === "two_leg") {
            for (const a of root.editing.current.route)
                if (!list.some(x => x.account_id === a.account_id))
                    list.push({ account_id: a.account_id, name: a.name + (a.closed ? " (closed)" : ""), kind: a.kind, currency: a.currency });
        }
        return list;
    }
    Rectangle {
        visible: root.editing !== null && flick.contentHeight > flick.height
        anchors.right: parent.right
        anchors.rightMargin: 4
        y: flick.y + 10 + flick.visibleArea.yPosition * flick.height
        width: 4
        height: Math.max(20, flick.visibleArea.heightRatio * flick.height)
        radius: 2
        color: Theme.line
    }
}
