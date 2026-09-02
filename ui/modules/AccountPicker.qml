pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Window
import "../services"

// Pick one account. A dropdown, hand-rolled in the MinkaConf idiom.
//
// Shows the account KIND alongside the name, because "Rent" as an expense and "Rent" as a standing
// order to a landlord's account are different things and the list is the only place to tell them
// apart.
//
// The list is searchable, and the search matches name AND kind. Matching kind is not decoration:
// names collide -- that collision is the whole reason kind is on screen -- so "expense" has to be
// a way of narrowing to the one of them she meant.
Rectangle {
    id: root

    property var accounts: []
    property string label
    property int selected: -1

    signal picked(int accountId)

    // Opening the list reparents it to the WINDOW SURFACE and positions it in that surface's
    // coordinates.
    //
    // It used to do `parent: root.parent`, which was wrong in a way that only showed up in use:
    // every picker sits inside a Row, and a Row is a POSITIONER -- it lays out each visible child
    // left to right and ignores their x. The popup became a third element in that Row and was
    // pushed off the right-hand edge of the form. Leaving it as a plain child of this item would
    // fix the position but put its geometry outside this item's bounds, which is not somewhere
    // input can be relied on to reach.
    //
    // The search field lives INSIDE that reparented popup for the same reason: anywhere else and
    // it is a stray child of the Row again.
    function openList() {
        const surface = root.Window.contentItem;
        if (!surface)
            return;
        popup.parent = surface;
        root.placePopup();
        // Taking focus is new, so giving it back has to be too: she opens this mid-form, and
        // before the search field existed the amount she was typing in kept the cursor. Without
        // this, picking an account leaves the keyboard pointing at nothing.
        //
        // What must NOT be captured is another picker's search box. Opening the second picker of
        // the from/to pair while the first is still up used to save that first search field as the
        // place to hand the keyboard back to; it is gone by the time the hand-back happens (this
        // popup taking focus is what closes it), so the caret went into a field that was no longer
        // on screen and nothing she typed reached anything. What she was typing in before the
        // FIRST picker opened is the right answer, and the first picker is still holding it.
        //
        // The two names are looked up through variables because the walk is over plain Items, and
        // only a picker's popup carries either one -- a name that is not there reads as undefined,
        // which is the test. Spelled as literals the linter resolves them against QQuickItem and
        // reports the very absence this relies on.
        const mark = "accountPickerPopup";
        const saved = "restoreFocus";
        let back = root.Window.activeFocusItem;
        for (let it = back; it; it = it.parent) {
            if (it[mark] === true) {
                back = it[saved];
                break;
            }
        }
        popup.restoreFocus = back;
        // Visible before focus: an invisible item cannot take active focus, so the order here is
        // what makes typing work the instant the list opens.
        popup.visible = true;
        search.forceActiveFocus();
    }

    // Split out of openList() because filtering changes the popup's height on every keystroke, and
    // a popup that was flipped above the field would otherwise keep the y it was given when it was
    // full-length and drift away from the field as the list shrinks.
    function placePopup() {
        const surface = root.Window.contentItem;
        if (!surface || popup.parent !== surface)
            return;
        const below = root.mapToItem(surface, 0, root.height + 2);
        // Clamped both ways: the import panel's picker sits near the bottom edge, and a narrow
        // picker near the right edge can push a popup past the surface too.
        popup.x = Math.max(0, Math.min(below.x, surface.width - popup.width));
        const above = root.mapToItem(surface, 0, 0).y - popup.height - 2;
        popup.y = below.y + popup.height <= surface.height ? below.y : Math.max(0, above);
    }

    // A popup reparented to the window outlives the form that owns it, so it has to be dismissed
    // when that form goes away -- otherwise it hangs over whatever replaces it.
    onVisibleChanged: if (!root.visible) popup.close(false)

    implicitHeight: 46
    radius: 6
    color: Theme.surface
    border.width: 1
    border.color: popup.visible ? Theme.purple : Theme.line

    readonly property string selectedName: {
        for (const a of (accounts || []))
            if (a.account_id === selected)
                return a.name;
        return "";
    }

    Text {
        anchors.left: parent.left
        anchors.leftMargin: 8
        anchors.top: parent.top
        anchors.topMargin: 4
        text: root.label
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 4
        color: Theme.textFaint
    }

    Text {
        anchors.left: parent.left
        anchors.right: chevron.left
        anchors.leftMargin: 8
        anchors.rightMargin: 4
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 5
        elide: Text.ElideRight
        text: root.selectedName.length > 0 ? root.selectedName : "choose…"
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize
        color: root.selectedName.length > 0 ? Theme.text : Theme.textFaint
    }

    Text {
        id: chevron
        anchors.right: parent.right
        anchors.rightMargin: 8
        anchors.verticalCenter: parent.verticalCenter
        text: popup.visible ? "▴" : "▾"
        color: Theme.textFaint
        font.pixelSize: Theme.fontSize
    }

    MouseArea {
        anchors.fill: parent
        cursorShape: Qt.PointingHandCursor
        onClicked: {
            if (popup.visible)
                popup.close(true);
            else
                root.openList();
        }
    }

    // Position and parent are set by openList(); only the size is bound here.
    Rectangle {
        id: popup
        visible: false
        width: root.width
        // Height is computed from the match COUNT rather than read off the ListView's contentHeight,
        // because with no matches the content is empty and the "no match" line still needs a row's
        // worth of space to be seen in.
        height: 40 + popup.listHeight
        z: 100
        color: Theme.surfaceRaised
        border.width: 1
        border.color: Theme.purple
        radius: 6

        // Read by a sibling picker walking up from the focused item, to tell "the keyboard is in
        // another picker's search box" from "the keyboard is in the form".
        readonly property bool accountPickerPopup: true

        readonly property int rowHeight: 24
        readonly property int listHeight: Math.min(Math.max(popup.matches.length, 1) * popup.rowHeight, 168)

        readonly property string query: search.text
        // Which row Enter would take. Moved by the arrows, and by the mouse when the mouse is the
        // thing that is moving.
        property int highlight: 0
        // Set while the arrows are driving, so a list scrolling under a stationary pointer cannot
        // yank the highlight back to whatever row slid beneath the cursor.
        property bool keyNav: false
        // Whatever held the cursor when the list opened, handed back when it closes.
        property Item restoreFocus: null

        // RANKED, not filtered in the accounts' own order, because Enter takes the top row: typing
        // an account's whole name has to land on THAT account. Left unranked, "rent" put "Current
        // account" first -- it contains the letters and it happens to be account 1 -- so the
        // fastest path the search invites, type the name and press Enter, picked the wrong side of
        // a payment. Exact name, then names that start with it, then names that contain it, then
        // the kind; source order within each tier, so nothing reshuffles between keystrokes.
        readonly property var matches: {
            const all = root.accounts || [];
            const q = popup.query.trim().toLowerCase();
            if (q.length === 0)
                return all;
            const exact = [], starts = [], contains = [], kinds = [];
            for (const a of all) {
                const name = (a.name || "").toLowerCase();
                const kind = (a.kind || "").toLowerCase();
                if (name === q)
                    exact.push(a);
                else if (name.startsWith(q))
                    starts.push(a);
                else if (name.indexOf(q) >= 0)
                    contains.push(a);
                else if (kind.indexOf(q) >= 0)
                    kinds.push(a);
            }
            return exact.concat(starts, contains, kinds);
        }

        // The one place the search is reset, so the next open starts unfiltered however it closed.
        onVisibleChanged: if (!popup.visible) {
            search.text = "";
            popup.highlight = 0;
            popup.keyNav = false;
        }

        // Every close runs through here -- picking, Escape, clicking the field again, the keyboard
        // moving out, and the owning form going away.
        //
        // `giveBack` is false whenever focus has already gone somewhere she chose, because handing
        // it back then is not restoring the caret, it is stealing it off whatever she just clicked.
        function close(giveBack: bool) {
            if (!popup.visible)
                return;
            // Read and cleared BEFORE hiding: hiding drops the search's focus, which re-enters here
            // through onActiveFocusChanged, and the second pass must not undo the first's.
            const back = popup.restoreFocus;
            popup.restoreFocus = null;
            popup.visible = false;
            // `visible` on an Item is EFFECTIVE visibility, so a field inside a popup that has
            // since closed reports false and the hand-back is skipped rather than stranding the
            // keyboard in something that is not on screen.
            if (giveBack && back && back.visible && back.enabled)
                back.forceActiveFocus();
        }

        onQueryChanged: {
            popup.highlight = 0;
            list.positionViewAtBeginning();
        }

        onHeightChanged: if (popup.visible) root.placePopup()

        function choose(i: int) {
            const opts = popup.matches;
            if (i < 0 || i >= opts.length)
                return;
            root.selected = opts[i].account_id;
            root.picked(opts[i].account_id);
            popup.close(true);
        }

        function moveHighlight(delta: int) {
            const n = popup.matches.length;
            if (n === 0)
                return;
            popup.keyNav = true;
            popup.highlight = Math.max(0, Math.min(popup.highlight + delta, n - 1));
            list.positionViewAtIndex(popup.highlight, ListView.Contain);
        }

        Rectangle {
            id: searchBox
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.margins: 4
            height: 28
            radius: 4
            color: Theme.surface
            border.width: 1
            border.color: search.activeFocus ? Theme.purple : Theme.line

            TextInput {
                id: search
                anchors.fill: parent
                anchors.leftMargin: 6
                anchors.rightMargin: 6
                verticalAlignment: TextInput.AlignVCenter
                clip: true
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 1
                color: Theme.text
                selectByMouse: true
                selectionColor: Theme.purpleDim
                selectedTextColor: Theme.text

                // Enter takes the highlighted row, which with no arrow keys pressed is the top
                // match. Escape leaves without picking -- it must not fall through to the row
                // under the highlight.
                Keys.onReturnPressed: event => {
                    popup.choose(popup.highlight);
                    event.accepted = true;
                }
                Keys.onEnterPressed: event => {
                    popup.choose(popup.highlight);
                    event.accepted = true;
                }
                Keys.onEscapePressed: event => {
                    popup.close(true);
                    event.accepted = true;
                }
                Keys.onDownPressed: event => {
                    popup.moveHighlight(1);
                    event.accepted = true;
                }
                Keys.onUpPressed: event => {
                    popup.moveHighlight(-1);
                    event.accepted = true;
                }

                // Losing the keyboard closes the list. That is click-outside-to-close wherever the
                // click lands on something focusable, and it is what stops two of these being open
                // at once: opening the second takes the focus off the first. No hand-back -- focus
                // is already where she put it.
                onActiveFocusChanged: if (!search.activeFocus) popup.close(false)

                Text {
                    anchors.fill: parent
                    verticalAlignment: Text.AlignVCenter
                    visible: search.text.length === 0
                    text: "search name or kind…"
                    font: search.font
                    color: Theme.textFaint
                    elide: Text.ElideRight
                }
            }

            MouseArea {
                anchors.fill: parent
                acceptedButtons: Qt.NoButton
                cursorShape: Qt.IBeamCursor
            }
        }

        Text {
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: searchBox.bottom
            anchors.leftMargin: 10
            anchors.rightMargin: 10
            anchors.topMargin: 8
            visible: popup.matches.length === 0
            text: "no match"
            elide: Text.ElideRight
            color: Theme.textFaint
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 1
        }

        ListView {
            id: list
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            anchors.top: searchBox.bottom
            anchors.margins: 4
            clip: true
            model: popup.matches
            delegate: Rectangle {
                id: row
                // Required under `pragma ComponentBehavior: Bound`: the pragma is what lets the
                // delegate reach `root`, and its price is that modelData must be declared rather
                // than injected. Without this the delegate renders blank at runtime while qmllint
                // stays silent.
                required property var modelData
                required property int index

                readonly property bool current: row.modelData.account_id === root.selected

                width: ListView.view.width
                height: popup.rowHeight
                color: popup.highlight === row.index ? Theme.surface : "transparent"
                radius: 3
                Text {
                    anchors.left: parent.left
                    anchors.leftMargin: 6
                    anchors.right: kind.left
                    anchors.verticalCenter: parent.verticalCenter
                    elide: Text.ElideRight
                    text: row.modelData.name
                    // The account already chosen stays legible in the list after a reopen.
                    color: row.current ? Theme.purple : Theme.text
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 1
                }
                Text {
                    id: kind
                    anchors.right: parent.right
                    anchors.rightMargin: 6
                    anchors.verticalCenter: parent.verticalCenter
                    text: row.modelData.kind
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 3
                }
                MouseArea {
                    id: hover
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    // Hover only takes the highlight back off the keyboard once the pointer has
                    // actually moved, so arrowing through a list under the cursor is not fought.
                    onPositionChanged: {
                        popup.keyNav = false;
                        popup.highlight = row.index;
                    }
                    onEntered: if (!popup.keyNav) popup.highlight = row.index
                    onClicked: popup.choose(row.index)
                }
            }
        }
    }
}
