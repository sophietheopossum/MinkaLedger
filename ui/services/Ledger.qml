pragma Singleton
import QtQuick
import Quickshell
import Quickshell.Io
// Through the config-root symlink: Quickshell only honours qmldir registration
// for paths inside the shell root.
import "../MinkaLink"

// The ledger core, spoken to over its stdio.
//
// Same protocol as ShojiWM's socket, so the correlation logic is MinkaLink's NdjsonRpc and only the
// transport differs — a child process instead of a Unix socket. That is the whole reason the RPC
// half was extracted: two transports, one implementation of the fiddly part.
//
// The core owns the database. This object owns nothing but the pipe, and every method here is a
// thin pass-through: any state the UI needs is asked for, never mirrored, because a cached balance
// that disagrees with the book is worse than a slow one.
Singleton {
    id: root

    // Resolved once: an explicit path, else the core's own default location.
    property string binary: Quickshell.env("MINKA_LEDGER_BIN") || "minka-ledger"
    property string book: Quickshell.env("MINKA_LEDGER_DB") || ""

    readonly property bool running: proc.running
    property string lastError: ""

    // Bumped whenever something is written, so views can re-query without polling.
    property int revision: 0

    function request(method, params, onResult) {
        if (!proc.running) {
            root.lastError = "ledger core is not running";
            if (onResult)
                onResult(null, { code: "no_core", message: root.lastError });
            return;
        }
        rpc.request(method, params, (result, error) => {
            if (error)
                root.lastError = error.message || String(error.code);
            if (onResult)
                onResult(result, error);
        });
    }

    // A write: same as request, but nudges `revision` so open views refresh.
    function write(method, params, onResult) {
        request(method, params, (result, error) => {
            if (!error)
                root.revision++;
            if (onResult)
                onResult(result, error);
        });
    }

    NdjsonRpc {
        id: rpc
        writeLine: line => {
            if (proc.running)
                proc.write(line);
        }
    }

    Process {
        id: proc
        // `--db ""` would create a book called "" -- omit the flag entirely and let the core pick.
        command: root.book.length > 0 ? [root.binary, "--db", root.book] : [root.binary]
        running: true

        stdout: SplitParser {
            onRead: line => rpc.feedLine(line)
        }
        // The core writes diagnostics to stderr; surfacing them is how a missing binary or an
        // unreadable book becomes visible rather than a silent dead UI.
        stderr: SplitParser {
            onRead: line => {
                if (line.indexOf("cannot open") >= 0)
                    root.lastError = line;
            }
        }
        onRunningChanged: {
            rpc.reset();
            if (!running)
                root.lastError = "ledger core exited";
        }
    }
}
