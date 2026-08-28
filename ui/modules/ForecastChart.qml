import QtQuick
import "../services"

// Projected balance over time, hand-drawn on a Canvas.
//
// House pattern, following MinkaMon's Sparkline/MultiTrendLine: Canvas + onPaint, repaint on
// resize, series colours from the theme, nulls leave a gap. QtCharts is deliberately not used --
// it would be a dependency for a chart that repaints on interaction rather than per frame, and it
// is awkward to theme against Proustite.
//
// UNLIKE MinkaMon's charts, the Y axis here MUST span negatives. A ledger's most important moment
// is the day a projected balance crosses zero, and MinkaMon's charts clamp to a 0..max range
// because CPU percentages never go below it. Getting that wrong would hide the single thing this
// view exists to show.
Canvas {
    id: root

    // [{ on: "YYYY-MM-DD", balance_minor: int }], ascending by date.
    property var series: []
    // Optional second series drawn dimmer, for a scenario comparison.
    property var compare: []
    property string todayIso: ""
    property int minorDigits: 2

    onWidthChanged: requestPaint()
    onHeightChanged: requestPaint()
    onSeriesChanged: requestPaint()
    onCompareChanged: requestPaint()

    readonly property int padLeft: 64
    readonly property int padBottom: 20
    readonly property int padTop: 8

    function _bounds() {
        let lo = 0, hi = 0, any = false; // always include zero: the crossing is the point
        for (const set of [series, compare]) {
            for (const p of (set || [])) {
                const v = p.balance_minor;
                if (v === null || v === undefined)
                    continue;
                if (!any) { lo = Math.min(0, v); hi = Math.max(0, v); any = true; }
                lo = Math.min(lo, v);
                hi = Math.max(hi, v);
            }
        }
        if (!any)
            return { lo: -1, hi: 1 };
        if (lo === hi) { lo -= 1; hi += 1; }
        const pad = (hi - lo) * 0.08;
        return { lo: lo - pad, hi: hi + pad };
    }

    function _dates() {
        const out = [];
        for (const p of (series || []))
            out.push(p.on);
        return out;
    }

    function _fmt(minor) {
        const div = Math.pow(10, minorDigits);
        const v = minor / div;
        return (v < 0 ? "-" : "") + "£" + Math.abs(v).toFixed(minorDigits === 0 ? 0 : 2);
    }

    onPaint: {
        const ctx = getContext("2d");
        ctx.clearRect(0, 0, width, height);
        const b = _bounds();
        const plotW = width - padLeft - 8;
        const plotH = height - padTop - padBottom;
        if (plotW <= 0 || plotH <= 0)
            return;

        const dates = _dates();
        const n = Math.max(dates.length, 2);
        const xOf = i => padLeft + (n === 1 ? 0 : (i / (n - 1)) * plotW);
        const yOf = v => padTop + plotH - ((v - b.lo) / (b.hi - b.lo)) * plotH;

        // Zero line first, so a negative balance is unmistakable.
        const zeroY = yOf(0);
        ctx.strokeStyle = Theme.line;
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.moveTo(padLeft, zeroY);
        ctx.lineTo(width - 8, zeroY);
        ctx.stroke();

        // Axis labels: just the extremes and zero. More would need a real tick algorithm and this
        // is a glanceable chart, not a report.
        ctx.fillStyle = Theme.textFaint;
        ctx.font = "10px " + Theme.monoFamily;
        ctx.textAlign = "right";
        ctx.fillText(_fmt(b.hi), padLeft - 6, padTop + 10);
        ctx.fillText(_fmt(0), padLeft - 6, zeroY + 3);
        ctx.fillText(_fmt(b.lo), padLeft - 6, padTop + plotH);

        if (dates.length > 0) {
            ctx.textAlign = "left";
            ctx.fillText(dates[0], padLeft, height - 6);
            ctx.textAlign = "right";
            ctx.fillText(dates[dates.length - 1], width - 8, height - 6);
        }

        // The today divider: everything right of it is projection, not history.
        if (todayIso.length > 0 && dates.length > 1) {
            let idx = -1;
            for (let i = 0; i < dates.length; i++)
                if (dates[i] >= todayIso) { idx = i; break; }
            if (idx > 0) {
                const x = xOf(idx);
                ctx.strokeStyle = Theme.textFaint;
                ctx.setLineDash([2, 3]);
                ctx.beginPath();
                ctx.moveTo(x, padTop);
                ctx.lineTo(x, padTop + plotH);
                ctx.stroke();
                ctx.setLineDash([]);
            }
        }

        const drawLine = (set, colour, wide) => {
            if (!set || set.length === 0)
                return;
            ctx.strokeStyle = colour;
            ctx.lineWidth = wide ? 1.6 : 1.0;
            ctx.beginPath();
            let pen = false;
            for (let i = 0; i < set.length; i++) {
                const v = set[i].balance_minor;
                if (v === null || v === undefined) { pen = false; continue; } // gap, as MinkaMon does
                const x = xOf(i), y = yOf(v);
                if (pen) ctx.lineTo(x, y); else ctx.moveTo(x, y);
                pen = true;
            }
            ctx.stroke();
        };

        drawLine(compare, Theme.textFaint, false);
        // Red once it goes negative is the one piece of colour semantics worth having here.
        const endsNegative = series.length > 0
            && series[series.length - 1].balance_minor < 0;
        drawLine(series, endsNegative ? Theme.red : Theme.okGreen, true);
    }
}
