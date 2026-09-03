import QtQuick
import "../services"

// Balance over time, hand-drawn on a Canvas: history to the left of today, projection to the
// right, one line per account or one summed line.
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
//
// THE X AXIS IS TIME, not a slot per point. A balance only has a point on a day it moved, so two
// accounts rarely share dates and a busy account has far more of them than a quiet one; spacing
// points evenly would let the selection change the shape of a line that has not changed, and
// would push "today" wherever the point count happened to put it. Each point sits at its date's
// distance along the window instead. Straight segments between points, as before: a balance is
// really a step, but the corner is the same information and the slope reads better at this size.
Canvas {
    id: root

    // [{ label, currency, colour, points: [{ on: "YYYY-MM-DD", balance_minor: int }] }], each
    // points list ascending by date. History and projection are one list; `todayIso` splits it.
    property var lines: []
    property string todayIso: ""
    // The axis is labelled in this currency; the caller passes the one the lines share.
    property string currency: "GBP"

    onWidthChanged: requestPaint()
    onHeightChanged: requestPaint()
    onLinesChanged: requestPaint()
    onTodayIsoChanged: requestPaint()

    readonly property int padLeft: 72
    readonly property int padBottom: 20
    readonly property int padTop: 8

    function _bounds() {
        let lo = 0, hi = 0, any = false; // always include zero: the crossing is the point
        for (const line of (lines || [])) {
            for (const p of (line.points || [])) {
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
        // Whole minor units, because the axis labels go through Money.format, which is integer
        // arithmetic: a fractional pad would print as "133.32.6000000004".
        const pad = (hi - lo) * 0.08;
        return { lo: Math.floor(lo - pad), hi: Math.ceil(hi + pad) };
    }

    // Every date any line has a point on, ascending and unique.
    function _dates() {
        const seen = {};
        for (const line of (lines || []))
            for (const p of (line.points || []))
                seen[p.on] = true;
        return Object.keys(seen).sort();
    }

    // No currency code on the axis: it does not fit beside a six-figure balance, and the legend
    // chips carry it. When the lines are in different currencies the scale is shared nominally
    // and only the chips can tell them apart.
    function _fmt(minor) {
        return Money.format(Math.round(minor), root.currency);
    }

    // The projection is the same line drawn thinner in the air: it is a forecast, and the eye
    // should read it as one.
    function _faded(colour) {
        const c = typeof colour === "string" ? Qt.color(colour) : colour;
        return Qt.rgba(c.r, c.g, c.b, 0.55);
    }

    // A date's distance along the window, 0..1. ISO dates parse as UTC midnight, so the
    // difference is whole days and daylight saving cannot shift a point.
    function _at(iso, t0, span) {
        return span <= 0 ? 0 : (Date.parse(iso) - t0) / span;
    }

    // One polyline through `pts`, each at the x of its date; a null balance lifts the pen, as
    // MinkaMon does. A lone point is not a line, so it draws nothing.
    function _stroke(ctx, pts, colour, xOf, yOf) {
        if (pts.length < 2)
            return;
        ctx.strokeStyle = colour;
        ctx.lineWidth = 1.6;
        ctx.beginPath();
        let pen = false;
        for (const p of pts) {
            const v = p.balance_minor;
            if (v === null || v === undefined) { pen = false; continue; }
            const x = xOf(p.on), y = yOf(v);
            if (pen) ctx.lineTo(x, y); else ctx.moveTo(x, y);
            pen = true;
        }
        ctx.stroke();
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
        const t0 = dates.length > 0 ? Date.parse(dates[0]) : 0;
        const span = dates.length > 1 ? Date.parse(dates[dates.length - 1]) - t0 : 0;
        const xOf = iso => padLeft + _at(iso, t0, span) * plotW;
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
        if (todayIso.length > 0 && dates.length > 1
            && todayIso > dates[0] && todayIso < dates[dates.length - 1]) {
            const x = xOf(todayIso);
            ctx.strokeStyle = Theme.textFaint;
            ctx.setLineDash([2, 3]);
            ctx.beginPath();
            ctx.moveTo(x, padTop);
            ctx.lineTo(x, padTop + plotH);
            ctx.stroke();
            ctx.setLineDash([]);
            ctx.fillStyle = Theme.textFaint;
            ctx.textAlign = "center";
            ctx.fillText("today", x, padTop + 10);
        }

        // One stroke per line per side of today, so history is solid and the projection faded.
        // The first projected segment starts at the last known point, so the two halves join.
        // (Plain loops rather than callbacks with locals: qmllint silently stops checking a
        // file that declares a const inside a nested closure.)
        const all = lines || [];
        const single = all.length === 1;
        for (let i = 0; i < all.length; i++) {
            const line = all[i];
            const pts = line.points || [];
            if (pts.length === 0)
                continue;
            const last = pts[pts.length - 1].balance_minor;
            // Red once it goes negative is the one piece of colour semantics worth having here;
            // with several lines the palette has to carry the identity, so only the end marker
            // turns red.
            const base = line.colour || Theme.seriesPalette[i % Theme.seriesPalette.length];
            const colour = single && last < 0 ? Theme.red : base;
            const past = pts.filter(p => todayIso.length === 0 || p.on <= todayIso);
            const future = pts.filter(p => todayIso.length > 0 && p.on >= todayIso);
            if (past.length > 0 && future.length > 0 && past[past.length - 1].on !== future[0].on)
                future.unshift(past[past.length - 1]);
            _stroke(ctx, past, colour, xOf, yOf);
            _stroke(ctx, future, _faded(colour), xOf, yOf);
            // Where it ends up.
            const end = pts[pts.length - 1];
            ctx.fillStyle = last < 0 ? Theme.red : colour;
            ctx.beginPath();
            ctx.arc(xOf(end.on), yOf(last), 2.5, 0, Math.PI * 2);
            ctx.fill();
        }
    }
}
