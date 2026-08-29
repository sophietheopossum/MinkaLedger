import QtQuick
import "../services"

// A labelled single-line input. The caller owns the value: bind `text`, react to `edited`.
//
// Hand-rolled rather than QtQuick.Controls' TextField, matching MinkaConf: this repo's widgets are
// plain QtQuick so they theme cleanly against Proustite and carry no style dependency.
Rectangle {
    id: root

    property alias text: input.text
    property string label
    property string placeholder: ""
    property bool numeric: false
    /// Set by the caller to mark a value the core rejected.
    property bool invalid: false

    signal edited(string value)
    signal accepted

    implicitHeight: 46
    radius: 6
    color: Theme.surface
    border.width: 1
    border.color: root.invalid ? Theme.red : (input.activeFocus ? Theme.purple : Theme.line)

    Text {
        id: cap
        anchors.left: parent.left
        anchors.leftMargin: 8
        anchors.top: parent.top
        anchors.topMargin: 4
        text: root.label
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 4
        color: root.invalid ? Theme.red : Theme.textFaint
    }

    TextInput {
        id: input
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: 8
        anchors.rightMargin: 8
        anchors.top: cap.bottom
        anchors.topMargin: 1
        clip: true
        // Monospace for anything the eye needs to line up -- amounts and dates.
        font.family: root.numeric ? Theme.monoFamily : Theme.fontFamily
        font.pixelSize: Theme.fontSize
        color: Theme.text
        selectByMouse: true
        selectionColor: Theme.purpleDim
        selectedTextColor: Theme.text

        onTextEdited: {
            root.invalid = false; // typing clears the rejection
            root.edited(text);
        }
        onAccepted: root.accepted()

        Text {
            anchors.fill: parent
            visible: input.text.length === 0 && !input.activeFocus
            text: root.placeholder
            font: input.font
            color: Theme.textFaint
            elide: Text.ElideRight
        }
    }

    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.NoButton
        cursorShape: Qt.IBeamCursor
    }

    function focusInput() {
        input.forceActiveFocus();
    }
    function clear() {
        input.text = "";
        root.invalid = false;
    }
}
