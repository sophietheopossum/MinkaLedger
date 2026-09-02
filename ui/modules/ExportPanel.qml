pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Dialogs
import QtCore
import "../services"

// Getting a copy of the book out of the app.
//
// THREE EXPORTS, ONLY ONE OF WHICH IS A BACKUP, and the panel is arranged around that fact rather
// than treating them as three equal buttons. `export.snapshot` is `VACUUM INTO`: a real SQLite
// database that opens as this book, taken safely while the core has it open. The JSON archive and
// the CSV are readable and hand-overable but there is no route back from either — a book cannot be
// rebuilt from them. Presenting all three as "export" is how someone ends up holding a CSV and
// discovering, at the moment they need it, that it was never a backup.
//
// THE SNAPSHOT FILE IS SAFE; THE OBVIOUS WAY TO PUT IT BACK IS NOT. The book runs in WAL mode
// (db.rs sets journal_mode=WAL), so after a crash or a kill there is a `-wal` beside `book.db`
// holding everything since the last checkpoint — her live book right now is 262 KB of `book.db`
// against 2.7 MB of `book.db-wal`. Copy a snapshot over `book.db` with that `-wal` still there and
// SQLite replays the orphaned frames onto the fresh copy. Measured on this machine (SQLite 3.53.4):
// because VACUUM INTO re-lays-out the pages, the frames land on the wrong ones and the restored
// book comes out MALFORMED — `PRAGMA integrity_check` lists wrong index entries and missing rows —
// while `db.check` returns ok:true and the app opens it without complaint. The first open then
// checkpoints and deletes the sidecars, so the damage is permanent and nothing looks wrong
// afterwards. That is why the restore text below is a procedure with an order to it and not a
// reassurance: every signal this app has says the broken restore worked.
//
// THE SNAPSHOT REFUSES TO OVERWRITE (export.rs: `if Path::new(path).exists()`), on purpose — a
// backup that can silently destroy the previous backup is a worse backup. That refusal is the one
// awkward edge in the UI, and it is handled in two places rather than ignored:
//
//   1. The suggested name is dated (ISO so it sorts) and carries a counter, so a second backup on
//      the same day is a new file rather than a collision. The counter advances after every write.
//   2. When a collision happens anyway, THE CORE'S REFUSAL IS THE TEST. There is deliberately no
//      "does this file exist" probe from QML: the core checks immediately before it writes, so any
//      probe from here is a race that can disagree, and the refusal costs nothing because it
//      happens before a single byte is written. For the name this panel generated we simply take
//      the next one and try again; for a path the user picked themselves we never rename behind
//      their back — the message is shown and the next free name is offered as one click.
//
// EVERY CHOOSER WRITES. The file dialog is `SaveFile`, and its confirm button says Save; a chooser
// that only remembered a path would be the panel's own failure mode (believing you hold a backup
// you never took) reached from the backup half. So all three choosers write on accept and are
// labelled for what they do. The one thing the dialog can offer that this panel will not honour is
// replacing an existing backup — the collision message says so rather than leaving it unexplained.
Rectangle {
    id: root

    signal done

    /// The window's forecast horizon, offered to the JSON archive as `forecast_to`.
    property string horizon: ""

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8
    // Content-driven, like EntryForm: the panel GROWS by a result line and again by the
    // next-name offer, and a fixed height that fits without them clips exactly the two things
    // there is any point reading.
    implicitHeight: form.implicitHeight + 24

    // ---- where things are written ----

    /// Chosen once at load: Documents if the platform has one, else home. Never the repo.
    property string folder: ""
    // The readable exports keep their OWN folder. Sharing `folder` meant that nominating a place
    // for the backup silently moved the JSON and the CSV there too — and those overwrite in
    // silence (the core's fs::write truncates), so a folder picked only ever for a backup could
    // lose a same-named file to a later CSV. Nothing on screen would have shown the move.
    property string sideFolder: ""
    property string dateStamp: ""

    // The backup target, held as stem + counter + extension rather than parsed back out of a
    // path. Parsing is what breaks here: a greedy "-(digits) at the end" reads the 02 of
    // minka-ledger-2026-09-02 as the counter and produces 2026-09-3.
    property string stem: ""
    property string ext: ".db"
    property int attempt: 1
    /// True once the path came from the file dialog: those are never silently renamed.
    property bool chosen: false

    readonly property string target: root.folder.length === 0 ? ""
        : root.folder + "/" + root.stem + (root.attempt > 1 ? "-" + root.attempt : "") + root.ext

    property string result: ""
    property bool failed: false
    /// Set when the snapshot was refused because the file was already there, so the panel can
    /// offer the next name instead of leaving a dead end.
    property bool collided: false

    property string sideResult: ""
    property bool sideFailed: false
    property bool busy: false

    property bool redact: false
    property bool withForecast: false

    Component.onCompleted: root.reset()
    onVisibleChanged: if (root.visible) root.reset()

    function reset() {
        if (root.folder.length === 0)
            root.folder = root.defaultFolder();
        if (root.sideFolder.length === 0)
            root.sideFolder = root.defaultFolder();
        const today = Qt.formatDate(new Date(), "yyyy-MM-dd");
        // A session left open across midnight would otherwise keep yesterday's stem and start
        // its counter at whatever the day before reached.
        if (today !== root.dateStamp) {
            root.dateStamp = today;
            root.stem = "minka-ledger-" + today;
            root.ext = ".db";
            root.attempt = 1;
            root.chosen = false;
        }
        root.result = "";
        root.failed = false;
        root.collided = false;
        root.sideResult = "";
        root.sideFailed = false;
        root.busy = false;
    }

    function defaultFolder() {
        const docs = root.toLocalPath(StandardPaths.writableLocation(StandardPaths.DocumentsLocation));
        if (docs.length > 0)
            return docs;
        return root.toLocalPath(StandardPaths.writableLocation(StandardPaths.HomeLocation));
    }

    // A file: URL is percent-encoded; the core wants a plain path. ImportPanel strips the scheme
    // the same way, but a folder with a space in it arrives here as %20 and would be written as a
    // literal one, so the decode is not optional.
    function toLocalPath(u) {
        const s = String(u);
        if (s.indexOf("file://") !== 0)
            return s;
        const raw = s.substring(7);
        try {
            return decodeURIComponent(raw);
        } catch (err) {
            return raw;
        }
    }

    // Per SEGMENT, and encodeURIComponent rather than encodeURI. encodeURI deliberately leaves the
    // URL-structural characters alone -- including "#" and "?" -- and Qt then reads everything after
    // one of them as a fragment or a query rather than as part of the path. A folder called
    // "Tax #2026" opened the chooser on its PARENT, with the name mangled into "Tax.db#2026/...".
    // Same class of bug as the bare file:// strip on the way back in, pointing outward.
    function toUrl(p) {
        if (p.length === 0)
            return "";
        return "file://" + p.split("/").map(encodeURIComponent).join("/");
    }

    /// Shorten for display only — a home-relative path is what she would call the place.
    function pretty(p) {
        const home = root.toLocalPath(StandardPaths.writableLocation(StandardPaths.HomeLocation));
        return (home.length > 0 && p.indexOf(home + "/") === 0)
               ? "~" + p.substring(home.length) : p;
    }

    function suggestedFor(kind) {
        const day = Qt.formatDate(new Date(), "yyyy-MM-dd");
        if (kind === "snapshot")
            return root.stem + (root.attempt > 1 ? "-" + root.attempt : "") + root.ext;
        return "minka-ledger-" + day + (kind === "bundle" ? ".json" : ".csv");
    }

    /// Adopt a path the user picked: split it once, here, so the counter machinery still works.
    function adopt(path) {
        const slash = path.lastIndexOf("/");
        const dot = path.lastIndexOf(".");
        root.folder = slash > 0 ? path.substring(0, slash) : root.folder;
        const name = path.substring(slash + 1);
        const hasExt = dot > slash;
        root.stem = hasExt ? path.substring(slash + 1, dot) : name;
        root.ext = hasExt ? path.substring(dot) : ".db";
        root.attempt = 1;
        root.chosen = true;
    }

    /// The readable half keeps only the FOLDER: the file names are dated and regenerated, and the
    /// one just written is named in the result line, so there is nothing about a chosen name worth
    /// remembering — and no path that could quietly become the target of a later click.
    function adoptSide(path) {
        const slash = path.lastIndexOf("/");
        if (slash > 0)
            root.sideFolder = path.substring(0, slash);
    }

    // ---- the backup ----

    function snapshot() {
        if (root.busy || root.target.length === 0)
            return;
        // Ledger.request() QUEUES when the core is down and never calls back, so without this the
        // button would sit on "Backing up…" forever with no error and no timeout — reading as
        // "a backup is in progress" in exactly the situation that made someone reach for one.
        if (!Ledger.running) {
            root.failed = true;
            root.collided = false;
            root.result = "The ledger core is not running — no backup can be taken.";
            return;
        }
        root.busy = true;
        root.result = "";
        root.failed = false;
        root.collided = false;
        root._snapshotStep();
    }

    function _snapshotStep() {
        const path = root.target;
        Ledger.request("export.snapshot", { path: path }, (r, e) => {
            if (!e) {
                root.busy = false;
                root.failed = false;
                root.collided = false;
                // The next backup should not open on a name that has just been taken.
                root.attempt = root.attempt + 1;
                root.result = "Backed up to " + root.pretty(r.written)
                            + " — a real book. To restore it later: quit the app, delete any "
                            + "-wal and -shm files beside your book, then copy this over it.";
                return;
            }
            const gone = e.code === "io" && String(e.message).indexOf("already exists") >= 0;
            // Our own dated name: take the next one. 99 is not a real limit, it is a guard
            // against looping forever if the refusal ever stops meaning what it means here.
            if (gone && !root.chosen && root.attempt < 99) {
                root.attempt = root.attempt + 1;
                root._snapshotStep();
                return;
            }
            root.busy = false;
            root.failed = true;
            root.collided = gone;
            if (gone) {
                // Not a rename behind her back: the offer below says the name it would use. The
                // last clause is there because the file chooser's own "Replace?" prompt can have
                // just offered the opposite — that offer is the system's, not this app's.
                root.attempt = root.attempt + 1;
                root.result = root.pretty(path) + " is already there. A backup never overwrites "
                            + "one that exists — that is deliberate, so an accidental second "
                            + "backup cannot destroy the first, even if the file chooser offered "
                            + "to replace it.";
            } else {
                root.result = e.message;
            }
        });
    }

    // ---- the two readable exports ----

    function optionsFor(kind) {
        const opt = { path: root.sideFolder + "/" + root.suggestedFor(kind) };
        const f = fromField.text.trim();
        const t = toField.text.trim();
        if (f.length > 0) opt.from = f;
        if (t.length > 0) opt.to = t;
        if (root.redact) opt.redact = true;
        // Only the bundle: the core hard-codes forecast_to: None for the CSV (main.rs), which is
        // why the toggle is labelled for the JSON rather than sitting over both buttons unqualified.
        if (kind === "bundle" && root.withForecast && root.horizon.length > 0)
            opt.forecast_to = root.horizon;
        return opt;
    }

    function writeSide(kind, path) {
        if (!Ledger.running) {
            root.sideFailed = true;
            root.sideResult = "The ledger core is not running — nothing can be written.";
            return;
        }
        const opt = root.optionsFor(kind);
        if (path !== undefined)
            opt.path = path;
        root.sideResult = "";
        root.sideFailed = false;
        Ledger.request(kind === "bundle" ? "export.bundle" : "export.csv", opt, (r, e) => {
            if (e) {
                root.sideFailed = true;
                root.sideResult = e.message;
                return;
            }
            root.sideFailed = false;
            const n = kind === "bundle" ? r.lines : r.rows;
            root.sideResult = "Wrote " + root.pretty(r.written) + " — " + n + " "
                            + (kind === "bundle"
                               ? (n === 1 ? "ledger line" : "ledger lines")
                               : (n === 1 ? "row" : "rows")) + ".";
        });
    }

    /// The dialog opens ON the name that would be written anyway, so choosing a place is a
    /// change of mind rather than a step — and nobody has to type a path to use this panel.
    function openChooser(kind) {
        const dir = kind === "snapshot" ? root.folder : root.sideFolder;
        chooser.kind = kind;
        chooser.currentFolder = root.toUrl(dir);
        chooser.currentFile = root.toUrl(dir + "/" + root.suggestedFor(kind));
        chooser.open();
    }

    // The core dying mid-flight is the other way `busy` gets stranded: Ledger's onRunningChanged
    // calls rpc.reset(), which drops every outstanding callback uninvoked. Watching `running`
    // rather than running a timeout keeps a slow VACUUM INTO from being reported as a failure.
    Connections {
        target: Ledger
        function onRunningChanged() {
            if (!Ledger.running && root.busy) {
                root.busy = false;
                root.failed = true;
                root.collided = false;
                root.result = "The ledger core stopped before it confirmed the backup — treat any "
                            + "file of that name as unfinished.";
            }
        }
    }

    FileDialog {
        id: chooser
        property string kind: "snapshot"
        title: chooser.kind === "snapshot" ? "Where to write the backup"
             : chooser.kind === "bundle" ? "Where to write the JSON archive"
                                         : "Where to write the CSV"
        fileMode: FileDialog.SaveFile
        defaultSuffix: chooser.kind === "snapshot" ? "db"
                     : chooser.kind === "bundle" ? "json" : "csv"
        nameFilters: chooser.kind === "snapshot"
                     ? ["SQLite book (*.db)", "All files (*)"]
                     : chooser.kind === "bundle"
                       ? ["JSON (*.json)", "All files (*)"]
                       : ["CSV (*.csv)", "All files (*)"]
        // All three write. The dialog's confirm button says Save, so accepting it and getting no
        // file was the one outcome this panel must not produce.
        onAccepted: {
            const path = root.toLocalPath(chooser.selectedFile);
            if (chooser.kind === "snapshot") {
                root.adopt(path);
                root.snapshot();
            } else {
                root.adoptSide(path);
                root.writeSide(chooser.kind, path);
            }
        }
    }

    Column {
        id: form
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 12
        spacing: 8

        Item {
            width: parent.width
            height: closeButton.height
            Text {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "BACK UP THIS BOOK"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            PushButton {
                id: closeButton
                anchors.right: parent.right
                label: "Close"
                onClicked: root.done()
            }
        }

        // At the TOP because it governs both halves of the panel; the readable exports are far
        // enough down that a note beside them would be missed.
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: !Ledger.running
            text: "The ledger core is not running — nothing here can be written until it is back."
            color: Theme.warnAmber
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: "A complete, consistent copy of the whole book, taken with SQLite's own "
                + "VACUUM INTO — safe to make while the app is running. This is the one export "
                + "you can restore from."
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        // The panel's one genuinely dangerous instruction, so it is a procedure with an order to
        // it rather than a promise. See the file header for the measurement behind it.
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: "TO RESTORE, THE ORDER MATTERS. Quit MinkaLedger first. If files ending -wal and "
                + "-shm are still sitting beside your book, it did not exit cleanly — delete those "
                + "two BEFORE copying, or SQLite replays them onto the fresh copy and corrupts it, "
                + "and this app will still open it and still call it healthy. Simpler and safer: "
                + "point MINKA_LEDGER_DB at the backup and open it where it is. A snapshot has no "
                + "-wal beside it, so there is nothing to replay."
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            id: backupRow
            width: parent.width
            spacing: 8
            PushButton {
                id: backupButton
                anchors.verticalCenter: parent.verticalCenter
                label: root.busy ? "Backing up…" : "Back up now"
                primary: true
                enabled: !root.busy && root.target.length > 0 && Ledger.running
                onClicked: root.snapshot()
            }
            PushButton {
                id: chooseButton
                anchors.verticalCenter: parent.verticalCenter
                // Says what accepting the dialog will do, because accepting it writes.
                label: "Back up as…"
                enabled: !root.busy && Ledger.running
                onClicked: root.openChooser("snapshot")
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                // The whole path, elided in the MIDDLE: the folder and the file name are both
                // load-bearing here and tail-eliding hides the one that changes.
                width: Math.max(0, backupRow.width - backupButton.width - chooseButton.width - 16)
                elide: Text.ElideMiddle
                text: root.pretty(root.target)
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.result.length > 0
            text: root.result
            color: root.failed ? Theme.red : Theme.okGreen
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        // The way out of the one dead end this panel has. Shown only after the core refused an
        // existing path, and it names the file it will write rather than "try again".
        PushButton {
            visible: root.collided
            label: "Write " + root.target.split("/").pop() + " instead"
            primary: true
            enabled: !root.busy && Ledger.running
            onClicked: root.snapshot()
        }

        Rectangle {
            width: parent.width
            height: 1
            color: Theme.line
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: "READABLE COPIES — neither of these can be restored into the app, and each "
                + "replaces a same-day file of the same name rather than refusing."
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Row {
            width: parent.width
            spacing: 8
            Field {
                id: fromField
                width: 120
                numeric: true
                label: "from (optional)"
                placeholder: "YYYY-MM-DD"
            }
            Field {
                id: toField
                width: 120
                numeric: true
                label: "to (optional)"
                placeholder: "YYYY-MM-DD"
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "redact text"
                primary: root.redact
                onClicked: root.redact = !root.redact
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                // Named for the export it actually reaches: the CSV never carries forecast rows,
                // and an unqualified label over both buttons read as if it did.
                label: "include forecast (JSON)"
                primary: root.withForecast
                enabled: root.horizon.length > 0
                onClicked: root.withForecast = !root.withForecast
            }
        }

        Row {
            width: parent.width
            spacing: 8
            PushButton {
                label: "Write JSON archive"
                enabled: Ledger.running
                onClicked: root.writeSide("bundle", undefined)
            }
            PushButton {
                label: "Write CSV"
                enabled: Ledger.running
                onClicked: root.writeSide("csv", undefined)
            }
            PushButton {
                label: "Write JSON to…"
                enabled: Ledger.running
                onClicked: root.openChooser("bundle")
            }
            PushButton {
                label: "Write CSV to…"
                enabled: Ledger.running
                onClicked: root.openChooser("csv")
            }
        }

        // The backup shows its target and these did not, which is how a folder change here could
        // go unseen. Only the folder: the names are the dated ones above.
        Text {
            width: parent.width
            elide: Text.ElideMiddle
            text: "into " + root.pretty(root.sideFolder) + "/"
            color: Theme.textFaint
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.sideResult.length > 0
            text: root.sideResult
            color: root.sideFailed ? Theme.red : Theme.okGreen
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
    }
}
