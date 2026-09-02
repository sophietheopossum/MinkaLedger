pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Window
import "../services"

// A description field that SUGGESTS but does not RESTRICT.
//
// Most descriptions are new, so this is a Field first and a dropdown second: whatever is typed is
// taken verbatim, and the list is only a shortcut for the labels that repeat fifty times a month.
// Nothing here may get between her and the keyboard -- so there is no chevron, no "choose…", and no
// list at all until typing produces a match.
//
// ENTER SUBMITS WHAT IS TYPED, and the list is deliberately MOUSE-ONLY to keep that true. Give it
// arrow-key navigation and Enter has to choose between the typed text and a highlighted row, and
// the wrong choice puts a description she never wrote onto a real payment. AccountPicker can read
// Enter as "the highlighted account" because there is nothing else Enter could mean inside its
// popup; here the typed text is always the other candidate, so the ambiguity is never created.
//
// The typing surface is a Field rather than a copy of one: identical look and identical behaviour
// are the requirement, and a second TextInput styled by hand is a second thing to drift.
Item {
    id: root

    // Drop-in for the Field it replaces: same text/label/placeholder, same edited/accepted, same
    // clear().
    property alias text: field.text
    property string label
    property string placeholder: ""

    signal edited(string value)
    signal accepted

    implicitHeight: field.implicitHeight

    // The core's answer for `_lastQuery`: [{ description, count, last_on }], already ordered
    // most-used first. The order is NOT re-sorted here -- ranking is the core's job and it has the
    // counts to do it with. `last_on` is on the wire and deliberately unused: it is what breaks the
    // core's ties, not something a one-line row has room for.
    property var suggestions: []
    // Escape hides the list without touching a character of the typed text. Typing brings it back.
    property bool dismissed: false
    // Which query `suggestions` belongs to, so a prefix that comes back round -- typed, deleted,
    // retyped -- is answered from what is already here instead of asked for again.
    property string lastQuery: ""
    // Only the newest request may write `suggestions`. Without this a slow answer for "sho" can
    // land after the fast one for "shop" and repopulate the list with matches for text she has
    // already moved past.
    property int seq: 0

    // True while the caret is in THIS field. Walked from the window's focus item rather than read
    // off `field`, because activeFocus belongs to the TextInput inside Field and a plain Item
    // parent does not report its child's.
    //
    // It is what closes the list when she moves on: clicking a Field or an AccountPicker takes
    // focus away (AccountPicker focuses its own search box on open), and the suggestions go with
    // it rather than hanging over the form.
    readonly property bool inputFocused: {
        let item = root.Window.activeFocusItem;
        while (item) {
            if (item === field)
                return true;
            item = item.parent;
        }
        return false;
    }

    // No matches means no popup at all -- the common case is a description that has never been
    // used, and that case must look exactly like a plain text field.
    readonly property bool listOpen: root.visible && root.inputFocused && !root.dismissed
                                     && root.suggestions.length > 0
                                     && root.Window.contentItem !== null

    function clear() {
        field.clear();
        root.suggestions = [];
        root.lastQuery = "";
        root.dismissed = false;
        debounce.stop();
    }

    // One request per pause in typing, not one per keystroke: at her typing speed a description is
    // a dozen keystrokes, and a dozen round trips to fill one list is a dozen chances for the list
    // to be answering a prefix she has already left.
    Timer {
        id: debounce
        interval: 180
        onTriggered: root.ask()
    }

    function ask() {
        const query = field.text.trim();
        // Whitespace-only is not a search: the core treats a blank prefix as "no filter" and would
        // answer with the fifty most-used descriptions, which is not what pressing space asked for.
        if (query.length === 0) {
            root.suggestions = [];
            root.lastQuery = "";
            return;
        }
        if (query === root.lastQuery)
            return;
        const mine = ++root.seq;
        Ledger.request("txn.descriptions", { prefix: query, limit: 50 }, (r, e) => {
            if (mine !== root.seq)
                return; // a later keystroke has already overtaken this answer
            // An error leaves `lastQuery` alone so the next keystroke re-asks rather than caching
            // the failure as "no matches".
            if (e)
                root.suggestions = [];
            else {
                root.lastQuery = query;
                root.suggestions = r || [];
            }
        });
    }

    function choose(description: string) {
        field.text = description;
        // The list held matches for a PREFIX of this, so it no longer describes what is in the
        // field. Dropping it stops those stale rows flashing under the next keystroke.
        root.suggestions = [];
        root.lastQuery = "";
        root.dismissed = true;
        root.edited(description);
    }

    // Reparent to the WINDOW SURFACE and position in that surface's coordinates -- the same reason
    // AccountPicker does it, and it applies here too: this field sits in EntryForm's Column, and a
    // Column is a POSITIONER. A popup left as an ordinary child becomes another element to lay out
    // and lands under the form rather than over it. See AccountPicker's comment in full.
    function placeList() {
        const surface = root.Window.contentItem;
        if (!surface)
            return;
        popup.parent = surface;
        const below = root.mapToItem(surface, 0, root.height + 2);
        popup.x = Math.max(0, Math.min(below.x, surface.width - popup.width));
        const above = root.mapToItem(surface, 0, 0).y - popup.height - 2;
        popup.y = below.y + popup.height <= surface.height ? below.y : Math.max(0, above);
    }

    // Escape closes the list and CHANGES NOTHING ELSE. Unhandled when there is no list, so it still
    // means whatever it means to the rest of the window.
    Keys.onEscapePressed: event => {
        if (root.listOpen) {
            root.dismissed = true;
            event.accepted = true;
        } else {
            event.accepted = false;
        }
    }

    Field {
        id: field
        anchors.fill: parent
        label: root.label
        placeholder: root.placeholder

        onEdited: value => {
            root.dismissed = false; // typing after an Escape is asking for the list again
            debounce.restart();
            root.edited(value);
        }
        // Enter takes the text as typed. It closes the list; it never substitutes a row for it.
        onAccepted: {
            root.dismissed = true;
            root.accepted();
        }
    }

    // Position and parent are set by placeList(); only the size is bound here.
    Rectangle {
        id: popup
        visible: root.listOpen
        width: root.width
        height: Math.min(list.contentHeight + 8, 176)
        z: 100
        color: Theme.surfaceRaised
        border.width: 1
        border.color: Theme.purple
        radius: 6

        readonly property int rowHeight: 24

        onVisibleChanged: if (popup.visible) root.placeList()
        // The list grows and shrinks as matches change, and a popup flipped above the field would
        // otherwise keep the y it was given at its old height and drift away from the field.
        onHeightChanged: if (popup.visible) root.placeList()

        ListView {
            id: list
            anchors.fill: parent
            anchors.margins: 4
            clip: true
            model: root.suggestions
            delegate: Rectangle {
                id: row
                // Required under `pragma ComponentBehavior: Bound`: the pragma is what lets the
                // delegate reach `root`, and its price is that modelData must be declared rather
                // than injected. Without this the delegate renders blank at runtime while qmllint
                // stays silent.
                required property var modelData

                width: ListView.view.width
                height: popup.rowHeight
                color: hover.containsMouse ? Theme.surface : "transparent"
                radius: 3
                Text {
                    anchors.left: parent.left
                    anchors.leftMargin: 6
                    anchors.right: uses.left
                    anchors.rightMargin: 4
                    anchors.verticalCenter: parent.verticalCenter
                    elide: Text.ElideRight
                    text: row.modelData.description
                    color: Theme.text
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 1
                }
                // The count is what makes "common" visible -- a list ordered by frequency looks
                // arbitrary without it. It keeps its width and the description elides into what is
                // left, because how often a label was used is one glyph or two and the label is
                // the part worth reading.
                Text {
                    id: uses
                    anchors.right: parent.right
                    anchors.rightMargin: 6
                    anchors.verticalCenter: parent.verticalCenter
                    text: "×" + row.modelData.count
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 3
                }
                MouseArea {
                    id: hover
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.choose(row.modelData.description)
                }
            }
        }
    }
}
