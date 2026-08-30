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

        // Closing the window ENDS the app. A Quickshell config is a shell rather than an
        // application, so `qs` stays alive after its last window is destroyed -- which for a
        // windowed app means an invisible process per launch, each still holding its resources.
        // Alt+F4 and Super+Q both reach here: ShojiWM's closeFocusedWindow() sends
        // xdg_toplevel.close, a request, and this is the handler for it.
        //
        // MinkaMon deliberately does the opposite (`onClosed: visible = false`) because it is a
        // long-lived monitor whose satellites reopen. This is not that.
        onClosed: Qt.quit()
        implicitWidth: 900
        implicitHeight: 620
        color: Theme.ground

        // The window asks for what it needs whenever the book changes, rather than mirroring state.
        property var accounts: []       // open accounts, for the pickers and the chart
        property var allAccounts: []    // every account incl. hidden, with reference counts
        property bool editAccounts: false
        property bool showDanger: false
        property bool showCurrencies: false
        property bool showJourneys: false
        property bool showPayments: false
        property var currencies: []
        property var projection: ({ balances: [], occurrences: [] })
        property string asOf: Qt.formatDate(new Date(), "yyyy-MM-dd")
        property string horizon: Qt.formatDate(
            new Date(new Date().setMonth(new Date().getMonth() + 12)), "yyyy-MM-dd")
        property int focusAccount: -1
        property var scenarios: []
        property var activeScenarios: []
        property var editing: null          // the occurrence open in the editor
        property bool showEntry: false      // the record-a-payment form
        property bool showSeries: false     // the new-recurring-payment form
        property bool showImport: false     // the CSV import screen
        property bool showBrief: false      // the computed brief, for reading and for handing over
        property bool showScenarios: false  // the what-if panel
        property int seriesScenario: -1     // >=0 makes the recurring form build a hypothetical
        property string seriesScenarioName: ""

        // Switching a scenario on or off is just a change to the argument the projection is given;
        // nothing is written. Shared by the chips above the chart and the scenario panel, so the
        // two cannot drift.
        function toggleScenario(id) {
            const next = win.activeScenarios.slice();
            const at = next.indexOf(id);
            if (at >= 0) next.splice(at, 1); else next.push(id);
            win.activeScenarios = next;
            win.refresh();
        }

        function refresh() {
            Ledger.request("account.balances", {}, (r, e) => {
                if (!e) {
                    win.accounts = r || [];
                    if (win.focusAccount < 0 && win.accounts.length > 0)
                        win.focusAccount = win.accounts[0].account_id;
                }
            });
            Ledger.request("scenario.list", {}, (r, e) => { if (!e) win.scenarios = r || []; });
            Ledger.request("account.list", {}, (r, e) => { if (!e) win.allAccounts = r || []; });
            Ledger.request("currency.list", {}, (r, e) => { if (!e) win.currencies = r || []; });
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
                    onClicked: { win.showEntry = !win.showEntry; if (win.showEntry) win.showSeries = false; }
                }
                PushButton {
                    label: win.showSeries ? "Close" : "+ recurring"
                    onClicked: { win.showSeries = !win.showSeries;
                                 // Always baseline: a scenario left over from a what-if would
                                 // silently make a real commitment hypothetical.
                                 win.seriesScenario = -1;
                                 if (win.showSeries) { win.showEntry = false; win.showImport = false;
                                                       win.showScenarios = false; } }
                }
                PushButton {
                    label: win.showImport ? "Close" : "import"
                    onClicked: { win.showImport = !win.showImport;
                                 if (win.showImport) { win.showEntry = false; win.showSeries = false;
                                                       win.showBrief = false; } }
                }
                PushButton {
                    label: win.showPayments ? "Close" : "payments"
                    onClicked: { win.showPayments = !win.showPayments;
                                 if (win.showPayments) { win.showEntry = false; win.showSeries = false;
                                                         win.showImport = false; win.showBrief = false;
                                                         win.showScenarios = false; win.showDanger = false;
                                                         win.showCurrencies = false; win.showJourneys = false; } }
                }
                PushButton {
                    label: win.showJourneys ? "Close" : "chains"
                    onClicked: { win.showJourneys = !win.showJourneys;
                                 if (win.showJourneys) { win.showEntry = false; win.showSeries = false;
                                                         win.showImport = false; win.showBrief = false;
                                                         win.showScenarios = false; win.showDanger = false;
                                                         win.showCurrencies = false; } }
                }
                PushButton {
                    label: win.showScenarios ? "Close" : "what if"
                    onClicked: { win.showScenarios = !win.showScenarios;
                                 if (win.showScenarios) { win.showEntry = false; win.showSeries = false;
                                                          win.showImport = false; win.showBrief = false; } }
                }
                PushButton {
                    label: win.showBrief ? "Close" : "brief"
                    onClicked: { win.showBrief = !win.showBrief;
                                 if (win.showBrief) { win.showEntry = false; win.showSeries = false;
                                                      win.showImport = false; win.showScenarios = false; } }
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
                                onClicked: { newAccount.visible = !newAccount.visible;
                                             if (newAccount.visible) win.editAccounts = false; }
                            }
                            PushButton {
                                label: win.editAccounts ? "done" : "edit"
                                primary: win.editAccounts
                                onClicked: { win.editAccounts = !win.editAccounts;
                                             if (win.editAccounts) newAccount.visible = false; }
                            }
                        }

                        // Creating an account is rare but a fresh book is unusable without it, so
                        // it lives here rather than behind a menu.
                        Rectangle {
                            id: newAccount
                            visible: false
                            Layout.fillWidth: true
                            // Content-driven, not a magic number: at 108 the box was 26px shorter
                            // than the 46 + 6 + 30 + 6 + 30 it holds plus margins, so the Create
                            // and Cancel buttons hung out of the bottom of it.
                            implicitHeight: accountForm.implicitHeight + 16
                            color: Theme.surfaceRaised
                            border.color: Theme.line
                            border.width: 1
                            radius: 6

                            Column {
                                id: accountForm
                                anchors.left: parent.left
                                anchors.right: parent.right
                                anchors.top: parent.top
                                anchors.margins: 8
                                spacing: 6
                                Field {
                                    id: accName
                                    width: parent.width
                                    label: "name"
                                    placeholder: "Current, Rent, Salary…"
                                }
                                // A Flow, not a Row: the four kinds need about 300px of buttons
                                // and this sidebar is 260px wide, so a Row ran them off the edge.
                                // Wrapping is the only thing that fits without abbreviating them.
                                Flow {
                                    width: parent.width
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
                                // An account's currency is fixed by a composite foreign key the
                                // moment it has a posting, so it has to be chosen here rather
                                // than corrected later. It used to default silently to GBP.
                                Flow {
                                    width: parent.width
                                    spacing: 6
                                    Repeater {
                                        model: win.currencies
                                        PushButton {
                                            implicitHeight: 24
                                            label: modelData.code
                                            primary: newAccount.currency === modelData.code
                                            onClicked: newAccount.currency = modelData.code
                                        }
                                    }
                                    PushButton {
                                        implicitHeight: 24
                                        label: "+ currency…"
                                        onClicked: {
                                            win.showEntry = false; win.showSeries = false;
                                            win.showImport = false; win.showBrief = false;
                                            win.showScenarios = false; win.showDanger = false;
                                            win.showCurrencies = true;
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
                                                { name: accName.text, kind: newAccount.kind,
                                                  currency: newAccount.currency },
                                                (r, e) => {
                                                    if (!e) { accName.clear(); newAccount.visible = false; }
                                                });
                                        }
                                    }
                                    PushButton { label: "Cancel"; onClicked: newAccount.visible = false }
                                }
                            }
                            property string kind: "expense"
                            property string currency: "GBP"
                        }

                        AccountAdmin {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            visible: win.editAccounts
                            accounts: win.allAccounts
                            onChanged: win.refresh()
                            onEmptyBookRequested: {
                                win.showEntry = false; win.showSeries = false;
                                win.showImport = false; win.showBrief = false;
                                win.showScenarios = false;
                                win.showDanger = true;
                            }
                        }

                        ListView {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            visible: !win.editAccounts
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
                                        text: Money.format(modelData.balance_minor, modelData.currency)
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
                                        onClicked: win.toggleScenario(modelData.id)
                                    }
                                }
                            }
                        }
                    }
                }
            }

            EntryForm {
                Layout.fillWidth: true
                // No explicit height: the form reports its own, and it changes when the
                // cross-currency row appears.
                visible: win.showEntry
                accounts: win.accounts
                defaultDate: win.asOf
                onSaved: win.showEntry = false
            }

            PaymentsPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 380
                visible: win.showPayments
                accounts: win.accounts
                onDone: win.showPayments = false
            }

            JourneyPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 320
                visible: win.showJourneys
                today: win.asOf
                onDone: win.showJourneys = false
                onChanged: win.refresh()
            }

            CurrencyPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 260
                visible: win.showCurrencies
                onDone: win.showCurrencies = false
                onChanged: win.refresh()
            }

            DangerZone {
                Layout.fillWidth: true
                Layout.preferredHeight: 220
                visible: win.showDanger
                onDone: win.showDanger = false
                onChanged: win.refresh()
            }

            ScenarioPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 430
                visible: win.showScenarios
                active: win.activeScenarios
                onToggled: id => win.toggleScenario(id)
                onChanged: win.refresh()
                // Adding a hypothetical payment reuses the ordinary recurring form rather than a
                // second copy of it -- the only difference is which scenario the row lands in.
                onAddPaymentRequested: (id, name) => {
                    win.seriesScenario = id;
                    win.seriesScenarioName = name;
                    win.showScenarios = false;
                    win.showSeries = true;
                }
            }

            SeriesForm {
                Layout.fillWidth: true
                Layout.preferredHeight: 430
                visible: win.showSeries
                accounts: win.accounts
                defaultDate: win.asOf
                scenarioId: win.seriesScenario
                scenarioName: win.seriesScenarioName
                onSaved: { win.showSeries = false; win.seriesScenario = -1; }
                onCancelled: { win.showSeries = false; win.seriesScenario = -1; }
            }

            AnalysisPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 430
                visible: win.showBrief
                asOf: win.asOf
            }

            ImportPanel {
                Layout.fillWidth: true
                Layout.preferredHeight: 430
                visible: win.showImport
                accounts: win.accounts
                onChanged: win.refresh()
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
                        // An Item, not a RowLayout, because the MouseArea has to cover the whole
                        // row and a layout MANAGES its children's geometry -- anchoring inside one
                        // is undefined behaviour Qt warns about, and this is the row you click to
                        // alter an occurrence, so an unreliable hit area is not cosmetic.
                        delegate: Item {
                            id: occRow
                            width: ListView.view.width
                            implicitHeight: occRowLayout.implicitHeight
                            // Only a real series occurrence can be overridden. Generated interest
                            // and payment legs carry a negative series_id and are not editable:
                            // they are consequences of a rule, not instances of one.
                            readonly property bool editable: modelData.series_id > 0
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: occRow.editable ? Qt.PointingHandCursor : Qt.ArrowCursor
                                onClicked: if (occRow.editable) win.editing = modelData
                            }
                            RowLayout {
                            id: occRowLayout
                            anchors.fill: parent
                            spacing: 10
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
                                text: Money.format(modelData.amount_minor, modelData.currency)
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
}
