pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Emptying the book.
//
// THE POINT OF THIS SCREEN IS FRICTION, and the friction is informative rather than decorative.
// It is reached only from the account edit list, it states exactly what will go, and the confirm
// button stays dead until the word is typed in full. None of that is arming or double-clicking:
// a mis-click cannot type.
//
// It is also RECOVERABLE, which is worth more than any of the above. The core snapshots the book
// with VACUUM INTO before it clears anything and refuses to proceed if that fails, so the worst
// outcome is a file to restore rather than finances that are gone. The path is shown before and
// after, because a backup nobody can find is not a backup.
Rectangle {
    id: root

    signal done
    signal changed

    color: Theme.surface
    border.width: 1
    border.color: Theme.red
    radius: 8

    readonly property string token: "DELETE"
    property var counts: null
    property string result: ""
    property string backup: ""

    // Both triggers: onVisibleChanged alone misses a panel that starts visible, since the signal
    // fires on a CHANGE and there was none.
    Component.onCompleted: if (root.visible) root.load()
    onVisibleChanged: {
        if (root.visible) {
            confirmField.clear();
            root.result = "";
            root.backup = "";
            root.load();
        }
    }

    function load() {
        Ledger.request("analysis.query", {
            sql: "SELECT (SELECT COUNT(*) FROM account) accounts, (SELECT COUNT(*) FROM txn) txns,"
               + " (SELECT COUNT(*) FROM posting) postings, (SELECT COUNT(*) FROM series) series,"
               + " (SELECT COUNT(*) FROM scenario) scenarios,"
               + " (SELECT COUNT(*) FROM import_batch) batches"
        }, (r, e) => {
            if (!e && r.rows && r.rows.length > 0) {
                const row = r.rows[0];
                const out = {};
                for (let i = 0; i < r.columns.length; i++)
                    out[r.columns[i]] = row[i];
                root.counts = out;
            }
        });
    }

    // "1 transactions" on a screen whose only job is to be read carefully is exactly the kind of
    // sloppiness that makes a reader stop trusting the rest of it.
    function n(count, one, many) {
        return count + " " + (count === 1 ? one : many);
    }

    readonly property bool empty: root.counts !== null
                                  && root.counts.accounts === 0 && root.counts.txns === 0
    // Exact match, not trimmed and not case-folded: this is the one place where being strict
    // about what was typed is the entire feature.
    readonly property bool armed: confirmField.text === root.token

    function reset() {
        if (!root.armed)
            return;
        Ledger.write("book.reset", { confirm: root.token }, (r, e) => {
            if (e) {
                root.result = e.message;
                return;
            }
            root.backup = r.backup;
            const n = Object.keys(r.cleared || {}).length;
            root.result = "The book is empty. " + n + " table"
                        + (n === 1 ? " was" : "s were") + " cleared.";
            confirmField.clear();
            root.load();
            root.changed();
        });
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Text {
            text: "EMPTY THIS BOOK"
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            text: root.counts === null
                  ? "reading the book…"
                  : root.empty
                    ? "This book is already empty — there is nothing to remove."
                    : "This removes " + root.n(root.counts.accounts, "account", "accounts")
                      + ", " + root.n(root.counts.txns, "transaction", "transactions")
                      + " (" + root.n(root.counts.postings, "posting", "postings") + "), "
                      + root.n(root.counts.series, "recurring payment", "recurring payments")
                      + ", " + root.n(root.counts.scenarios, "scenario", "scenarios")
                      + " and " + root.n(root.counts.batches, "import batch", "import batches") + "."
            color: root.empty ? Theme.textFaint : Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 1
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: !root.empty
            text: "A complete copy is written next to the book first, and the wipe is abandoned if "
                + "that copy cannot be made. Nothing here is unrecoverable."
            color: Theme.okGreen
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }

        Row {
            spacing: 8
            visible: !root.empty
            Field {
                id: confirmField
                width: 200
                label: "type " + root.token + " to enable"
                placeholder: root.token
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "Empty the book"
                enabled: root.armed
                onClicked: root.reset()
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "Cancel"
                primary: true
                onClicked: root.done()
            }
        }

        PushButton {
            visible: root.empty
            label: "Close"
            primary: true
            onClicked: root.done()
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.result.length > 0
            text: root.result
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 1
        }
        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.backup.length > 0
            // Shown after the fact as well as before: this is the only thing standing between a
            // mistake and a rebuilt book, so it must be readable, not just promised.
            text: "The previous book is saved at " + root.backup
            color: Theme.okGreen
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 3
        }
    }
}
