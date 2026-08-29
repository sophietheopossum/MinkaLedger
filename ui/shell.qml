import QtQuick
import QtQuick.Layouts
import Quickshell
import "services"
import "modules"

// MinkaLedger — the forecast-first ledger's window.
//
// A thin view over the Rust core: every number shown comes from a request, nothing is computed
// here and nothing is cached. The core owns the book, the arithmetic and the win.projection; this
// draws them.
ShellRoot {
    FloatingWindow {
        id: win
        title: "MinkaLedger"
        implicitWidth: 900
        implicitHeight: 620
        color: Theme.ground

        // The window asks for what it needs whenever the book changes, rather than mirroring state.
        property var accounts: []
        property var projection: ({ balances: [], occurrences: [] })
        property string asOf: Qt.formatDate(new Date(), "yyyy-MM-dd")
        property string horizon: Qt.formatDate(
            new Date(new Date().setMonth(new Date().getMonth() + 12)), "yyyy-MM-dd")
        property int focusAccount: -1
        property var scenarios: []
        property var activeScenarios: []
        property var editing: null          // the occurrence open in the editor
        property bool showEntry: false      // the record-a-payment form

        function refresh() {
            Ledger.request("account.balances", {}, (r, e) => {
                if (!e) {
                    win.accounts = r || [];
                    if (win.focusAccount < 0 && win.accounts.length > 0)
                        win.focusAccount = win.accounts[0].account_id;
                }
            });
            Ledger.request("scenario.list", {}, (r, e) => { if (!e) win.scenarios = r || []; });
            Ledger.request("forecast.project",
                           { as_of: win.asOf, horizon: win.horizon, scenarios: win.activeScenarios },
                           (r, e) => { if (!e) win.projection = r; });
        }

        Component.onCompleted: win.refresh()
        Connections {
            target: Ledger
            function onRevisionChanged() { win.refresh(); }
        }

        // The projected balance series for the focused account, seeded with today's actual so the
        // line starts from where the money really is.
        function seriesFor(accountId) {
            const out = [];
            const acc = win.accounts.find(a => a.account_id === accountId);
            if (acc)
                out.push({ on: win.asOf, balance_minor: acc.balance_minor });
            for (const b of (win.projection.balances || []))
                if (b.account_id === accountId)
                    out.push({ on: b.on, balance_minor: b.balance_minor });
            return out;
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 14
            spacing: 12

            RowLayout {
                Layout.fillWidth: true
                Text {
                    text: "MinkaLedger"
                    color: Theme.text
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize + 4
                }
                Item { Layout.fillWidth: true }
                Text {
                    visible: Ledger.lastError.length > 0
                    text: Ledger.lastError
                    color: Theme.red
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 1
                }
                PushButton {
                    label: win.showEntry ? "Close" : "+ payment"
                    primary: !win.showEntry
                    onClicked: win.showEntry = !win.showEntry
                }
                Text {
                    text: Ledger.running ? "core up" : "core down"
                    color: Ledger.running ? Theme.textFaint : Theme.red
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 1
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                spacing: 12

                // ---- win.accounts ----
                Rectangle {
                    Layout.preferredWidth: 260
                    Layout.fillHeight: true
                    color: Theme.surface
                    border.color: Theme.line
                    border.width: 1

                    ColumnLayout {
                        anchors.fill: parent
                        anchors.margins: 10
                        spacing: 6

                        RowLayout {
                            Layout.fillWidth: true
                            Text {
                                text: "ACCOUNTS"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                            Item { Layout.fillWidth: true }
                            PushButton {
                                label: "+ account"
                                onClicked: newAccount.visible = !newAccount.visible
                            }
                        }

                        // Creating an account is rare but a fresh book is unusable without it, so
                        // it lives here rather than behind a menu.
                        Rectangle {
                            id: newAccount
                            visible: false
                            Layout.fillWidth: true
                            implicitHeight: 108
                            color: Theme.surfaceRaised
                            border.color: Theme.line
                            border.width: 1
                            radius: 6

                            Column {
                                anchors.fill: parent
                                anchors.margins: 8
                                spacing: 6
                                Field {
                                    id: accName
                                    width: parent.width
                                    label: "name"
                                    placeholder: "Current, Rent, Salary…"
                                }
                                Row {
                                    spacing: 6
                                    Repeater {
                                        model: ["asset", "liability", "income", "expense"]
                                        PushButton {
                                            label: modelData
                                            primary: newAccount.kind === modelData
                                            onClicked: newAccount.kind = modelData
                                        }
                                    }
                                }
                                Row {
                                    spacing: 6
                                    PushButton {
                                        label: "Create"
                                        primary: true
                                        enabled: accName.text.length > 0
                                        onClicked: {
                                            Ledger.write("account.create",
                                                { name: accName.text, kind: newAccount.kind },
                                                (r, e) => {
                                                    if (!e) { accName.clear(); newAccount.visible = false; }
                                                });
                                        }
                                    }
                                    PushButton { label: "Cancel"; onClicked: newAccount.visible = false }
                                }
                            }
                            property string kind: "expense"
                        }

                        ListView {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            clip: true
                            model: win.accounts
                            delegate: Rectangle {
                                width: ListView.view.width
                                height: 26
                                color: modelData.account_id === win.focusAccount
                                       ? Theme.surfaceRaised : "transparent"
                                RowLayout {
                                    anchors.fill: parent
                                    anchors.leftMargin: 6
                                    anchors.rightMargin: 6
                                    Text {
                                        Layout.fillWidth: true
                                        text: modelData.name
                                        elide: Text.ElideRight
                                        color: Theme.text
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 1
                                    }
                                    Text {
                                        text: (modelData.balance_minor / 100).toFixed(2)
                                        color: modelData.balance_minor < 0 ? Theme.red : Theme.text
                                        font.family: Theme.monoFamily
                                        font.pixelSize: Theme.fontSize - 1
                                    }
                                }
                                MouseArea {
                                    anchors.fill: parent
                                    onClicked: win.focusAccount = modelData.account_id
                                }
                            }
                        }
                    }
                }

                // ---- forecast ----
                Rectangle {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    color: Theme.surface
                    border.color: Theme.line
                    border.width: 1

                    ColumnLayout {
                        anchors.fill: parent
                        anchors.margins: 10
                        spacing: 6

                        RowLayout {
                            Layout.fillWidth: true
                            Text {
                                text: "PROJECTED BALANCE"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                            Item { Layout.fillWidth: true }
                            Text {
                                text: win.asOf + "  to  " + win.horizon
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                        }

                        ForecastChart {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            series: win.seriesFor(win.focusAccount)
                            todayIso: win.asOf
                        }

                        // Scenario toggles: requirement 8, as a row of switches over a live baseline.
                        Flow {
                            Layout.fillWidth: true
                            spacing: 6
                            visible: win.scenarios.length > 0
                            Repeater {
                                model: win.scenarios
                                Rectangle {
                                    height: 22
                                    width: label.implicitWidth + 16
                                    radius: 3
                                    color: win.activeScenarios.indexOf(modelData.id) >= 0
                                           ? Theme.purpleDim : Theme.surfaceRaised
                                    border.color: Theme.line
                                    Text {
                                        id: label
                                        anchors.centerIn: parent
                                        text: modelData.name
                                        color: Theme.text
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 2
                                    }
                                    MouseArea {
                                        anchors.fill: parent
                                        onClicked: {
                                            const next = win.activeScenarios.slice();
                                            const at = next.indexOf(modelData.id);
                                            if (at >= 0) next.splice(at, 1); else next.push(modelData.id);
                                            win.activeScenarios = next;
                                            win.refresh();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            EntryForm {
                Layout.fillWidth: true
                Layout.preferredHeight: 270
                visible: win.showEntry
                accounts: win.accounts
                defaultDate: win.asOf
                onSaved: win.showEntry = false
            }

            OccurrenceEditor {
                Layout.fillWidth: true
                Layout.preferredHeight: 190
                occurrence: win.editing
                onChanged: win.editing = null
                onDismissed: win.editing = null
            }

            // ---- what is coming ----
            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: 150
                color: Theme.surface
                border.color: Theme.line
                border.width: 1

                ColumnLayout {
                    anchors.fill: parent
                    anchors.margins: 10
                    spacing: 4

                    Text {
                        text: "UPCOMING"
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }

                    ListView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        // One line per occurrence on the focused account, in date order.
                        model: (win.projection.occurrences || []).filter(
                            o => o.account_id === win.focusAccount)
                        delegate: RowLayout {
                            id: occRow
                            width: ListView.view.width
                            spacing: 10
                            // Only a real series occurrence can be overridden. Generated interest
                            // and payment legs carry a negative series_id and are not editable:
                            // they are consequences of a rule, not instances of one.
                            readonly property bool editable: modelData.series_id > 0
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: occRow.editable ? Qt.PointingHandCursor : Qt.ArrowCursor
                                onClicked: if (occRow.editable) win.editing = modelData
                            }
                            Text {
                                text: modelData.value_on
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                Layout.fillWidth: true
                                text: modelData.description
                                       + (modelData.value_on !== modelData.occurrence_on
                                          ? "  (moved from " + modelData.occurrence_on + ")" : "")
                                elide: Text.ElideRight
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                text: (modelData.amount_minor / 100).toFixed(2)
                                color: modelData.amount_minor < 0 ? Theme.red : Theme.okGreen
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                        }
                    }
                }
            }
        }
    }
}
