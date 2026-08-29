pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Dialogs
import "../services"

// Import a bank CSV: pick, map, review, commit.
//
// THE REVIEW STEP IS THE FEATURE. The core never writes to the ledger on stage, so a whole file
// sits in front of you as rows you can uncheck or re-file before anything happens — and `revert`
// undoes a committed batch exactly. That is GnuCash's shape, and it is what makes importing a
// statement a low-stakes thing to try rather than something to be careful about.
//
// MAPPING USES THE FILE'S REAL HEADERS. `import.peek` reads them without staging, so the columns
// are picked from a list rather than typed from memory — a mistyped header silently yields a
// column of nulls, which is the worst kind of import bug because it looks like it worked.
Rectangle {
    id: root

    property var accounts: []
    signal changed

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property string path: ""
    property var headers: []
    property var sample: []
    property int totalLines: 0
    property var profiles: []
    property int profileId: -1
    property int batchId: -1
    property var rows: []
    property string note: ""
    property int intoAccount: -1

    // The mapping being built, keyed by ledger field.
    property var map: ({ date: "", amount: "", description: "" })

    Component.onCompleted: root.loadProfiles()

    function loadProfiles() {
        Ledger.request("import.profiles", {}, (r, e) => { if (!e) root.profiles = r || []; });
    }

    function peek(p) {
        root.path = p;
        root.batchId = -1;
        root.rows = [];
        Ledger.request("import.peek", { path: p }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.headers = r.headers;
            root.sample = r.sample;
            root.totalLines = r.total_lines;
            root.note = r.total_lines + " rows in the file";
            // A profile whose mapping fits these headers is almost certainly the right one.
            for (const pr of root.profiles) {
                const m = pr.mapping;
                if (m && r.headers.indexOf(m.date) >= 0
                      && (r.headers.indexOf(m.amount) >= 0 || r.headers.indexOf(m.money_in) >= 0)) {
                    root.profileId = pr.id;
                    root.note += " · matched profile “" + pr.name + "”";
                    return;
                }
            }
            root.profileId = -1;
        });
    }

    function createProfile(name) {
        Ledger.write("import.create_profile", {
            name: name, date_format: "%d/%m/%Y", account_id: root.intoAccount,
            mapping: { date: root.map.date, amount: root.map.amount,
                       description: root.map.description }
        }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.profileId = r.id;
            root.loadProfiles();
            root.note = "profile saved — it will be matched automatically next time";
        });
    }

    function stage() {
        Ledger.write("import.stage", { profile_id: root.profileId, path: root.path,
                                       source_name: root.path.split("/").pop() }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.batchId = r.batch_id;
            root.note = r.rows + " read · " + r.new + " new · " + r.duplicates
                      + " already imported · " + r.errors + " unreadable";
            Ledger.write("import.categorise", { batch_id: r.batch_id }, () => root.loadRows());
        });
    }

    function loadRows() {
        Ledger.request("import.rows", { batch_id: root.batchId }, (r, e) => {
            if (!e) root.rows = r || [];
        });
    }

    function commit() {
        Ledger.write("import.commit", { batch_id: root.batchId }, (r, e) => {
            root.note = e ? e.message : (r.created + " transactions created");
            if (!e) { root.loadRows(); root.changed(); }
        });
    }

    function revert() {
        Ledger.write("import.revert", { batch_id: root.batchId }, (r, e) => {
            root.note = e ? e.message : (r.removed + " transactions removed — the ledger is as it was");
            if (!e) { root.loadRows(); root.changed(); }
        });
    }

    FileDialog {
        id: chooser
        title: "Choose a bank export"
        nameFilters: ["CSV files (*.csv)", "All files (*)"]
        onAccepted: root.peek(selectedFile.toString().replace("file://", ""))
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Row {
            spacing: 8
            width: parent.width
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "IMPORT A STATEMENT"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            PushButton { label: "Choose file…"; primary: root.path === ""; onClicked: chooser.open() }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: root.path === "" ? "" : root.path.split("/").pop()
                color: Theme.text
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }

        // ---- mapping, only while there is no profile for this file ----
        Column {
            visible: root.headers.length > 0 && root.profileId < 0 && root.batchId < 0
            width: parent.width
            spacing: 6

            Text {
                text: "no saved profile fits this file — say which column is which"
                color: Theme.warnAmber
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }

            Repeater {
                model: [
                    { field: "date",        caption: "date" },
                    { field: "amount",      caption: "amount" },
                    { field: "description", caption: "description" }
                ]
                Row {
                    id: mapRow
                    required property var modelData
                    spacing: 6
                    Text {
                        width: 80
                        anchors.verticalCenter: parent.verticalCenter
                        text: mapRow.modelData.caption
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                    Repeater {
                        model: root.headers
                        PushButton {
                            required property var modelData
                            label: modelData
                            primary: root.map[mapRow.modelData.field] === modelData
                            onClicked: {
                                const next = root.map;
                                next[mapRow.modelData.field] = modelData;
                                root.map = next;
                            }
                        }
                    }
                }
            }

            Row {
                spacing: 6
                AccountPicker {
                    width: 220
                    label: "these rows belong to"
                    accounts: root.accounts
                    onPicked: id => root.intoAccount = id
                }
                PushButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "Save profile"
                    primary: true
                    enabled: root.map.date !== "" && root.map.amount !== "" && root.intoAccount >= 0
                    onClicked: root.createProfile(root.path.split("/").pop().replace(".csv", ""))
                }
            }
        }

        Row {
            spacing: 8
            visible: root.profileId >= 0 && root.batchId < 0
            PushButton { label: "Read the file"; primary: true; onClicked: root.stage() }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "nothing is written until you commit"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
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

        // ---- the review table ----
        Rectangle {
            visible: root.batchId >= 0
            width: parent.width
            height: 210
            color: Theme.ground
            border.width: 1
            border.color: Theme.line
            radius: 5

            ListView {
                anchors.fill: parent
                anchors.margins: 4
                clip: true
                model: root.rows
                delegate: Rectangle {
                    id: row
                    required property var modelData
                    width: ListView.view.width
                    height: 22
                    color: hov.containsMouse ? Theme.surface : "transparent"

                    readonly property bool committed: row.modelData.state === "committed"
                    readonly property bool dup: row.modelData.state === "duplicate"
                    readonly property bool bad: row.modelData.state === "error"

                    Row {
                        anchors.fill: parent
                        anchors.leftMargin: 4
                        spacing: 8
                        Text {
                            width: 16
                            anchors.verticalCenter: parent.verticalCenter
                            // A duplicate or an unreadable row cannot be accepted, so it shows why
                            // rather than an unchecked box that looks like a choice.
                            text: row.bad ? "!" : row.dup ? "=" : (row.modelData.accepted ? "✓" : "·")
                            color: row.bad ? Theme.red : row.dup ? Theme.textFaint
                                 : row.modelData.accepted ? Theme.okGreen : Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 1
                        }
                        Text {
                            width: 78
                            anchors.verticalCenter: parent.verticalCenter
                            text: row.modelData.occurred_on || "—"
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            width: 210
                            anchors.verticalCenter: parent.verticalCenter
                            elide: Text.ElideRight
                            text: row.modelData.error || row.modelData.description
                            color: row.bad ? Theme.red : Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            width: 80
                            horizontalAlignment: Text.AlignRight
                            anchors.verticalCenter: parent.verticalCenter
                            text: row.modelData.amount_minor === null ? ""
                                  : (row.modelData.amount_minor / 100).toFixed(2)
                            color: (row.modelData.amount_minor || 0) < 0 ? Theme.red : Theme.okGreen
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: row.modelData.far_account || "unclassified"
                            color: row.modelData.far_account ? Theme.textMuted : Theme.warnAmber
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                    }

                    MouseArea {
                        id: hov
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: (row.dup || row.bad || row.committed)
                                     ? Qt.ArrowCursor : Qt.PointingHandCursor
                        onClicked: {
                            if (row.dup || row.bad || row.committed)
                                return;
                            Ledger.write("import.set_row",
                                { id: row.modelData.id, accepted: !row.modelData.accepted },
                                () => root.loadRows());
                        }
                    }
                }
            }
        }

        Row {
            spacing: 8
            visible: root.batchId >= 0
            PushButton { label: "Commit"; primary: true; onClicked: root.commit() }
            PushButton { label: "Revert"; onClicked: root.revert() }
            PushButton {
                label: "Start again"
                onClicked: {
                    root.batchId = -1; root.rows = []; root.path = "";
                    root.headers = []; root.note = "";
                }
            }
        }
    }
}
