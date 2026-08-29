pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Currencies.
//
// minor_digits IS THE WHOLE FEATURE. It is the divisor for every amount ever recorded in that
// currency, so getting it wrong is a silent 100x error rather than a visible failure — and it is
// not guessable: most currencies are 2, JPY and KRW are 0, the Gulf dinars are 3. Asking someone
// to type it is asking them to be wrong eventually, so typing a known code fills it in from the
// ISO 4217 table below and says so. The manual path stays for anything not listed, but it is the
// exception rather than the default.
//
// 18-decimal currencies are deliberately absent: amounts are i64 minor units, which overflows at
// 9.22 units at that scale, so the core caps minor_digits at 8 and ETH cannot be represented.
Rectangle {
    id: root

    signal done
    signal changed

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    property var currencies: []
    property string note: ""
    property int manualDigits: 2
    property bool manual: false

    // ISO 4217 minor units. Only entries I can state with confidence — anything else takes the
    // manual path rather than being guessed at.
    readonly property var known: ({
        "USD": ["US Dollar", 2],          "EUR": ["Euro", 2],
        "GBP": ["Pound Sterling", 2],     "JPY": ["Yen", 0],
        "CHF": ["Swiss Franc", 2],        "SEK": ["Swedish Krona", 2],
        "NOK": ["Norwegian Krone", 2],    "DKK": ["Danish Krone", 2],
        "AUD": ["Australian Dollar", 2],  "CAD": ["Canadian Dollar", 2],
        "NZD": ["New Zealand Dollar", 2], "PLN": ["Zloty", 2],
        "CZK": ["Czech Koruna", 2],       "HUF": ["Forint", 2],
        "RON": ["Romanian Leu", 2],       "BGN": ["Bulgarian Lev", 2],
        "TRY": ["Turkish Lira", 2],       "ILS": ["New Israeli Sheqel", 2],
        "ZAR": ["Rand", 2],               "MXN": ["Mexican Peso", 2],
        "BRL": ["Brazilian Real", 2],     "INR": ["Indian Rupee", 2],
        "CNY": ["Yuan Renminbi", 2],      "HKD": ["Hong Kong Dollar", 2],
        "SGD": ["Singapore Dollar", 2],   "MYR": ["Malaysian Ringgit", 2],
        "THB": ["Baht", 2],               "PHP": ["Philippine Peso", 2],
        "IDR": ["Rupiah", 2],             "AED": ["UAE Dirham", 2],
        "SAR": ["Saudi Riyal", 2],        "UAH": ["Hryvnia", 2],
        "KRW": ["Won", 0],                "ISK": ["Iceland Krona", 0],
        "CLP": ["Chilean Peso", 0],       "VND": ["Dong", 0],
        "BHD": ["Bahraini Dinar", 3],     "IQD": ["Iraqi Dinar", 3],
        "JOD": ["Jordanian Dinar", 3],    "KWD": ["Kuwaiti Dinar", 3],
        "LYD": ["Libyan Dinar", 3],       "OMR": ["Rial Omani", 3],
        "TND": ["Tunisian Dinar", 3],     "BTC": ["Bitcoin", 8]
    })

    Component.onCompleted: if (root.visible) root.reload()
    onVisibleChanged: {
        if (root.visible) {
            codeField.clear();
            nameField.clear();
            root.manual = false;
            root.note = "";
            root.reload();
        }
    }

    function reload() {
        Ledger.request("currency.list", {}, (r, e) => { if (!e) root.currencies = r || []; });
    }

    readonly property string code: codeField.text.trim().toUpperCase()
    readonly property bool validCode: /^[A-Z]{3}$/.test(root.code)
    readonly property var match: root.validCode && root.known[root.code] !== undefined
                                 ? root.known[root.code] : null
    readonly property bool existing: (root.currencies || []).some(c => c.code === root.code)

    function add() {
        if (!root.validCode || root.existing)
            return;
        const digits = root.match ? root.match[1] : root.manualDigits;
        const name = root.match ? root.match[0]
                                : (nameField.text.trim().length > 0 ? nameField.text.trim() : root.code);
        Ledger.write("currency.create",
                     { code: root.code, minor_digits: digits, name: name }, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.note = r.code + " added — " + r.name + ", " + r.minor_digits + " decimal places";
            codeField.clear(); nameField.clear();
            root.manual = false;
            root.reload();
            root.changed();
        });
    }

    function remove(c) {
        Ledger.write("currency.delete", { code: c.code }, (r, e) => {
            root.note = e ? e.message : (c.code + " removed");
            if (!e) { root.reload(); root.changed(); }
        });
    }

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Row {
            width: parent.width
            spacing: 8
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "CURRENCIES"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Item { width: parent.width - 260; height: 1 }
            PushButton { label: "Close"; primary: true; onClicked: root.done() }
        }

        // ---- add one ----
        Row {
            spacing: 6
            Field {
                id: codeField
                width: 90
                label: "code"
                placeholder: "CHF"
            }
            Column {
                anchors.verticalCenter: parent.verticalCenter
                spacing: 1
                Text {
                    text: !root.validCode ? "three letters"
                        : root.existing ? "already in this book"
                        : root.match ? root.match[0]
                        : "not a code I know — say how many decimal places"
                    color: root.existing ? Theme.warnAmber
                         : root.match ? Theme.okGreen : Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 2
                }
                Text {
                    visible: root.match !== null && !root.existing
                    text: root.match ? root.match[1] + " decimal places (ISO 4217)" : ""
                    color: Theme.textFaint
                    font.family: Theme.monoFamily
                    font.pixelSize: Theme.fontSize - 4
                }
            }
        }

        // ---- the manual path, only when the code is not one we know ----
        Row {
            visible: root.validCode && !root.existing && root.match === null
            spacing: 6
            Field {
                id: nameField
                width: 170
                label: "name"
                placeholder: root.code
            }
            Column {
                anchors.verticalCenter: parent.verticalCenter
                spacing: 2
                Text {
                    text: "decimal places"
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 4
                }
                Row {
                    spacing: 4
                    Repeater {
                        model: [0, 2, 3, 4, 6, 8]
                        PushButton {
                            required property var modelData
                            implicitWidth: 30
                            implicitHeight: 24
                            label: String(modelData)
                            primary: root.manualDigits === modelData
                            onClicked: root.manualDigits = modelData
                        }
                    }
                }
            }
        }

        Row {
            spacing: 8
            PushButton {
                label: "Add currency"
                primary: root.validCode && !root.existing
                enabled: root.validCode && !root.existing
                onClicked: root.add()
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                width: 420
                wrapMode: Text.Wrap
                text: root.note
                color: root.note.indexOf("added") >= 0 || root.note.indexOf("removed") >= 0
                       ? Theme.okGreen : Theme.red
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 3
            }
        }

        // ---- what the book has ----
        Text {
            text: "IN THIS BOOK"
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
        }
        Flow {
            width: parent.width
            spacing: 6
            Repeater {
                model: root.currencies
                Rectangle {
                    id: chip
                    required property var modelData
                    // The display currency can never be removed and neither can one in use, so
                    // neither gets an × -- a control that only ever refuses is worse than none.
                    readonly property bool used: chip.modelData.accounts > 0
                                                 || chip.modelData.postings > 0
                                                 || chip.modelData.is_display === true
                    implicitWidth: chipRow.implicitWidth + 14
                    height: 26
                    radius: 4
                    color: Theme.surfaceRaised
                    border.width: 1
                    border.color: Theme.line
                    Row {
                        id: chipRow
                        anchors.centerIn: parent
                        spacing: 6
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: chip.modelData.code
                            color: Theme.text
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 1
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            // The scale is shown on every chip, because it is the thing that
                            // silently ruins a book and the thing nobody thinks to check.
                            text: chip.modelData.minor_digits + "dp"
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            visible: chip.used
                            text: chip.modelData.is_display === true
                                  ? "display" : chip.modelData.accounts + " acct"
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            visible: !chip.used
                            text: "×"
                            color: rm.containsMouse ? Theme.red : Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 1
                            MouseArea {
                                id: rm
                                anchors.fill: parent
                                anchors.margins: -4
                                hoverEnabled: true
                                cursorShape: Qt.PointingHandCursor
                                onClicked: root.remove(chip.modelData)
                            }
                        }
                    }
                }
            }
        }
    }
}
