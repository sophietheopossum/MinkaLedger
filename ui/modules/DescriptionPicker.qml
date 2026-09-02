pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Window
import "../services"

// A description field that SUGGESTS but does not RESTRICT.
//
// Most descriptions are new, so this is a Field first and a dropdown second: whatever is typed is
// taken verbatim, and the list is only a shortcut for the labels that repeat fifty times a month.
//
// THE LIST NEVER APPEARS UNINVITED. It used to drop open on its own as soon as typing produced a
// match, and that put a full-width strip of clickable rows exactly where the next click of the
// workflow was going: the form's order is description, then from, then to, and with two matches the
// list covered BOTH account pickers -- with four it reached the Record button. A click aimed at the
// "from" picker was swallowed by a suggestion row, the picker never opened, and the description
// silently became a label she never chose, which then went into the book. The list was the only
// thing between her and the form, which is the opposite of what it is for.
//
// So it opens only when asked: DOWN from the field, or a click on the match count in the corner of
// the field. When it does open it goes ABOVE the field wherever there is room, because the row
// above is the one she has already filled in.
//
// ENTER STILL SUBMITS WHAT IS TYPED. `highlight` starts at -1 and only the arrow keys move it off
// -1, so Enter means "take my text" in every state except the one she deliberately arrowed into --
// the mouse never moves it, and hovering a row only tints it. That is the same rule AccountPicker's
// `keyNav` flag keeps, and it is what lets this list have a keyboard route at all without Enter
// ever having to guess between the typed text and a row.
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
    // Set by openList() and cleared by Escape, by picking, and by the caret leaving the field.
    // Nothing else opens the list -- see the header.
    property bool opened: false
    // Which row Enter would take, -1 for NONE. Only the arrow keys move it, and typing and Escape
    // put it back to -1.
    property int highlight: -1
    // Which query `suggestions` belongs to, so a prefix that comes back round -- typed, deleted,
    // retyped -- is answered from what is already here instead of asked for again.
    property string lastQuery: ""
    // Only the newest request may write `suggestions`. Without this a slow answer for "sho" can
    // land after the fast one for "shop" and repopulate the list with matches for text she has
    // already moved past. ABANDONING a query counts as overtaking it, so clear() and the blank
    // branch of ask() bump this too -- otherwise an answer for text she has deleted still arrives
    // and offers itself over an empty field.
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

    readonly property bool listOpen: root.visible && root.inputFocused && root.opened
                                     && root.suggestions.length > 0
                                     && root.Window.contentItem !== null

    // The whole of the list's presence when it is shut: a count and an arrow, inside the field.
    // Without it a list that only opens on request is a feature with nothing to say it is there.
    readonly property bool badgeVisible: root.visible && root.inputFocused
                                         && root.suggestions.length > 0
                                         && root.Window.contentItem !== null

    // Moving on with the caret takes the list with it, and leaves it shut for the next visit --
    // reopening under her the moment she comes back is the uninvited list again.
    onInputFocusedChanged: if (!root.inputFocused) root.closeList()

    function clear() {
        field.clear();
        ++root.seq;
        root.suggestions = [];
        root.lastQuery = "";
        root.closeList();
        debounce.stop();
    }

    function openList() {
        if (root.suggestions.length === 0)
            return;
        root.opened = true;
    }

    function closeList() {
        root.opened = false;
        root.highlight = -1;
    }

    // -1 is a real position, one step above the first row: arrowing back up to it hands Enter to
    // the typed text again without shutting the list.
    function moveHighlight(delta: int) {
        const n = root.suggestions.length;
        if (n === 0)
            return;
        const next = root.highlight + delta;
        root.highlight = next < 0 ? -1 : Math.min(next, n - 1);
        if (root.highlight >= 0)
            list.positionViewAtIndex(root.highlight, ListView.Contain);
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
            ++root.seq; // an answer still in flight is for text that no longer exists
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
                // Show them without being asked. The review that moved this list ABOVE the field
                // offered gating it behind the badge as an ALTERNATIVE to the move, not as well as
                // it -- taking both left the suggestions invisible until you found a small chevron,
                // which defeats the point ("that way choosing common descriptions is easier").
                // Opening upward is what makes auto-open safe: the row above is the already-filled
                // date and amount, where a stray click moves a caret, while below are the account
                // pickers a click was being stolen from.
                if (root.inputFocused)
                    root.openList();
            }
        });
    }

    function choose(description: string) {
        field.text = description;
        // The list held matches for a PREFIX of this, so it no longer describes what is in the
        // field. Dropping it stops those stale rows flashing under the next keystroke.
        ++root.seq;
        root.suggestions = [];
        root.lastQuery = "";
        root.closeList();
        // Taking a label is not recording a payment: `accepted` stays with Enter on the text.
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
        dismiss.parent = surface;
        dismiss.x = 0;
        dismiss.y = 0;
        dismiss.width = surface.width;
        dismiss.height = surface.height;
        const here = root.mapToItem(surface, 0, 0);
        popup.x = Math.max(0, Math.min(here.x, surface.width - popup.width));
        // ABOVE by preference, which is the opposite way round from the usual dropdown and is the
        // point: below this field are the two account pickers and the Record button, and anything
        // covering them turns her next click into a suggestion she did not pick. Above is the
        // date/amount row, already filled in, where the worst a swallowed click costs is a caret.
        // Below is the fallback for a field with no room above it, and the clamp keeps a list
        // taller than the window on screen either way.
        const above = here.y - popup.height - 2;
        const below = here.y + root.height + 2;
        // BELOW by preference, as Sophie asked. A review measured the downward list covering both
        // account pickers, so a click meant for "from" was taken by a suggestion row and stored a
        // description that was never typed; the dismiss layer below closes the list on any click
        // outside it, which is the behaviour she wanted. It does NOT cover a click that lands ON a
        // row while aiming at a picker underneath -- that risk is accepted, not solved.
        popup.y = below + popup.height <= surface.height ? below
                                                         : Math.max(0, above);
    }

    // Escape closes the list and CHANGES NOTHING ELSE. Unhandled when there is no list, so it still
    // means whatever it means to the rest of the window.
    Keys.onEscapePressed: event => {
        if (root.listOpen) {
            root.closeList();
            event.accepted = true;
        } else {
            event.accepted = false;
        }
    }

    // Down is the keyboard's way in: the first press opens the list AND takes the top row, because
    // a press that only opened it would need a second one to do anything. Up walks back out to -1.
    Keys.onDownPressed: event => {
        if (root.suggestions.length === 0 || !root.inputFocused) {
            event.accepted = false;
            return;
        }
        if (!root.opened) {
            root.openList();
            root.highlight = 0;
            list.positionViewAtBeginning();
        } else {
            root.moveHighlight(1);
        }
        event.accepted = true;
    }
    Keys.onUpPressed: event => {
        if (!root.listOpen) {
            event.accepted = false;
            return;
        }
        root.moveHighlight(-1);
        event.accepted = true;
    }

    Field {
        id: field
        anchors.fill: parent
        label: root.label
        placeholder: root.placeholder

        onEdited: value => {
            // Typing is not arrowing: whatever was highlighted described a different prefix, and
            // leaving it set would hand Enter a row she chose for text she has since changed.
            root.highlight = -1;
            debounce.restart();
            root.edited(value);
        }
        // Enter takes the text as typed unless she has arrowed onto a row, which is the only state
        // where anything else is on offer.
        onAccepted: {
            if (root.listOpen && root.highlight >= 0 && root.highlight < root.suggestions.length) {
                root.choose(root.suggestions[root.highlight].description);
                return;
            }
            root.closeList();
            root.accepted();
        }
    }

    // The affordance, and the only thing on screen when the list is shut: "×3 ▾" means three
    // descriptions in the book match what is typed. Opaque, so it sits over the text rather than
    // in it, and narrow, so it covers as little of a long description as a count can.
    Rectangle {
        id: badge
        visible: root.badgeVisible
        anchors.right: parent.right
        anchors.rightMargin: 6
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 5
        width: badgeText.implicitWidth + 10
        height: badgeText.implicitHeight + 4
        radius: 3
        color: Theme.surfaceRaised
        border.width: 1
        border.color: root.listOpen ? Theme.purple : Theme.line

        Text {
            id: badgeText
            anchors.centerIn: parent
            text: "×" + root.suggestions.length + (root.listOpen ? " ▴" : " ▾")
            color: root.listOpen ? Theme.purple : Theme.textFaint
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 3
        }

        MouseArea {
            anchors.fill: parent
            cursorShape: Qt.PointingHandCursor
            // A second click puts it away again, so the mouse has the same way out as Escape.
            onClicked: {
                if (root.listOpen)
                    root.closeList();
                else
                    root.openList();
            }
        }
    }

    // Click anywhere that is not the list, and the list goes away. Parented to the window surface
    // alongside the popup and sized to it, so it catches presses over the whole window; z sits one
    // below the popup so a press on a suggestion still reaches the row. `dismiss` swallows the
    // press rather than passing it through: the click that closes a dropdown should not also
    // actuate whatever was underneath it.
    MouseArea {
        id: dismiss
        visible: root.listOpen
        z: 99
        acceptedButtons: Qt.AllButtons
        onPressed: root.closeList()
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
                required property int index

                width: ListView.view.width
                height: popup.rowHeight
                // Hover tints a row but does NOT move `highlight`: the tint says what the mouse
                // would click, and Enter must keep meaning the typed text no matter where the
                // pointer is resting.
                color: (row.index === root.highlight || hover.containsMouse) ? Theme.surface
                                                                            : "transparent"
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
