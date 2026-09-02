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

    // Requests made before the child process has finished starting. The window asks for its data
    // in Component.onCompleted, which fires BEFORE Process.running flips true -- dropping those
    // would leave a permanently empty window that had merely asked too early.
    property var _pendingStart: []

    function request(method, params, onResult) {
        if (!proc.running) {
            _pendingStart.push({ method, params, onResult });
            return;
        }
        rpc.request(method, params, (result, error) => {
            if (error)
                root.lastError = error.message || String(error.code);
            else
                root.lastError = ""; // a good response means whatever went wrong is over
            if (onResult)
                onResult(result, error);
        });
    }

    function _flushPendingStart() {
        const queued = _pendingStart;
        _pendingStart = [];
        for (const q of queued)
            root.request(q.method, q.params, q.onResult);
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
            if (running) {
                root.lastError = "";
                root._flushPendingStart();
            } else {
                // ONLY a DROPPED transport invalidates outstanding callbacks, so the reset belongs
                // here and not on the way up as well.
                //
                // Resetting on the way up threw away requests that had been sent perfectly well:
                // `proc.running` already reads true inside request() a moment BEFORE this signal is
                // delivered, so the window's Component.onCompleted burst goes straight down the
                // pipe, and the reset a moment later dropped the callbacks waiting for it. The core
                // answered and nothing was listening. `account.balances` is in that burst, so the
                // cost was every AccountPicker in the window coming up with an empty list on a cold
                // start. Nothing needs clearing on the way up in any case: a previous process's
                // callbacks were already dropped when IT exited, and ids are never reused.
                rpc.reset();
                root.lastError = "ledger core exited";
            }
        }
    }
}
