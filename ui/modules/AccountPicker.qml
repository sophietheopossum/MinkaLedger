pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Window
import "../services"

// Pick one account. A dropdown, hand-rolled in the MinkaConf idiom.
//
// Shows the account KIND alongside the name, because "Rent" as an expense and "Rent" as a standing
// order to a landlord's account are different things and the list is the only place to tell them
// apart.
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
    function openList() {
        const surface = root.Window.contentItem;
        if (!surface)
            return;
        popup.parent = surface;
        const below = root.mapToItem(surface, 0, root.height + 2);
        popup.x = below.x;
        // Flip above the field when there is no room beneath -- the import panel's picker sits
        // near the bottom of the window, where dropping down would run off the edge.
        popup.y = below.y + popup.height <= surface.height
                  ? below.y
                  : root.mapToItem(surface, 0, 0).y - popup.height - 2;
        popup.visible = true;
    }

    // A popup reparented to the window outlives the form that owns it, so it has to be dismissed
    // when that form goes away -- otherwise it hangs over whatever replaces it.
    onVisibleChanged: if (!root.visible) popup.visible = false

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
                popup.visible = false;
            else
                root.openList();
        }
    }

    // Position and parent are set by openList(); only the size is bound here.
    Rectangle {
        id: popup
        visible: false
        width: root.width
        height: Math.min(list.contentHeight + 8, 220)
        z: 100
        color: Theme.surfaceRaised
        border.width: 1
        border.color: Theme.purple
        radius: 6

        ListView {
            id: list
            anchors.fill: parent
            anchors.margins: 4
            clip: true
            model: root.accounts
            delegate: Rectangle {
                id: row
                // Required under `pragma ComponentBehavior: Bound`: the pragma is what lets the
                // delegate reach `root`, and its price is that modelData must be declared rather
                // than injected. Without this the delegate renders blank at runtime while qmllint
                // stays silent.
                required property var modelData

                width: ListView.view.width
                height: 24
                color: hover.containsMouse ? Theme.surface : "transparent"
                radius: 3
                Text {
                    anchors.left: parent.left
                    anchors.leftMargin: 6
                    anchors.right: kind.left
                    anchors.verticalCenter: parent.verticalCenter
                    elide: Text.ElideRight
                    text: row.modelData.name
                    color: Theme.text
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
                    onClicked: {
                        root.selected = row.modelData.account_id;
                        root.picked(row.modelData.account_id);
                        popup.visible = false;
                    }
                }
            }
        }
    }
}
