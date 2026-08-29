pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Dialogs
import Quickshell
import "../services"

// The brief: what the book says, and what it cannot say.
//
// THIS IS NOT AN EXPORT SCREEN. Export hands over lines and leaves the arithmetic to whoever reads
// them, which is exactly where a language model goes wrong -- it sums hundreds of integers badly,
// annualises 4-weekly as x12, compares a part month against whole ones, and has no idea which
// outgoings were ever a choice. `analysis.brief` does that arithmetic in the core, so what leaves
// this window is conclusions-ready rather than raw.
//
// The same computation drives the screen and the clipboard on purpose. What Sophie reads here and
// what a model is handed are the same numbers, so a disagreement between her and it is about
// judgement rather than about who added up wrong.
Rectangle {
    id: root

    property string asOf: ""
    property var brief: null
    property string note: ""

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    // Three triggers, because one is not enough: `onVisibleChanged` alone misses a panel that
    // starts visible (the signal fires on a CHANGE, and there was none), and a brief computed
    // before an import is stale the moment the book moves.
    Component.onCompleted: if (root.visible) root.load()
    onVisibleChanged: if (root.visible && !root.brief) root.load()
    Connections {
        target: Ledger
        function onRevisionChanged() {
            if (root.visible) root.load();
            else root.brief = null;   // recomputed next time it is opened, never shown stale
        }
    }

    function load() {
        root.note = "reading the book…";
        Ledger.request("analysis.brief", { as_of: root.asOf, months: 6 }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.brief = r;
            root.note = "";
        });
    }

    // Percent formatting lives here rather than in the core: the core deals in exact integers and
    // has no business rounding for a label.
    function pct(v) { return v === null || v === undefined ? "" : (v > 0 ? "+" : "") + v + "%"; }

    readonly property var committed: brief && brief.commitments
        ? (brief.commitments.monthly_equivalent_by_currency || []) : []
    readonly property var deviations: {
        if (!brief || !brief.typical) return [];
        // Only expenses, only the ones that actually moved: a list of everything that stayed the
        // same is a list nobody reads.
        return (brief.typical.accounts || [])
            .filter(a => a.kind === "expense" && Math.abs(a.deviation_minor) >= 500)
            .slice(0, 6);
    }
    readonly property var worstOutlook: {
        if (!brief || !brief.outlook) return null;
        const accts = brief.outlook.accounts || [];
        let worst = null;
        for (const a of accts)
            if (!worst || a.lowest_minor < worst.lowest_minor) worst = a;
        return worst;
    }

    FileDialog {
        id: saver
        title: "Save the brief"
        fileMode: FileDialog.SaveFile
        defaultSuffix: "json"
        nameFilters: ["JSON (*.json)"]
        onAccepted: {
            const path = selectedFile.toString().replace("file://", "");
            // Written by the core, not by the frontend: one JSON serialiser, and the file is
            // byte-identical to what a model would receive over the socket.
            Ledger.request("analysis.brief", { as_of: root.asOf, months: 6, path: path }, (r, e) => {
                root.note = e ? e.message : ("written to " + r.written);
            });
        }
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Row {
            width: parent.width
            spacing: 8
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "BRIEF"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: root.brief
                      ? root.brief.history_window.from + " to " + root.brief.history_window.to
                        + "  ·  " + root.brief.history_window.complete_months_with_data
                        + " complete months"
                      : ""
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
            }
            PushButton { label: "Refresh"; onClicked: root.load() }
            PushButton {
                label: "Copy for a model"
                primary: true
                enabled: root.brief !== null
                onClicked: {
                    Quickshell.clipboardText = JSON.stringify(root.brief, null, 2);
                    root.note = "the brief is on the clipboard — paste it into a chat";
                }
            }
            PushButton {
                label: "Save…"
                enabled: root.brief !== null
                onClicked: saver.open()
            }
        }

        // ---- the two numbers that answer most questions ----
        Row {
            width: parent.width
            spacing: 10
            visible: root.brief !== null

            Rectangle {
                width: (parent.width - 10) / 2
                implicitHeight: 58
                color: Theme.surfaceRaised
                border.width: 1
                border.color: Theme.line
                radius: 5
                Column {
                    anchors.fill: parent
                    anchors.margins: 8
                    spacing: 2
                    Text {
                        text: "COMMITTED EACH MONTH"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                    Text {
                        text: root.committed.length === 0
                              ? "nothing recurring recorded"
                              : root.committed.map(c => c.amount_decimal + " " + c.currency).join("   ")
                        color: root.committed.length === 0 ? Theme.warnAmber : Theme.text
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize + 1
                    }
                    Text {
                        text: root.brief && root.brief.commitments.series.length > 0
                              ? root.brief.commitments.series.length
                                + " series, expanded over the next 12 months"
                              : "a 4-weekly payment is not a monthly one — the core expands the real rule"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                }
            }

            Rectangle {
                width: (parent.width - 10) / 2
                implicitHeight: 58
                color: Theme.surfaceRaised
                border.width: 1
                border.color: root.brief && root.brief.outlook.goes_negative ? Theme.red : Theme.line
                radius: 5
                Column {
                    anchors.fill: parent
                    anchors.margins: 8
                    spacing: 2
                    Text {
                        text: "LOWEST PROJECTED BALANCE"
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                    Text {
                        text: root.worstOutlook
                              ? root.worstOutlook.lowest_decimal + "  on " + root.worstOutlook.lowest_on
                              : "nothing scheduled in the window"
                        color: root.worstOutlook && root.worstOutlook.lowest_minor < 0
                               ? Theme.red : Theme.text
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize + 1
                    }
                    Text {
                        text: root.worstOutlook
                              ? (root.worstOutlook.first_negative_on
                                 ? root.worstOutlook.account + " goes under on "
                                   + root.worstOutlook.first_negative_on
                                 : root.worstOutlook.account + " stays positive to "
                                   + root.brief.outlook.horizon)
                              : ""
                        color: root.worstOutlook && root.worstOutlook.first_negative_on
                               ? Theme.red : Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                }
            }
        }

        // ---- what moved ----
        Text {
            visible: root.deviations.length > 0
            text: "AGAINST THE USUAL MONTH"
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
        Column {
            width: parent.width
            spacing: 2
            Repeater {
                model: root.deviations
                Row {
                    id: devRow
                    required property var modelData
                    width: parent.width
                    spacing: 8
                    Text {
                        width: 130
                        elide: Text.ElideRight
                        text: devRow.modelData.account
                        color: Theme.text
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        width: 80
                        horizontalAlignment: Text.AlignRight
                        text: devRow.modelData.median_monthly_decimal
                        color: Theme.textFaint
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        text: "→"
                        color: Theme.textFaint
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        width: 80
                        horizontalAlignment: Text.AlignRight
                        text: devRow.modelData.latest_month_decimal
                        color: Theme.text
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        width: 90
                        horizontalAlignment: Text.AlignRight
                        // An overspend is the red one: on an expense account a positive deviation
                        // means more went out, which is the opposite of the ledger's usual reading.
                        text: (devRow.modelData.deviation_minor > 0 ? "+" : "")
                              + devRow.modelData.deviation_decimal
                        color: devRow.modelData.deviation_minor > 0 ? Theme.red : Theme.okGreen
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Text {
                        text: root.pct(devRow.modelData.deviation_pct)
                        color: Theme.textFaint
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Text {
                        text: devRow.modelData.months_observed < 3
                              ? "(only " + devRow.modelData.months_observed + " months)" : ""
                        color: Theme.warnAmber
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                }
            }
        }

        // ---- the honest bit ----
        Text {
            visible: root.brief !== null
            text: "WHAT THIS CANNOT TELL YOU"
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
        Column {
            width: parent.width
            spacing: 3
            Repeater {
                model: root.brief ? root.brief.limits : []
                Row {
                    id: limRow
                    required property var modelData
                    width: parent.width
                    spacing: 6
                    Text {
                        width: 54
                        text: limRow.modelData.severity
                        color: limRow.modelData.severity === "fatal" || limRow.modelData.severity === "high"
                               ? Theme.red
                               : limRow.modelData.severity === "medium" ? Theme.warnAmber : Theme.textFaint
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 4
                    }
                    Text {
                        width: parent.width - 60
                        wrapMode: Text.Wrap
                        text: limRow.modelData.detail
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                }
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: root.note
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
    }
}
