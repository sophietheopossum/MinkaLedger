pragma Singleton
import QtQuick
import Quickshell

// Currency scale, in one place.
//
// EVERY AMOUNT IN THIS APP IS AN INTEGER IN MINOR UNITS, and the divisor is 10^minor_digits --
// which is NOT always 100. JPY and KRW are 0, the Gulf dinars are 3, BTC is 8. The UI used to
// divide by 100 and parse at 2dp in nine separate places, which was harmless while the book held
// only GBP and silently wrong the moment it did not: a ¥1000 balance rendered as 10.00, and typing
// 1000 into a JPY account recorded ¥10.
//
// The scales come FROM THE CORE (currency.list) rather than being restated here, so there is one
// source of truth and no second table to drift. Formatting is done locally rather than by calling
// money.format per cell, because a list of forty rows should not be forty round trips -- but the
// SCALE it formats with is always the core's.
Singleton {
    id: root

    // code -> minor_digits
    property var scales: ({})
    property bool loaded: false

    function reload() {
        Ledger.request("currency.list", {}, (r, e) => {
            if (e)
                return;
            const next = {};
            for (const c of (r || []))
                next[c.code] = c.minor_digits;
            root.scales = next;
            root.loaded = true;
        });
    }

    Component.onCompleted: root.reload()
    Connections {
        target: Ledger
        function onRevisionChanged() { root.reload(); }
    }

    // Falls back to 2 only when the code is genuinely unknown -- a book always has its currencies,
    // so this is the "before the first load returns" case rather than a guess about the currency.
    function digits(code) {
        const d = root.scales[code];
        return d === undefined ? 2 : d;
    }

    /// Integer minor units -> a decimal string at that currency's scale.
    function format(minor, code) {
        if (minor === null || minor === undefined)
            return "";
        const d = root.digits(code);
        const neg = minor < 0;
        const abs = Math.abs(minor);
        if (d === 0)
            return (neg ? "-" : "") + abs;
        const unit = Math.pow(10, d);
        const whole = Math.floor(abs / unit);
        const frac = String(abs % unit).padStart(d, "0");
        return (neg ? "-" : "") + whole + "." + frac;
    }

    /// The same, with the code appended -- for places where the currency is not otherwise obvious.
    function formatWith(minor, code) {
        return root.format(minor, code) + " " + code;
    }
}
