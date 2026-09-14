pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// What a change to a recurring payment will do, BEFORE it is saved.
//
// The editor asks the core to make the change and roll it back (`series.revise` with dry_run), and
// this shows the answer: whether the rule is corrected in place or changed from a date, what the
// forecast gains, loses and reprices over the next twelve months, the seam where the old terms end
// and the new ones begin, and anything that stands in the way. Nothing here is computed from the
// form: every line is the core's account of the book as it would be, so what is shown is what a
// save does.
//
// It never assigns its inputs. The editor owns `check` and `amendsFollow`; this only signals.
Column {
    id: root

    property var check: null
    property var rule: null
    property string asOf: ""
    property string applyMode: "whole"
    property bool amendsFollow: true

    signal useSuggestion(string fromOn)
    signal toggleAmends

    spacing: 4
    visible: root.check !== null

    property bool showAllImpact: false
    onCheckChanged: root.showAllImpact = false

    readonly property string currency: root.rule ? (root.rule.currency || "") : ""
    readonly property string desc: root.rule ? root.rule.description : ""

    function dmy(iso) {
        if (!iso || iso.length !== 10)
            return iso || "";
        return parseInt(iso.substring(8, 10)) + "/" + parseInt(iso.substring(5, 7)) + "/" + iso.substring(0, 4);
    }
    function money(minor) {
        return minor === null || minor === undefined ? "" : Money.format(Math.abs(minor), root.currency);
    }
    // The words on the editor's buttons, not the stored keys.
    function weekendWords(key) {
        return key === "before" ? "move earlier" : key === "after" ? "move later"
             : key === "modified_after" ? "later, same month" : "leave it";
    }
    function list(items) {
        return !items || items.length === 0 ? [] : items;
    }

    function modeLine() {
        const c = root.check;
        if (!c)
            return "";
        if (c.mode === "unchanged")
            return "Nothing changes";
        if (c.mode === "split") {
            let line = "Ends the current terms on " + root.dmy(c.ended.until_on)
                     + " and starts the new ones on " + root.dmy(c.created.dtstart);
            if (c.anchor && c.anchor.rrule_unchanged && c.anchor.first_slot !== c.anchor.from_on)
                line += " — the first payment on or after " + root.dmy(c.anchor.from_on) + ", so it keeps its rhythm";
            return line;
        }
        const dated = (c.changed || []).some(k => k === "rrule" || k === "route" || k === "amounts" || k === "dtstart");
        const pending = root.rule !== null && root.rule.parts.length > 1 && root.rule.current.dtstart > root.asOf;
        if (root.applyMode === "from" && dated)
            return root.rule && root.rule.parts.length > 1
                ? "The current terms start " + root.dmy(root.check.after.dtstart) + " and nothing of them falls before the date, so they are corrected in place"
                : "Nothing of " + root.desc + " falls before the date, so it is corrected in place";
        if (root.applyMode === "from")
            return "Not tied to a date, so it applies to the current terms" + (pending ? " from " + root.dmy(root.rule.current.dtstart) : "") + " in place";
        if (pending)
            return "Corrects the terms from " + root.dmy(root.rule.current.dtstart) + " in place — payments before then keep the earlier terms";
        return "Corrects the rule in place — recorded payments keep what they recorded";
    }

    function changeLines() {
        const c = root.check;
        if (!c || !c.before || !c.after)
            return [];
        const b = c.before;
        const a = c.after;
        const out = [];
        if (a.description !== b.description)
            out.push("name: " + b.description + " → " + a.description);
        if (a.rrule !== b.rrule)
            out.push("schedule: " + b.phrase + " → " + a.phrase);
        if (a.dtstart !== b.dtstart && c.mode !== "split")
            out.push("starts: " + root.dmy(b.dtstart) + " → " + root.dmy(a.dtstart));
        if (a.until_on !== b.until_on)
            out.push("end: " + (b.until_on ? root.dmy(b.until_on) : "none") + " → " + (a.until_on ? root.dmy(a.until_on) : "none"));
        if (a.weekend_rule !== b.weekend_rule)
            out.push("on a weekend: " + root.weekendWords(b.weekend_rule) + " → " + root.weekendWords(a.weekend_rule));
        if (a.route_names.join(" → ") !== b.route_names.join(" → "))
            out.push("route: " + b.route_names.join(" → ") + "  ⇒  " + a.route_names.join(" → "));
        for (let i = 0; i < a.amounts.length; i++) {
            if (a.amounts[i] === b.amounts[i])
                continue;
            if (a.amounts.length === 1)
                out.push("amount: " + root.money(b.amounts[i]) + " → " + root.money(a.amounts[i]));
            else
                out.push((i === 0 ? a.route_names[0] : a.route_names[i]) + " sends " + root.money(b.amounts[i]) + " → " + root.money(a.amounts[i]));
        }
        return out;
    }

    function seamText() {
        const c = root.check;
        if (!c || !c.seam || c.seam.length === 0)
            return "";
        // A loop, not a map callback: a const inside a nested closure blinds the linter.
        const parts = [];
        for (const s of c.seam) {
            const glyph = s.state === "recorded" ? "✓ recorded" : s.state === "new" ? "new"
                        : s.state === "earlier" ? (c.mode === "split" ? "old" : "due") : "";
            const amount = s.amount_minor === null ? "" : " " + root.money(s.amount_minor);
            const gap = s.gap_days === null ? "" : " +" + s.gap_days + " d";
            parts.push(root.dmy(s.value_on) + " " + glyph + amount + gap);
        }
        return parts.join("   ·   ");
    }

    function blockersOf(kind) {
        const c = root.check;
        return c ? (c.blockers || []).filter(b => b.kind === kind) : [];
    }
    function recordedText() {
        return root.blockersOf("recorded").map(b => root.dmy(b.occurrence_on) + " (paid " + root.dmy(b.occurred_on) + ")").join(", ");
    }
    function droppedText() {
        return root.blockersOf("override").map(b => b.word + " " + root.dmy(b.occurrence_on)
                                                    + (b.moved_to ? " → " + root.dmy(b.moved_to) : "")).join(" · ");
    }
    function dueText() {
        return root.blockersOf("due").map(b => root.dmy(b.occurrence_on) + " (money moves " + root.dmy(b.value_on) + ")").join(", ");
    }
    function datesOf(items) {
        return root.list(items).map(x => (x.word ? x.word + " " : "") + root.dmy(x.occurrence_on)).join(" · ");
    }

    function impactRows() {
        const c = root.check;
        if (!c || !c.impact)
            return [];
        const rows = [];
        for (const p of c.impact.appears)
            rows.push({ sign: "+", text: root.dmy(p.value_on) + "  " + root.money(p.amount_minor) + (p.seq > 0 ? "  (leg " + (p.seq + 1) + ")" : ""), color: Theme.okGreen });
        for (const p of c.impact.disappears)
            rows.push({ sign: "−", text: root.dmy(p.value_on) + "  " + root.money(p.amount_minor) + (p.seq > 0 ? "  (leg " + (p.seq + 1) + ")" : ""), color: Theme.warnAmber });
        for (const ch of c.impact.changes) {
            const moved = ch.before.value_on !== ch.after.value_on ? root.dmy(ch.before.value_on) + " → " + root.dmy(ch.after.value_on) : root.dmy(ch.after.value_on);
            const repriced = ch.before.amount_minor !== ch.after.amount_minor
                ? "  " + root.money(ch.before.amount_minor) + " → " + root.money(ch.after.amount_minor)
                : "  " + root.money(ch.after.amount_minor);
            rows.push({ sign: "~", text: moved + repriced + (ch.seq > 0 ? "  (leg " + (ch.seq + 1) + ")" : ""), color: Theme.text });
        }
        return rows;
    }
    readonly property var allImpact: root.impactRows()
    readonly property var shownImpact: root.showAllImpact ? root.allImpact : root.allImpact.slice(0, 8)

    // ---- the mode ----
    Text {
        width: root.width
        wrapMode: Text.Wrap
        text: root.modeLine()
        color: root.check && root.check.mode === "split" ? Theme.purple : Theme.text
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 2
    }

    // ---- before and after ----
    Repeater {
        model: root.changeLines()
        Text {
            required property var modelData
            width: root.width
            wrapMode: Text.Wrap
            text: modelData
            color: Theme.textMuted
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 3
        }
    }

    // ---- the seam ----
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: text.length > 0
        text: root.seamText().length > 0 ? "around the change:  " + root.seamText() : ""
        color: Theme.textFaint
        font.family: Theme.monoFamily
        font.pixelSize: Theme.fontSize - 3
    }

    // ---- what stands in the way ----
    Text {
        width: root.width
        wrapMode: Text.Wrap
        // The adjustments and due dates have their own lines below; a refusal only needs saying here
        // when nothing else explains it.
        visible: root.check !== null && root.check.refusal !== null && root.check.refusal !== undefined
                 && root.check.refusal.code !== "would_drop" && root.check.refusal.code !== "would_fall_due"
        text: visible ? root.check.refusal.message : ""
        color: Theme.warnAmber
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 2
    }
    Row {
        spacing: 8
        visible: root.blockersOf("recorded").length > 0
        Text {
            anchors.verticalCenter: parent.verticalCenter
            width: Math.min(implicitWidth, root.width - suggest.width - 8)
            elide: Text.ElideRight
            text: "recorded as paid under the current schedule: " + root.recordedText()
            color: Theme.warnAmber
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
        PushButton {
            id: suggest
            visible: root.check !== null && !!root.check.suggest_from
            implicitHeight: 24
            label: visible ? "change it from " + root.dmy(root.check.suggest_from) + " instead" : ""
            onClicked: root.useSuggestion(root.check.suggest_from)
        }
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.blockersOf("override").length > 0
        text: "no longer on the new schedule, so saving removes these adjustments: " + root.droppedText()
        color: Theme.warnAmber
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.blockersOf("due").length > 0
        text: "would fall due now: " + root.dueText() + " — make sure it isn't already paid"
        color: Theme.warnAmber
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }

    // ---- where adjustments go ----
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && root.list(root.check.carried).length > 0
        text: visible ? "carried to the new terms: " + root.datesOf(root.check.carried) : ""
        color: Theme.textMuted
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && root.list(root.check.left_behind).length > 0
        text: visible ? "stays with the earlier terms and no longer applies (it comes back if the change is undone): "
                        + root.datesOf(root.check.left_behind) + " — the new schedule has no payment that day" : ""
        color: Theme.textFaint
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && root.list(root.check.left_on_old_dates).length > 0
        text: visible ? "older adjustments left where they are (they no longer change any forecast): "
                        + root.datesOf(root.check.left_on_old_dates) : ""
        color: Theme.textFaint
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }

    // ---- amends saved at the old price ----
    Row {
        spacing: 8
        visible: root.check !== null && root.check.amends && root.check.amends.follow.length > 0
        PushButton {
            implicitHeight: 24
            primary: root.amendsFollow
            label: visible ? (root.amendsFollow ? "✓ " : "") + root.check.amends.follow.length
                             + " adjusted date" + (root.check.amends.follow.length === 1 ? "" : "s") + " take the new amount" : ""
            onClicked: root.toggleAmends()
        }
        Text {
            anchors.verticalCenter: parent.verticalCenter
            text: visible ? root.check.amends.follow.map(f => root.dmy(f.occurrence_on)).join(", ")
                            + " carried the old amount only because the date was moved" : ""
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && root.check.amends && root.check.amends.keep.length > 0
        text: visible ? "keeps an amount of its own: " + root.check.amends.keep.map(k => root.dmy(k.occurrence_on) + " "
                        + k.amounts.map(a => a === null ? "rule" : root.money(a)).join(" / ")).join(", ")
                        + " — Reset it in UPCOMING to follow the rule" : ""
        color: Theme.textFaint
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && root.list(root.check.dormant).length > 0
        text: visible ? "after the new end, so no longer paid: " + root.check.dormant.map(d => (d.kind === "recorded" ? "recorded " : (d.word || "adjusted") + " ")
                        + root.dmy(d.occurrence_on)).join(", ") + " (records stay as they are)" : ""
        color: Theme.textFaint
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }

    // ---- the next twelve months ----
    Text {
        visible: root.allImpact.length > 0
        text: "IN THE NEXT 12 MONTHS  (+ new · − gone · ~ changed)"
        color: Theme.textMuted
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 4
    }
    Flow {
        width: root.width
        spacing: 12
        visible: root.allImpact.length > 0
        Repeater {
            model: root.shownImpact
            Text {
                required property var modelData
                text: modelData.sign + " " + modelData.text
                color: modelData.color
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
            }
        }
        Text {
            visible: root.allImpact.length > root.shownImpact.length
            text: "+" + (root.allImpact.length - root.shownImpact.length) + " more"
            color: Theme.purple
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
            MouseArea {
                anchors.fill: parent
                cursorShape: Qt.PointingHandCursor
                onClicked: root.showAllImpact = true
            }
        }
    }
    Text {
        width: root.width
        wrapMode: Text.Wrap
        visible: root.check !== null && !!root.check.impact && !!root.check.impact.before_error
        text: visible ? "the stored rule doesn't expand (" + root.check.impact.before_error + "): every forecast fails until this is saved" : ""
        color: Theme.red
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 3
    }
}
