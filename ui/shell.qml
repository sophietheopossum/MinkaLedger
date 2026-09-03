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
        property bool showPayments: false
        property var currencies: []
        property int seriesCount: 0
        // Every full-width panel is mutually exclusive, but UPCOMING sat under all of them and
        // its 150px was the difference between the column fitting and the layout squeezing items
        // into each other. It is also the least useful thing on screen while a panel is open.
        readonly property bool panelOpen: win.showEntry || win.showSeries || win.showImport
                                          || win.showBrief || win.showScenarios
                                          || win.showCurrencies || win.showDanger
                                          || win.showPayments || win.showExport
        property var projection: ({ balances: [], occurrences: [] })
        property string asOf: Qt.formatDate(new Date(), "yyyy-MM-dd")
        property string horizon: Qt.formatDate(
            new Date(new Date().setMonth(new Date().getMonth() + 12)), "yyyy-MM-dd")
        // The chart looks back as far as it looks forward, so today sits in the middle.
        property string historyFrom: Qt.formatDate(
            new Date(new Date().setMonth(new Date().getMonth() - 12)), "yyyy-MM-dd")
        // Balance history per open account over that window, from account.history.
        property var history: []
        // The accounts the chart draws: click one to see it alone, shift-click to add or remove,
        // Escape to clear. Empty means every asset, summed per currency -- the household's money
        // as one line, which is what the chart is for when nothing in particular is being asked.
        property var selectedAccounts: []
        readonly property var chartAccountIds: win.selectedAccounts.length > 0
            ? win.selectedAccounts
            : win.accounts.filter(a => a.kind === "asset").map(a => a.account_id)
        property var scenarios: []
        property var activeScenarios: []
        property var editing: null          // the occurrence open in the editor
        property bool showEntry: false      // the record-a-payment form
        property bool showSeries: false     // the new-recurring-payment form
        property bool showImport: false     // the CSV import screen
        property bool showExport: false     // taking a backup, and the two readable exports
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

        function createAccount(params) {
            Ledger.write("account.create", params, (r, e) => {
                if (!e) {
                    accName.clear();
                    accOpening.clear();
                    newAccount.visible = false;
                }
            });
        }

        function refresh() {
            Ledger.request("account.balances", {}, (r, e) => {
                if (!e) {
                    win.accounts = r || [];
                    // An account that went away takes its selection with it. (No const inside
                    // the callback: qmllint silently stops checking a file that has one.)
                    win.selectedAccounts = win.selectedAccounts.filter(
                        id => win.accounts.some(a => a.account_id === id));
                }
            });
            Ledger.request("account.history", { from: win.historyFrom, to: win.asOf },
                           (r, e) => { if (!e) win.history = r || []; });
            Ledger.request("scenario.list", {}, (r, e) => { if (!e) win.scenarios = r || []; });
            Ledger.request("account.list", {}, (r, e) => { if (!e) win.allAccounts = r || []; });
            Ledger.request("currency.list", {}, (r, e) => { if (!e) win.currencies = r || []; });
            Ledger.request("series.list", {}, (r, e) => {
                if (!e) win.seriesCount = (r || []).filter(x => !x.scenario_id).length;
            });
            Ledger.request("forecast.project",
                           { as_of: win.asOf, horizon: win.horizon, scenarios: win.activeScenarios },
                           (r, e) => { if (!e) win.projection = r; });
        }

        Component.onCompleted: win.refresh()
        Connections {
            target: Ledger
            function onRevisionChanged() { win.refresh(); }
        }

        function pickAccount(id, extend) {
            if (!extend) {
                win.selectedAccounts = [id];
                return;
            }
            const next = win.selectedAccounts.slice();
            const at = next.indexOf(id);
            if (at >= 0) next.splice(at, 1); else next.push(id);
            win.selectedAccounts = next;
        }

        // One account's balance line: the history window, then today, then the projection --
        // so the line starts where the money was, passes through where it is, and ends where
        // the rules say it will be.
        //
        // Today's point comes from the history, not from the sidebar's balance: the sidebar
        // counts a post-dated payment already, while history and projection both stop at today,
        // and mixing the two put a one-day spike on the line. When the projection has a point
        // on today it wins, because it already includes whatever is due today.
        function lineFor(accountId) {
            const pts = [];
            const h = win.history.find(a => a.account_id === accountId);
            let today = h ? h.opening_minor : 0;
            if (h) {
                // A movement on the window's first day is a point, not part of the opening.
                if (!(h.points.length > 0 && h.points[0].on === win.historyFrom))
                    pts.push({ on: win.historyFrom, balance_minor: h.opening_minor });
                for (const p of h.points) {
                    if (p.on >= win.historyFrom && p.on <= win.asOf) {
                        pts.push(p);
                        today = p.balance_minor;
                    }
                }
            }
            const projected = (win.projection.balances || []).filter(
                b => b.account_id === accountId && b.on >= win.asOf);
            if (!(projected.length > 0 && projected[0].on === win.asOf)
                && !(pts.length > 0 && pts[pts.length - 1].on === win.asOf))
                pts.push({ on: win.asOf, balance_minor: today });
            for (const b of projected)
                pts.push({ on: b.on, balance_minor: b.balance_minor });
            const acc = win.accounts.find(a => a.account_id === accountId);
            return {
                label: acc ? acc.name : String(accountId),
                currency: acc ? acc.currency : (h ? h.currency : ""),
                points: pts
            };
        }

        // Several lines added up: on every date any of them moves, the sum of each one's latest
        // value. A balance carries forward between its own points, so this is the sum of what
        // every account held that day, not just of the ones that moved.
        function sumLines(members, label, currency) {
            const dates = {};
            const byDate = [];
            for (const m of members) {
                const mine = {};
                for (const p of m.points) {
                    dates[p.on] = true;
                    mine[p.on] = p.balance_minor;
                }
                byDate.push(mine);
            }
            const latest = members.map(() => 0);
            const pts = [];
            for (const on of Object.keys(dates).sort()) {
                let total = 0;
                for (let i = 0; i < members.length; i++) {
                    if (byDate[i][on] !== undefined)
                        latest[i] = byDate[i][on];
                    total += latest[i];
                }
                pts.push({ on: on, balance_minor: total });
            }
            return { label: label, currency: currency, points: pts };
        }

        readonly property var chartLines: {
            if (win.selectedAccounts.length > 0)
                return win.selectedAccounts.map(id => win.lineFor(id));
            // Every asset, one summed line per currency: money in different currencies is not
            // one number, and pretending it is would be worse than two lines.
            const byCurrency = {};
            for (const a of win.accounts.filter(a => a.kind === "asset"))
                (byCurrency[a.currency] = byCurrency[a.currency] || []).push(win.lineFor(a.account_id));
            return Object.keys(byCurrency).sort().map(cur =>
                win.sumLines(byCurrency[cur], "all assets", cur));
        }
        readonly property string chartCurrency: win.chartLines.length > 0 ? win.chartLines[0].currency : "GBP"

        // Escape clears the selection. A window Shortcut sees the key BEFORE any item does and
        // consumes it, so it is switched off whenever a panel is open: that is where the
        // pickers live, and Escape there already means "close the list without picking".
        Shortcut {
            sequences: ["Escape"]
            enabled: !win.panelOpen && win.selectedAccounts.length > 0
            onActivated: {
                // Quickshell's window has no activeFocusItem of its own; the attached Window
                // property on any item inside it does, the same way the pickers read it.
                const it = body.Window.activeFocusItem;
                if (it && it.hasOwnProperty("cursorPosition"))
                    return;
                win.selectedAccounts = [];
            }
        }

        ColumnLayout {
            id: body
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
                    onClicked: { win.showEntry = !win.showEntry;
                                 if (win.showEntry) { win.showSeries = false; win.showExport = false; } }
                }
                PushButton {
                    label: win.showSeries ? "Close" : "+ recurring"
                    onClicked: { win.showSeries = !win.showSeries;
                                 // Always baseline: a scenario left over from a what-if would
                                 // silently make a real commitment hypothetical.
                                 win.seriesScenario = -1;
                                 if (win.showSeries) { win.showEntry = false; win.showImport = false;
                                                       win.showScenarios = false; win.showExport = false; } }
                }
                PushButton {
                    label: win.showImport ? "Close" : "import"
                    onClicked: { win.showImport = !win.showImport;
                                 if (win.showImport) { win.showEntry = false; win.showSeries = false;
                                                       win.showBrief = false; win.showExport = false; } }
                }
                // Next to import, because it is the same question pointed the other way. Every
                // other panel closes: a backup is wanted when something has gone wrong or is
                // about to, and a column squeezed between two screens is the last thing needed.
                PushButton {
                    label: win.showExport ? "Close" : "back up"
                    onClicked: { win.showExport = !win.showExport;
                                 if (win.showExport) { win.showEntry = false; win.showSeries = false;
                                                       win.showImport = false; win.showBrief = false;
                                                       win.showScenarios = false; win.showDanger = false;
                                                       win.showCurrencies = false; win.showPayments = false; } }
                }
                PushButton {
                    label: win.showPayments ? "Close" : "payments"
                    onClicked: { win.showPayments = !win.showPayments;
                                 if (win.showPayments) { win.showEntry = false; win.showSeries = false;
                                                         win.showImport = false; win.showBrief = false;
                                                         win.showScenarios = false; win.showDanger = false;
                                                         win.showCurrencies = false; win.showExport = false; } }
                }
                PushButton {
                    label: win.showScenarios ? "Close" : "what if"
                    onClicked: { win.showScenarios = !win.showScenarios;
                                 if (win.showScenarios) { win.showEntry = false; win.showSeries = false;
                                                          win.showImport = false; win.showBrief = false;
                                                          win.showExport = false; } }
                }
                PushButton {
                    label: win.showBrief ? "Close" : "brief"
                    onClicked: { win.showBrief = !win.showBrief;
                                 if (win.showBrief) { win.showEntry = false; win.showSeries = false;
                                                      win.showImport = false; win.showScenarios = false;
                                                      win.showExport = false; } }
                }
                Text {
                    text: Ledger.running ? "core up" : "core down"
                    color: Ledger.running ? Theme.textFaint : Theme.red
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 1
                }
            }

            // Accounts and the chart step aside while a panel is open. Every panel is really a
            // full screen -- the recurring form alone is ~470px -- and competing for a 773px
            // window is what made the layout squeeze items into each other. You are not reading
            // the forecast while filling in a form.
            RowLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                visible: !win.panelOpen
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
                                            win.showExport = false; win.showCurrencies = true;
                                        }
                                    }
                                }
                                // Only for the kinds that can HOLD anything. An income or
                                // expense account is a running total of flow and starts at zero
                                // by definition, so offering it a starting balance would invite
                                // an entry that means nothing.
                                Field {
                                    id: accOpening
                                    visible: newAccount.kind === "asset"
                                             || newAccount.kind === "liability"
                                    width: parent.width
                                    numeric: true
                                    label: newAccount.kind === "liability"
                                           ? "currently owed (optional)"
                                           : "starting balance (optional)"
                                    placeholder: "0.00"
                                }
                                Row {
                                    spacing: 6
                                    PushButton {
                                        label: "Create"
                                        primary: true
                                        enabled: accName.text.length > 0
                                        onClicked: {
                                            const params = { name: accName.text,
                                                             kind: newAccount.kind,
                                                             currency: newAccount.currency };
                                            const typed = accOpening.text.trim();
                                            if (accOpening.visible && typed.length > 0) {
                                                // Parsed by the core at the account's own scale,
                                                // then signed by kind: "currently owed" is a
                                                // NEGATIVE balance, which is the one place the
                                                // phrasing and the arithmetic disagree.
                                                Ledger.request("money.parse",
                                                    { text: typed,
                                                      minor_digits: Money.digits(newAccount.currency) },
                                                    (pr, pe) => {
                                                        if (pe) { accOpening.invalid = true; return; }
                                                        accOpening.invalid = false;
                                                        params.opening_minor =
                                                            newAccount.kind === "liability"
                                                            ? -Math.abs(pr.minor) : pr.minor;
                                                        params.opening_on = win.asOf;
                                                        win.createAccount(params);
                                                    });
                                                return;
                                            }
                                            win.createAccount(params);
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
                            today: win.asOf
                            onChanged: win.refresh()
                            onEmptyBookRequested: {
                                win.showEntry = false; win.showSeries = false;
                                win.showImport = false; win.showBrief = false;
                                win.showScenarios = false; win.showExport = false;
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
                                color: win.selectedAccounts.indexOf(modelData.account_id) >= 0
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
                                    // Shift adds to (or takes from) the selection; a plain click
                                    // replaces it.
                                    onClicked: mouse => win.pickAccount(modelData.account_id,
                                                                        (mouse.modifiers & Qt.ShiftModifier) !== 0)
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
                                text: "BALANCE"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                            Text {
                                text: win.selectedAccounts.length === 0
                                      ? "all assets · click an account, shift-click for more"
                                      : "shift-click to add or remove, Esc for all assets"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 4
                            }
                            Item { Layout.fillWidth: true }
                            Text {
                                text: win.historyFrom + "  to  " + win.horizon
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 2
                            }
                        }

                        // One chip per line: its colour, its name, and where it ends up.
                        Flow {
                            Layout.fillWidth: true
                            spacing: 10
                            Repeater {
                                model: win.chartLines
                                Row {
                                    id: chip
                                    required property var modelData
                                    required property int index
                                    spacing: 5
                                    readonly property int endMinor: chip.modelData.points.length > 0
                                        ? chip.modelData.points[chip.modelData.points.length - 1].balance_minor : 0
                                    Rectangle {
                                        anchors.verticalCenter: parent.verticalCenter
                                        width: 10
                                        height: 3
                                        radius: 1
                                        // The same rule the chart uses: a lone line that ends
                                        // below zero is drawn red.
                                        color: win.chartLines.length === 1 && chip.endMinor < 0
                                               ? Theme.red
                                               : Theme.seriesPalette[chip.index % Theme.seriesPalette.length]
                                    }
                                    Text {
                                        text: chip.modelData.label
                                              + (win.chartLines.length > 1 || win.selectedAccounts.length === 0
                                                 ? " · " + chip.modelData.currency : "")
                                        color: Theme.textMuted
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                    Text {
                                        text: Money.format(chip.endMinor, chip.modelData.currency)
                                        color: chip.endMinor < 0 ? Theme.red : Theme.textFaint
                                        font.family: Theme.monoFamily
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                }
                            }
                        }

                        ForecastChart {
                            Layout.fillWidth: true
                            Layout.fillHeight: true
                            lines: win.chartLines
                            currency: win.chartCurrency
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
                Layout.fillHeight: true
                visible: win.showPayments
                accounts: win.accounts
                onDone: win.showPayments = false
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

            // The recurring-payments screen both LISTS and creates, like the accounts sidebar:
            // one button, and an existing rule can be bounded without hunting for it.
            SeriesList {
                id: seriesList
                Layout.fillWidth: true
                // Deliberately tight: the creation form below is ~390px, and on a 773px
                // window every row here comes off its Create button. Scrolls past three.
                //
                // The row height comes FROM the list rather than being restated here. Restating it
                // is what clipped the rename editor at one series: a row can be taller than 30 now,
                // and a formula that did not know it left the description field half outside a
                // viewport that could not be scrolled far enough to reach the rest.
                Layout.preferredHeight: Math.min(150, 22 + seriesList.rowHeight * win.seriesCount
                                                      + seriesList.extraHeight)
                visible: win.showSeries && win.seriesCount > 0
                today: win.asOf
                onChanged: win.refresh()
            }

            SeriesForm {
                Layout.fillWidth: true
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

            // Writes nothing to the book, so it takes no onChanged: a snapshot is a read, and
            // refreshing the window after one would only redraw the same numbers.
            ExportPanel {
                Layout.fillWidth: true
                // No explicit height: the panel reports its own, and it changes when a result
                // line or the next-name offer appears.
                visible: win.showExport
                horizon: win.horizon
                onDone: win.showExport = false
            }

            OccurrenceEditor {
                Layout.fillWidth: true
                Layout.preferredHeight: 190
                occurrence: win.editing
                onChanged: win.editing = null
                onDismissed: win.editing = null
            }

            // Soaks up whatever the visible panel does not want, so panels sit at the top
            // rather than floating in the middle of the column.
            Item {
                Layout.fillWidth: true
                Layout.fillHeight: true
                visible: win.panelOpen && !win.showPayments
            }

            // ---- what is coming ----
            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: 150
                visible: !win.panelOpen
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
                        // One line per occurrence on the accounts the chart is drawing, in
                        // date order: the selection, or every asset when there is none.
                        model: (win.projection.occurrences || []).filter(
                            o => win.chartAccountIds.indexOf(o.account_id) >= 0)
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
                                // With several accounts on the chart a row has to say whose it
                                // is: a transfer between two of them is two rows, one per side.
                                text: (win.chartAccountIds.length > 1 ? modelData.account + ": " : "")
                                       + modelData.description
                                       + (modelData.chain_len
                                          ? "  ⛓ " + (modelData.chain_seq + 1) + "/" + modelData.chain_len : "")
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
