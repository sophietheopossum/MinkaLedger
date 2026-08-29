import QtQuick
import "../services"

// A button. `primary` marks the one action a form exists to perform.
Rectangle {
    id: root

    property string label
    property bool primary: false
    // `enabled` is inherited from Item: it already gates input and is what callers expect.

    signal clicked

    implicitWidth: caption.implicitWidth + 26
    implicitHeight: 30
    radius: 5
    opacity: root.enabled ? 1.0 : 0.4
    color: !root.enabled ? Theme.surface
         : root.primary ? (area.containsMouse ? Theme.purple : Theme.purpleDim)
                        : (area.containsMouse ? Theme.surfaceRaised : Theme.surface)
    border.width: 1
    border.color: root.primary ? Theme.purple : Theme.line

    Text {
        id: caption
        anchors.centerIn: parent
        text: root.label
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 1
        color: Theme.text
    }

    MouseArea {
        id: area
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: root.enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
        onClicked: if (root.enabled) root.clicked()
    }
}
