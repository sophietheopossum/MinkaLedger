pragma Singleton
import QtQuick
import Quickshell
// Through the config-root symlink: Quickshell only honours qmldir
// singleton registration for paths inside the shell root.
import "../Proustite"

// Thin facade over the shared Proustite palette (replaces the tokens that
// were mirrored here from MinkaShell while waiting for a shared theme).
Singleton {
    readonly property color ground: Proustite.ground
    readonly property color surface: Proustite.surface
    readonly property color surfaceRaised: Proustite.surfaceRaised
    readonly property color line: Proustite.line
    readonly property color text: Proustite.text
    readonly property color textMuted: Proustite.textMuted
    readonly property color textFaint: Proustite.textFaint
    readonly property color red: Proustite.red
    readonly property color redDim: Proustite.redDim
    readonly property color purple: Proustite.purple

    readonly property string fontFamily: Proustite.fontFamily
    readonly property string monoFamily: Proustite.monoFamily
    readonly property int fontSize: Proustite.fontSize
}