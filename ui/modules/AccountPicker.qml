pragma ComponentBehavior: Bound
import QtQuick
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
        onClicked: popup.visible = !popup.visible
    }

    // Drawn as a sibling overlay so it is not clipped by the row it sits in.
    Rectangle {
        id: popup
        visible: false
        parent: root.parent
        x: root.x
        y: root.y + root.height + 2
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
