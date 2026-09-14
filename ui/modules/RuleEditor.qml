pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Edit an existing recurring payment.
//
// A SEPARATE FORM FROM THE ONE THAT CREATES. SeriesForm makes new rules and is left exactly as it
// was; this one opens on a rule that already has a history, and the questions it has to answer are
// different ones: does the change correct the rule for good, or apply from a date so the months
// already behind it keep what they were; what happens to the months already recorded as paid, to
// the skips and moves already made, and to what the forecast says next.
//
// THE CORE DECIDES, THE FORM ASKS. Every change is sent to `series.revise` as a dry run a moment
// after the typing stops, and RuleImpact shows the answer: the core makes the change, reads the
// book as it would be, and rolls it back. So what is shown is what a save will do, including the
// things that stand in its way -- a payment recorded under the old schedule, an adjustment the new
// schedule has no date for, a month that would suddenly fall due. A save that removes or starts
// anything asks twice.
//
// ONLY THE SHAPE THE CREATE FORM MAKES can have its amounts and accounts edited: a plain payment,
// or a chain whose hops pass the money along. A rule written with legs of its own (by an import or
// an agent) is edited in its name, schedule, weekend rule and end only, and says so. A chain keeps
// its number of stops: adding or removing one is ending it and creating a new one.
Rectangle {
    id: root

    property var rule: null
    property string asOf: ""
    // For the pickers: the open accounts, plus any closed account the rule already uses.
    property var accounts: []

    signal saved(string note)
    signal cancelled

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8
    implicitHeight: form.implicitHeight + 24

    readonly property bool active: root.rule !== null
    readonly property bool whatIf: root.active && root.rule.scenario_id !== null && root.rule.scenario_id !== undefined
    readonly property bool twoLeg: root.active && root.rule.shape === "two_leg"
    readonly property int hopCount: root.active ? root.rule.current.hops.length : 0
    readonly property bool chained: root.hopCount > 1
    readonly property string currency: root.active ? (root.rule.currency || "") : ""
    readonly property bool changedBefore: root.active && root.rule.parts.length > 1
    // A change from a date that has not started yet: "the whole rule" is then only the terms from
    // that date, because the terms before it are a part of their own.
    readonly property bool pendingChange: root.changedBefore && root.rule.current.dtstart > root.asOf

    property string applyMode: "whole"
    property var route: []          // account ids, one more than hops
    property var amounts: []        // per hop: { text, minor, ok, bad }
    property string preset: "custom"
    property int dayOfMonth: 1
    property string weekday: "MO"
    property string weekendRule: "none"
    property string endMode: "never"
    property var previewDates: []
    property string previewError: ""
    property int previewSeq: 0
    property bool dayOk: true
    property var check: null
    property int checkSeq: 0
    property bool checking: false
    property bool armed: false
    property bool amendsFollow: true
    property bool loading: false
    property bool undoArmed: false
    property var undoCheck: null

    function dmy(iso) {
        if (!iso || iso.length !== 10)
            return iso || "";
        return parseInt(iso.substring(8, 10)) + "/" + parseInt(iso.substring(5, 7)) + "/" + iso.substring(0, 4);
    }
    function nameOf(id) {
        const a = (root.accounts || []).find(x => x.account_id === id);
        return a ? a.name : "…";
    }
    readonly property var passThroughAccounts: (root.accounts || []).filter(a => a.kind === "asset" || a.kind === "liability")

    // The seven strings the presets generate, read back into the preset that generates them.
    // Anything else is shown as the custom rule it is, rather than squeezed into a preset that
    // would save a different rule.
    function presetOf(text) {
        const r = (text || "").trim();
        let m = /^FREQ=MONTHLY;BYMONTHDAY=([1-9]|[12]\d|3[01])$/.exec(r);
        if (m)
            return { preset: "monthly_day", day: parseInt(m[1]), weekday: "MO" };
        if (r === "FREQ=MONTHLY;BYMONTHDAY=-1")
            return { preset: "monthly_last", day: 1, weekday: "MO" };
        if (r === "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1")
            return { preset: "payday", day: 1, weekday: "MO" };
        m = /^FREQ=WEEKLY;BYDAY=(MO|TU|WE|TH|FR|SA|SU)$/.exec(r);
        if (m)
            return { preset: "weekly", day: 1, weekday: m[1] };
        m = /^FREQ=WEEKLY;INTERVAL=2;BYDAY=(MO|TU|WE|TH|FR|SA|SU)$/.exec(r);
        if (m)
            return { preset: "fortnightly", day: 1, weekday: m[1] };
        m = /^FREQ=WEEKLY;INTERVAL=4;BYDAY=(MO|TU|WE|TH|FR|SA|SU)$/.exec(r);
        if (m)
            return { preset: "four_weekly", day: 1, weekday: m[1] };
        if (r === "FREQ=YEARLY")
            return { preset: "yearly", day: 1, weekday: "MO" };
        return { preset: "custom", day: 1, weekday: "MO" };
    }

    readonly property string rrule: {
        switch (root.preset) {
        case "monthly_day":   return "FREQ=MONTHLY;BYMONTHDAY=" + root.dayOfMonth;
        case "monthly_last":  return "FREQ=MONTHLY;BYMONTHDAY=-1";
        case "payday":        return "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1";
        case "weekly":        return "FREQ=WEEKLY;BYDAY=" + root.weekday;
        case "fortnightly":   return "FREQ=WEEKLY;INTERVAL=2;BYDAY=" + root.weekday;
        case "four_weekly":   return "FREQ=WEEKLY;INTERVAL=4;BYDAY=" + root.weekday;
        case "yearly":        return "FREQ=YEARLY";
        default:              return customRule.text;
        }
    }
    readonly property bool needsDay: root.preset === "monthly_day"
    readonly property bool needsWeekday: root.preset === "weekly" || root.preset === "fortnightly" || root.preset === "four_weekly"
    readonly property bool rruleUnchanged: root.active && root.rrule.trim().toUpperCase() === root.rule.current.rrule.trim().toUpperCase()
    readonly property bool routeEdited: root.twoLeg && root.route.some((id, i) => !root.rule.current.route[i] || root.rule.current.route[i].account_id !== id)
    readonly property bool amountsEdited: root.twoLeg && root.amounts.some((a, i) => !root.rule.current.hops[i] || a.minor !== root.rule.current.hops[i].amount_minor)
    // Only a new schedule, amount or account can start on a date. A new name, end or weekend rule
    // applies to the current terms as they stand, whatever the date field holds, and the form says so
    // rather than promising that earlier payments keep something they do not.
    readonly property bool datedEdit: !root.rruleUnchanged || root.routeEdited || root.amountsEdited
    readonly property bool undatedFrom: root.active && root.applyMode === "from" && !root.datedEdit
    onUndatedFromChanged: root.refreshPreview()

    function termsPhrase() {
        return root.pendingChange ? "the terms from " + root.dmy(root.rule.current.dtstart) : "every payment still to come";
    }
    function weekendWords(key) {
        return key === "before" ? "weekends moved earlier" : key === "after" ? "weekends moved later"
             : key === "modified_after" ? "weekends moved later in the same month" : "weekends left as they are";
    }

    // Open on `rule`; `on` is the occurrence it was opened from, if any, which makes it a change
    // from that date.
    function load(rule, on) {
        root.loading = true;
        root.rule = rule;
        root.check = null;
        root.armed = false;
        root.undoArmed = false;
        root.undoCheck = null;
        root.amendsFollow = true;
        root.dayOk = true;
        root.previewError = "";
        status.text = "";
        descField.text = rule.description;
        const p = root.presetOf(rule.current.rrule);
        customRule.text = rule.current.rrule;
        root.dayOfMonth = p.day;
        root.weekday = p.weekday;
        root.preset = p.preset;
        dayField.text = String(p.day);
        dayField.invalid = false;
        root.weekendRule = rule.current.weekend_rule;
        root.endMode = rule.current.until_on ? "on" : "never";
        endField.text = rule.current.until_on || "";
        root.route = rule.shape === "two_leg" ? rule.current.route.map(a => a.account_id) : [];
        root.amounts = rule.current.hops.map(h => ({ text: Money.format(h.amount_minor, rule.currency || ""), minor: h.amount_minor, ok: true, bad: false }));
        const hasHistory = rule.recorded.count > 0 || rule.parts.length > 1
                           || (rule.current.first_slot !== null && rule.current.first_slot < root.asOf);
        // An ended rule is changed as a whole: its end is what gets moved, and there is no date ahead
        // of it for a change to start from.
        root.applyMode = root.whatIf || rule.group === "ended" ? "whole" : (on && on.length === 10) ? "from" : hasHistory ? "from" : "whole";
        // A change from a date belongs to the CURRENT terms: a rule already changed from 1/1 cannot be
        // changed again from a date the earlier terms still cover, so a payment the earlier terms make
        // opens on the first date of the current ones instead.
        const ahead = root.datesAhead(rule);
        const onCurrent = on && on.length === 10 && on >= rule.current.dtstart;
        fromField.text = onCurrent ? on
                       : ahead.length > 0 ? ahead[0].occurrence_on
                       : rule.current.first_slot && rule.current.first_slot > root.asOf ? rule.current.first_slot
                       : root.asOf;
        startField.text = rule.current.dtstart;
        root.loading = false;
        root.refreshPreview();
        root.scheduleCheck();
    }

    // The next payments that belong to the current terms, which is where a change from a date can start.
    function datesAhead(rule) {
        return rule ? rule.next.filter(n => n.occurrence_on >= rule.current.dtstart) : [];
    }

    function validateAmount(i, text) {
        const next = root.amounts.slice();
        next[i] = Object.assign({}, next[i], { text: text, ok: false, bad: false });
        root.amounts = next;
        if (text.trim().length === 0) {
            root.scheduleCheck();
            return;
        }
        // The reply is handled in a function of its own: a const inside a nested closure blinds
        // the linter over the whole file.
        // A half-typed amount is not something gone wrong: the toolbar's error line is put back.
        const before = Ledger.lastError;
        Ledger.request("money.parse", { text: text, minor_digits: Money.digits(root.currency) },
                       (r, e) => root.parsedAmount(i, text, r, e, before));
    }
    function parsedAmount(i, text, r, e, before) {
        if (e)
            Ledger.lastError = before;
        if (i >= root.amounts.length || root.amounts[i].text !== text)
            return;
        const done = root.amounts.slice();
        if (e) {
            done[i] = Object.assign({}, done[i], { bad: true });
            status.text = e.message;
        } else {
            done[i] = Object.assign({}, done[i], { minor: Math.abs(r.minor), ok: r.minor !== 0, bad: r.minor === 0 });
            status.text = r.minor === 0 ? "an amount is always positive" : "";
        }
        root.amounts = done;
        root.scheduleCheck();
    }
    // A day that is not 1-31 marks the box and blocks saving, rather than quietly keeping the last
    // good day while the box shows another.
    function setDay(value) {
        const n = Number(value);
        root.dayOk = /^\s*\d{1,2}\s*$/.test(value) && n >= 1 && n <= 31;
        if (root.dayOk)
            root.dayOfMonth = n;
        root.scheduleCheck();
    }
    function pick(i, id) {
        const next = root.route.slice();
        next[i] = id;
        root.route = next;
        root.scheduleCheck();
    }

    function previewStart() {
        if (root.applyMode === "from")
            return root.rruleUnchanged ? root.rule.current.dtstart : fromField.text;
        return startField.text;
    }
    // Where the preview starts: what it pays from here on, or from the date a change starts. The
    // months already behind a rule are not what is being edited.
    function previewLo() {
        return root.applyMode === "from" && !root.undatedFrom && fromField.text.length === 10 ? fromField.text : root.asOf;
    }
    function previewFrom() {
        const start = root.previewStart();
        return start > root.previewLo() ? start : root.previewLo();
    }
    function refreshPreview() {
        if (!root.active || root.rrule.length === 0 || root.previewStart().length !== 10) {
            root.previewDates = [];
            root.previewError = "";
            return;
        }
        const params = { rrule: root.rrule, dtstart: root.previewStart(), count: 6, weekend_rule: root.weekendRule };
        if (root.previewLo().length === 10)
            params.from_on = root.previewLo();
        if (root.endMode === "on" && endField.text.length === 10)
            params.until_on = endField.text;
        root.previewSeq++;
        const seq = root.previewSeq;
        // A rule half typed is not something gone wrong: the toolbar's error line is put back.
        const before = Ledger.lastError;
        Ledger.request("series.preview", params, (r, e) => root.previewed(seq, r, e, before));
    }
    function previewed(seq, r, e, before) {
        if (e)
            Ledger.lastError = before;
        if (seq !== root.previewSeq)
            return;
        const was = root.previewError;
        root.previewDates = e ? [] : r.dates;
        root.previewError = e ? e.message : "";
        if (was !== root.previewError)
            root.scheduleCheck();
    }
    function emptyPreviewText() {
        const from = root.previewFrom();
        if (root.endMode === "on" && endField.text.length === 10)
            return endField.text < from ? "no payments after the end on " + root.dmy(endField.text)
                                        : "no payments between " + root.dmy(from) + " and the end on " + root.dmy(endField.text);
        return "no dates from " + root.dmy(from) + " — check the rule";
    }

    readonly property bool routeChosen: !root.twoLeg || root.route.every(id => id >= 0)
    readonly property bool routeMoves: !root.twoLeg || root.route.every((id, i) => i === 0 || id !== root.route[i - 1])
    readonly property bool amountsOk: !root.twoLeg || root.amounts.every(a => a.ok)
    // No preview dates is not incomplete: an ended rule, or an end before the next payment, has none,
    // and the dry run is what says whether such a save is sound. A rule that does not parse is.
    readonly property bool complete: root.active && descField.text.trim().length > 0
                                     && root.rrule.trim().length > 0 && root.previewError.length === 0
                                     && (!root.needsDay || root.dayOk)
                                     && (root.applyMode !== "from" || fromField.text.length === 10)
                                     && (root.endMode !== "on" || endField.text.length === 10)
                                     && root.routeChosen && root.routeMoves && root.amountsOk

    function revisionParams() {
        const p = {
            id: root.rule.current.ids[0],
            as_of: root.asOf,
            mode: root.applyMode,
            description: descField.text.trim(),
            rrule: root.rrule.trim(),
            weekend_rule: root.weekendRule,
            until_on: root.endMode === "on" && endField.text.length === 10 ? endField.text : null,
            amends: root.amendsFollow ? "follow" : "keep"
        };
        if (root.applyMode === "from")
            p.from_on = fromField.text;
        else if (root.rule.editable.start)
            p.dtstart = startField.text;
        if (root.twoLeg) {
            p.route = root.route;
            p.amounts = root.amounts.map(a => a.minor);
        }
        return p;
    }

    Timer {
        id: checkTimer
        interval: 350
        onTriggered: root.runCheck()
    }
    /// Make it a change from `fromOn`: what RuleImpact offers when the whole rule cannot change.
    function changeFrom(fromOn) {
        fromField.text = fromOn;
        root.applyMode = "from";
        root.refreshPreview();
        root.scheduleCheck();
    }
    function scheduleCheck() {
        if (root.loading || !root.active)
            return;
        root.armed = false;
        root.undoArmed = false;
        root.checking = true;
        checkTimer.restart();
    }
    function runCheck() {
        if (!root.complete || !root.visible) {
            root.check = null;
            root.checking = false;
            return;
        }
        root.checkSeq++;
        const seq = root.checkSeq;
        const params = Object.assign(root.revisionParams(), { dry_run: true });
        // A dry run that fails validation is the form being half filled in, not something gone
        // wrong: the toolbar's error line is put back as it was so it does not flash while typing.
        const before = Ledger.lastError;
        Ledger.request("series.revise", params, (r, e) => {
            if (seq !== root.checkSeq)
                return;
            root.checking = false;
            if (e) {
                Ledger.lastError = before;
                status.text = e.message;
                root.check = null;
            } else {
                status.text = "";
                root.check = r;
            }
        });
    }

    onRruleChanged: { root.refreshPreview(); root.scheduleCheck(); }
    onWeekendRuleChanged: { root.refreshPreview(); root.scheduleCheck(); }
    onEndModeChanged: { root.refreshPreview(); root.scheduleCheck(); }
    onApplyModeChanged: { root.refreshPreview(); root.scheduleCheck(); }
    onAmendsFollowChanged: root.scheduleCheck()

    // Something else wrote to the book while this is open (UPCOMING, another panel): the last check
    // describes a book that is gone, so it is asked again.
    Connections {
        target: Ledger
        function onRevisionChanged() {
            if (!root.active || !root.visible)
                return;
            root.undoArmed = false;
            root.undoCheck = null;
            root.refreshPreview();
            root.scheduleCheck();
        }
    }

    readonly property var drops: root.check ? (root.check.blockers || []).filter(b => b.kind === "override") : []
    readonly property var dues: root.check ? (root.check.blockers || []).filter(b => b.kind === "due") : []
    readonly property bool canSave: root.complete && root.check !== null && !root.checking
                                    && root.check.mode !== "unchanged"
                                    && (root.check.refusal === null || root.check.refusal.code === "would_drop"
                                        || root.check.refusal.code === "would_fall_due")
    readonly property bool needsArm: root.check !== null && (root.check.mode === "split" || root.drops.length > 0
                                     || root.dues.length > 0 || (root.amendsFollow && root.check.amends.follow.length > 0)
                                     || root.check.dormant.length > 0)
    function armSentence() {
        const c = root.check;
        if (!c)
            return "";
        const parts = [];
        if (c.mode === "split")
            parts.push("ends the current terms on " + root.dmy(c.ended.until_on) + " and starts the new ones on " + root.dmy(c.created.dtstart));
        if (root.drops.length > 0)
            parts.push("removes " + root.drops.length + " adjustment" + (root.drops.length === 1 ? "" : "s"));
        if (root.dues.length > 0)
            parts.push(root.dues.map(d => root.dmy(d.occurrence_on)).join(", ") + " falls due now");
        if (root.amendsFollow && c.amends.follow.length > 0)
            parts.push(c.amends.follow.length + " adjusted date" + (c.amends.follow.length === 1 ? "" : "s") + " take the new amount");
        if (c.dormant.length > 0)
            parts.push(c.dormant.length + " date" + (c.dormant.length === 1 ? "" : "s") + " fall after the new end");
        return parts.join(" · ");
    }
    function noteFrom(r) {
        if (r.mode === "split")
            return r.after.description + ": new terms from " + root.dmy(r.created.dtstart) + " · the earlier terms end " + root.dmy(r.ended.until_on);
        return r.after.description + ": corrected (" + r.changed.join(", ").replace("rrule", "schedule").replace("until_on", "end")
               .replace("weekend_rule", "weekend rule").replace("dtstart", "start") + ")";
    }

    function save() {
        if (!root.canSave)
            return;
        if (root.needsArm && !root.armed) {
            root.armed = true;
            return;
        }
        const params = Object.assign(root.revisionParams(), {
            drop_overrides: root.drops.length > 0,
            acknowledge_due: root.dues.length > 0
        });
        Ledger.write("series.revise", params, (r, e) => {
            root.armed = false;
            if (e) {
                status.text = e.message;
                root.scheduleCheck();
            } else {
                root.saved(root.noteFrom(r));
            }
        });
    }

    function undo() {
        if (!root.undoArmed) {
            Ledger.request("series.undo_change", { id: root.rule.current.ids[0], as_of: root.asOf, dry_run: true }, (r, e) => {
                if (e) {
                    status.text = e.message;
                    return;
                }
                root.undoCheck = r;
                root.undoArmed = true;
            });
            return;
        }
        Ledger.write("series.undo_change", {
            id: root.rule.current.ids[0], as_of: root.asOf,
            acknowledge_due: root.undoCheck !== null && root.undoCheck.blockers.length > 0
        }, (r, e) => {
            root.undoArmed = false;
            if (e)
                status.text = e.message;
            else
                root.saved(root.rule.description + ": the change from " + root.dmy(root.rule.current.dtstart) + " is undone");
        });
    }
    function undoSentence() {
        const u = root.undoCheck;
        if (!u || !root.active || root.rule.parts.length < 2)
            return "";
        const earlier = root.rule.parts[root.rule.parts.length - 2];
        let s = "goes back to " + earlier.amounts.map(a => Money.format(a, root.currency)).join(" / ") + ", " + earlier.phrase
                + (earlier.route_names.length > 0 ? ", " + earlier.route_names.join(" → ") : "")
                + ", " + root.weekendWords(earlier.weekend_rule)
                + (root.rule.current.until_on ? ", ending " + root.dmy(root.rule.current.until_on) : ", no end");
        if (u.carried_back.length > 0)
            s += " · " + u.carried_back.length + " adjustment" + (u.carried_back.length === 1 ? "" : "s") + " move back";
        if (u.lost.length > 0)
            s += " · lost: " + u.lost.map(l => l.word + " " + root.dmy(l.occurrence_on)).join(", ");
        if (u.blockers.length > 0)
            s += " · " + u.blockers.map(b => root.dmy(b.occurrence_on)).join(", ") + " falls due now";
        return s;
    }

    Column {
        id: form
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 12
        spacing: 8

        Text {
            width: parent.width
            elide: Text.ElideRight
            text: !root.active ? ""
                  : root.whatIf ? "EDIT HYPOTHETICAL " + (root.chained ? "CHAIN" : "PAYMENT") + " — only in “" + root.rule.scenario + "”"
                  : root.chained ? "EDIT RECURRING CHAIN — " + root.hopCount + " legs"
                  : "EDIT RECURRING PAYMENT"
            color: root.whatIf ? Theme.purple : Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        // ---- whole rule, or from a date ----
        Flow {
            width: parent.width
            spacing: 6
            visible: root.active && !root.whatIf
            PushButton {
                label: root.pendingChange ? "the terms from " + root.dmy(root.rule.current.dtstart) + " — correct them"
                                          : "the whole rule — correct it"
                primary: root.applyMode === "whole"
                onClicked: root.applyMode = "whole"
            }
            PushButton {
                label: "from a date — earlier payments keep the current terms"
                primary: root.applyMode === "from"
                onClicked: root.applyMode = "from"
            }
        }
        Row {
            spacing: 6
            visible: root.active && root.applyMode === "from"
            Field {
                id: fromField
                width: 130
                label: "changes from"
                numeric: true
                placeholder: "YYYY-MM-DD"
                onEdited: { root.refreshPreview(); root.scheduleCheck(); }
            }
            Repeater {
                model: root.datesAhead(root.rule).slice(0, 3)
                PushButton {
                    required property var modelData
                    required property int index
                    // A repeated delegate outlives its parent for a moment while it is torn down.
                    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
                    implicitHeight: 26
                    label: root.dmy(modelData.occurrence_on) + (index === 0 ? " (next)" : "")
                    primary: fromField.text === modelData.occurrence_on
                    onClicked: {
                        fromField.text = modelData.occurrence_on;
                        root.refreshPreview();
                        root.scheduleCheck();
                    }
                }
            }
        }
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.active
            text: !root.active ? ""
                  : root.whatIf ? "a what-if has no history to keep, so a change applies to the whole of it"
                  : root.undatedFrom
                    ? "a new name, end or weekend rule isn't tied to a date: it applies to " + root.termsPhrase()
                      + ". Only a new schedule, amount or account changes from a date"
                  : root.applyMode === "from"
                    ? "payments before " + root.dmy(fromField.text) + " keep the current terms"
                      + (root.rule.recorded.count > 0 ? "; the " + root.rule.recorded.count + " recorded payment"
                         + (root.rule.recorded.count === 1 ? " is" : "s are") + " never changed" : "")
                  : root.pendingChange
                    ? "corrects the terms from " + root.dmy(root.rule.current.dtstart) + "; payments before then keep the earlier terms"
                    : "changes every payment still to come; recorded payments keep what they recorded"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }

        Field {
            id: descField
            width: parent.width
            label: root.changedBefore ? "description (every part of the rule)" : "description"
            placeholder: "Rent, Salary, Netflix…"
            onEdited: root.scheduleCheck()
        }

        // ---- the money: one row per leg, then where it ends up ----
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.active && !root.twoLeg
            text: root.active && root.rule.editable.reason ? root.rule.editable.reason : ""
            color: Theme.warnAmber
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.active && !root.twoLeg
            text: !root.active ? "" : root.rule.current.legs.map(hop => hop.map(l => l.name + " " + (l.amount_minor < 0 ? "−" : "+")
                                     + Money.format(Math.abs(l.amount_minor), root.currency) + " (" + l.role + ")").join(",  ")).join("   |   ")
            color: Theme.textMuted
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 3
        }
        Repeater {
            model: root.twoLeg ? root.hopCount : 0
            delegate: Row {
                id: hopRow
                required property int index
                width: form.width
                spacing: 8
                AccountPicker {
                    width: (hopRow.width - 8) * 0.6
                    label: hopRow.index === 0 ? "from" : "via"
                    accounts: hopRow.index === 0 ? root.accounts : root.passThroughAccounts
                    Binding on selected { value: root.route[hopRow.index] ?? -1 }
                    onPicked: id => root.pick(hopRow.index, id)
                }
                Field {
                    width: (hopRow.width - 8) * 0.4
                    label: (hopRow.index === 0 ? "amount" : "then sends") + (root.currency.length > 0 ? " (" + root.currency + ")" : "")
                    numeric: true
                    text: root.amounts[hopRow.index] ? root.amounts[hopRow.index].text : ""
                    onEdited: value => root.validateAmount(hopRow.index, value)
                    Binding on invalid { value: root.amounts[hopRow.index] ? root.amounts[hopRow.index].bad : false }
                }
            }
        }
        Row {
            width: parent.width
            spacing: 8
            visible: root.twoLeg
            AccountPicker {
                width: (parent.width - 8) * 0.6
                label: "to"
                accounts: root.accounts
                Binding on selected { value: root.route[root.hopCount] ?? -1 }
                onPicked: id => root.pick(root.hopCount, id)
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                width: (parent.width - 8) * 0.4
                wrapMode: Text.Wrap
                text: root.chained ? "stops can't be added or removed here — end the chain and create a new one" : ""
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 4
            }
        }
        Text {
            width: parent.width
            elide: Text.ElideRight
            visible: root.twoLeg && root.chained
            text: root.route.map(id => root.nameOf(id)).join(" → ")
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }

        // ---- how often ----
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
                    implicitHeight: 26
                    label: modelData.text
                    primary: root.preset === modelData.key
                    onClicked: root.preset = modelData.key
                }
            }
        }
        Row {
            spacing: 6
            visible: root.needsDay || root.needsWeekday
            Field {
                id: dayField
                visible: root.needsDay
                width: 90
                label: "day"
                numeric: true
                onEdited: value => {
                    root.setDay(value);
                    // After Field clears its own mark on typing.
                    dayField.invalid = !root.dayOk;
                }
            }
            Repeater {
                model: root.needsWeekday ? ["MO", "TU", "WE", "TH", "FR", "SA", "SU"] : []
                PushButton {
                    required property var modelData
                    implicitHeight: 26
                    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
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
            onEdited: { root.refreshPreview(); root.scheduleCheck(); }
        }

        // ---- start, end and weekends ----
        Row {
            spacing: 6
            Field {
                id: startField
                visible: root.applyMode === "whole"
                width: 150
                enabled: root.active && root.rule.editable.start
                label: !root.active || root.rule.editable.start ? "starts"
                       : root.rule.current.dtstart > root.asOf ? "starts (changes " + root.dmy(root.rule.current.dtstart) + ")"
                       : "started (changed " + root.dmy(root.rule.current.dtstart) + ")"
                numeric: true
                onEdited: { root.refreshPreview(); root.scheduleCheck(); }
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "ends:"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                implicitHeight: 26
                label: "never"
                primary: root.endMode === "never"
                onClicked: root.endMode = "never"
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                implicitHeight: 26
                label: "on a date"
                primary: root.endMode === "on"
                onClicked: root.endMode = "on"
            }
            Field {
                id: endField
                visible: root.endMode === "on"
                width: 130
                label: "last payment"
                numeric: true
                placeholder: "YYYY-MM-DD"
                onEdited: { root.refreshPreview(); root.scheduleCheck(); }
            }
        }
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
                    implicitHeight: 26
                    label: modelData.text
                    primary: root.weekendRule === modelData.key
                    onClicked: root.weekendRule = modelData.key
                }
            }
        }

        // ---- the rule and its next dates ----
        Rectangle {
            width: parent.width
            implicitHeight: previewCol.implicitHeight + 12
            color: Theme.surfaceRaised
            radius: 5
            border.width: 1
            border.color: Theme.line
            Column {
                id: previewCol
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.top: parent.top
                anchors.margins: 6
                spacing: 2
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    text: root.rrule + (root.active && !root.rruleUnchanged ? "   (was " + root.rule.current.phrase + ")" : root.active ? "   " + root.rule.current.phrase : "")
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 3
                }
                Text {
                    width: parent.width
                    elide: Text.ElideRight
                    text: root.previewError.length > 0 ? root.previewError
                          : root.previewDates.length === 0 ? root.emptyPreviewText()
                          : (root.previewFrom() === root.asOf ? "from today: " : "from " + root.dmy(root.previewFrom()) + ": ")
                            + root.previewDates.map(d => d.moved ? root.dmy(d.value_on) + "*" : root.dmy(d.value_on)).join("   ")
                    color: root.previewError.length > 0 || root.previewDates.length === 0 ? Theme.warnAmber : Theme.text
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 2
                }
            }
        }

        RuleImpact {
            width: parent.width
            check: root.check
            rule: root.rule
            asOf: root.asOf
            applyMode: root.applyMode
            amendsFollow: root.amendsFollow
            onUseSuggestion: fromOn => root.changeFrom(fromOn)
            onToggleAmends: root.amendsFollow = !root.amendsFollow
        }
        Text {
            visible: root.checking
            text: "checking…"
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 4
        }

        Text {
            id: status
            width: parent.width
            wrapMode: Text.Wrap
            visible: text.length > 0
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.armed
            text: "Confirm: " + root.armSentence()
            color: Theme.warnAmber
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.undoArmed
            text: "Undo the change: " + root.undoSentence() + "?"
            color: Theme.warnAmber
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            spacing: 8
            PushButton {
                label: root.armed ? "Confirm"
                       : root.check && root.check.mode === "split" ? "Save from " + root.dmy(root.check.created.dtstart)
                       : root.check && root.check.mode === "unchanged" ? "Nothing to save"
                       : "Save"
                primary: true
                enabled: root.canSave
                onClicked: root.save()
            }
            PushButton {
                label: "Cancel"
                onClicked: {
                    root.armed = false;
                    root.undoArmed = false;
                    root.cancelled();
                }
            }
            PushButton {
                visible: root.active && root.changedBefore && !root.whatIf
                label: root.undoArmed ? "sure? undo" : "undo change from " + (root.active ? root.dmy(root.rule.current.dtstart) : "")
                onClicked: root.undo()
            }
        }
    }
}
