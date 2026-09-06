import QtQuick
import "../services"

// Balance over time, hand-drawn on a Canvas: history to the left of today, projection to the
// right, one line per account or one summed line per currency.
//
// House pattern, following MinkaMon's Sparkline/MultiTrendLine: Canvas + onPaint, repaint on
// resize, series colours from the theme, nulls leave a gap. QtCharts is deliberately not used --
// it would be a dependency for a chart that repaints on interaction rather than per frame, and it
// is awkward to theme against Proustite.
//
// EACH CURRENCY IS SCALED BY ITS OWN PEAK. A book holding £8,000 and €40 on one shared axis draws
// the euro line flat along the bottom, which says nothing about how the euro balance moved. So the
// highest point of every currency lands at the same height and each line is read for its SHAPE.
// The consequence is that heights are no longer comparable between currencies -- deliberately, as
// they never were comparable in the first place -- so the exact numbers come from hovering.
//
// ZERO IS THE HORIZONTAL AXIS and it is shared, because dividing every currency by a positive peak
// keeps zero at zero. A ledger's most important moment is the day a projected balance crosses it.
// When nothing in the window ever goes negative there is no space below the axis at all: the axis
// sits on the floor of the plot and the whole height is spent on the part that exists.
//
// TODAY IS THE VERTICAL AXIS: history to its left, projection to its right.
//
// THE X AXIS IS TIME, not a slot per point. A balance only has a point on a day it moved, so two
// accounts rarely share dates and a busy account has far more of them than a quiet one; spacing
// points evenly would let the selection change the shape of a line that has not changed, and
// would push "today" wherever the point count happened to put it. Each point sits at its date's
// distance along the window instead. Straight segments between points: a balance is really a
// step, but the corner is the same information and the slope reads better at this size.
Item {
    id: root

    // [{ label, currency, colour, points: [{ on: "YYYY-MM-DD", balance_minor: int }] }], each
    // points list ascending by date. History and projection are one list; `todayIso` splits it.
    property var lines: []
    property string todayIso: ""
    // Fallback for a line that carries no currency of its own.
    property string currency: "GBP"

    readonly property int padLeft: 10
    readonly property int padRight: 10
    readonly property int padBottom: 20
    readonly property int padTop: 10

    readonly property real plotW: width - padLeft - padRight
    readonly property real plotH: height - padTop - padBottom

    // Every date any line has a point on, ascending and unique.
    readonly property var dates: {
        const seen = {};
        for (const line of (root.lines || []))
            for (const p of (line.points || []))
                seen[p.on] = true;
        return Object.keys(seen).sort();
    }

    readonly property real t0: root.dates.length > 0 ? Date.parse(root.dates[0]) : 0
    readonly property real tSpan: root.dates.length > 1
                                  ? Date.parse(root.dates[root.dates.length - 1]) - root.t0 : 0

    // Per currency: the value that maps to the top of the plot, plus that currency's extremes.
    // The scale is the peak, so the peak of every currency draws at the same height. A currency
    // that never rises above zero has no peak to align, so its deepest point sets the scale
    // instead and it hangs the full height below the axis.
    readonly property var scales: {
        const hi = {}, lo = {};
        for (const line of (root.lines || [])) {
            const cur = line.currency || root.currency;
            for (const p of (line.points || [])) {
                const v = p.balance_minor;
                if (v === null || v === undefined)
                    continue;
                if (hi[cur] === undefined) { hi[cur] = v; lo[cur] = v; }
                if (v > hi[cur]) hi[cur] = v;
                if (v < lo[cur]) lo[cur] = v;
            }
        }
        const out = {};
        for (const cur of Object.keys(hi))
            out[cur] = { scale: hi[cur] > 0 ? hi[cur] : Math.max(Math.abs(lo[cur]), 1),
                         lo: lo[cur], hi: hi[cur] };
        return out;
    }

    // How far below zero the deepest line reaches, in scaled units. Zero when nothing is
    // negative, which is what removes the negative half of the axis entirely.
    readonly property real unitLo: {
        let u = 0;
        for (const cur of Object.keys(root.scales))
            u = Math.min(u, root.scales[cur].lo / root.scales[cur].scale);
        return u;
    }
    // A little air above the tallest peak so it does not touch the top edge.
    readonly property real unitTop: 1.06

    function _scaleOf(line) {
        const s = root.scales[line.currency || root.currency];
        return s ? s.scale : 1;
    }

    // A date's distance along the window, 0..1. ISO dates parse as UTC midnight, so the
    // difference is whole days and daylight saving cannot shift a point.
    function _at(iso) {
        return root.tSpan <= 0 ? 0 : (Date.parse(iso) - root.t0) / root.tSpan;
    }
    function xOf(iso) {
        return root.padLeft + root._at(iso) * root.plotW;
    }
    function yOfUnit(u) {
        const span = root.unitTop - root.unitLo;
        return root.padTop + root.plotH * (root.unitTop - u) / (span <= 0 ? 1 : span);
    }
    function yOf(line, v) {
        return root.yOfUnit(v / root._scaleOf(line));
    }

    // The projection is the same line drawn thinner in the air: it is a forecast, and the eye
    // should read it as one.
    function _faded(colour) {
        const c = typeof colour === "string" ? Qt.color(colour) : colour;
        return Qt.rgba(c.r, c.g, c.b, 0.55);
    }

    function colourOf(i) {
        const line = (root.lines || [])[i];
        if (!line)
            return Theme.textFaint;
        return line.colour || Theme.seriesPalette[i % Theme.seriesPalette.length];
    }

    // ---- hover ----
    // The pointer picks a date, and every line reports the balance IT HELD on that date. That is
    // the carried-forward value, not a reading off the slope: a balance is a step, so between two
    // points the true figure is the earlier one, and interpolating would invent a number the
    // account never held. The marker dot therefore sits on the step rather than on the drawn
    // segment, which is the honest place for it.
    property real hoverX: -1
    property real hoverY: -1
    readonly property bool hovering: root.hoverX >= 0 && root.dates.length > 0

    // The date under the pointer, clamped to the window.
    readonly property string hoverIso: {
        if (!root.hovering)
            return "";
        if (root.tSpan <= 0 || root.plotW <= 0)
            return root.dates[0];
        const f = Math.max(0, Math.min(1, (root.hoverX - root.padLeft) / root.plotW));
        const d = new Date(root.t0 + f * root.tSpan);
        return Qt.formatDate(d, "yyyy-MM-dd");
    }

    // Balance in force on `iso` for one line: its last point on or before that date. Null before
    // the line starts, so a line that had not opened yet reads as absent rather than as zero.
    function balanceOn(line, iso) {
        const pts = line.points || [];
        let out = null;
        for (const p of pts) {
            if (p.on > iso)
                break;
            if (p.balance_minor !== null && p.balance_minor !== undefined)
                out = p.balance_minor;
        }
        return out;
    }

    // What the readout lists: one row per line that had a balance on the hovered date.
    readonly property var hoverRows: {
        if (!root.hovering)
            return [];
        const rows = [];
        const all = root.lines || [];
        for (let i = 0; i < all.length; i++) {
            const v = root.balanceOn(all[i], root.hoverIso);
            if (v === null)
                continue;
            rows.push({ label: all[i].label || "",
                        currency: all[i].currency || root.currency,
                        minor: v,
                        colour: root.colourOf(i) });
        }
        return rows;
    }

    onHoverIsoChanged: canvas.requestPaint()
    onHoveringChanged: canvas.requestPaint()

    Canvas {
        id: canvas
        anchors.fill: parent

        onWidthChanged: requestPaint()
        onHeightChanged: requestPaint()

        Connections {
            target: root
            function onLinesChanged() { canvas.requestPaint(); }
            function onTodayIsoChanged() { canvas.requestPaint(); }
            function onScalesChanged() { canvas.requestPaint(); }
        }

        // One polyline through `pts`, each at the x of its date; a null balance lifts the pen, as
        // MinkaMon does. A lone point is not a line, so it draws nothing.
        function stroke(ctx, line, pts, colour) {
            if (pts.length < 2)
                return;
            ctx.strokeStyle = colour;
            ctx.lineWidth = 1.6;
            ctx.beginPath();
            let pen = false;
            for (const p of pts) {
                const v = p.balance_minor;
                if (v === null || v === undefined) { pen = false; continue; }
                const x = root.xOf(p.on), y = root.yOf(line, v);
                if (pen) ctx.lineTo(x, y); else ctx.moveTo(x, y);
                pen = true;
            }
            ctx.stroke();
        }

        onPaint: {
            const ctx = getContext("2d");
            ctx.clearRect(0, 0, width, height);
            if (root.plotW <= 0 || root.plotH <= 0)
                return;

            const dates = root.dates;
            const right = root.padLeft + root.plotW;

            // The horizontal axis is zero. With no negative anywhere it lands on the floor of the
            // plot, so nothing is drawn below it and none of the height is wasted.
            const zeroY = root.yOfUnit(0);
            ctx.strokeStyle = Theme.line;
            ctx.lineWidth = 1;
            ctx.beginPath();
            ctx.moveTo(root.padLeft, zeroY);
            ctx.lineTo(right, zeroY);
            ctx.stroke();

            ctx.fillStyle = Theme.textFaint;
            ctx.font = "10px " + Theme.monoFamily;

            if (dates.length > 0) {
                ctx.textAlign = "left";
                ctx.fillText(dates[0], root.padLeft, height - 6);
                ctx.textAlign = "right";
                ctx.fillText(dates[dates.length - 1], right, height - 6);
            }

            // The vertical axis is today: history to its left, projection to its right.
            if (root.todayIso.length > 0 && dates.length > 1
                && root.todayIso > dates[0] && root.todayIso < dates[dates.length - 1]) {
                const tx = root.xOf(root.todayIso);
                ctx.strokeStyle = Theme.line;
                ctx.beginPath();
                ctx.moveTo(tx, root.padTop);
                ctx.lineTo(tx, root.padTop + root.plotH);
                ctx.stroke();
                ctx.fillStyle = Theme.textFaint;
                ctx.textAlign = "center";
                ctx.fillText("today", tx, root.padTop + 9);
            }

            // One stroke per line per side of today, so history is solid and the projection faded.
            // The first projected segment starts at the last known point, so the two halves join.
            // (Plain loops rather than callbacks with locals: qmllint silently stops checking a
            // file that declares a const inside a nested closure.)
            const all = root.lines || [];
            const single = all.length === 1;
            for (let i = 0; i < all.length; i++) {
                const line = all[i];
                const pts = line.points || [];
                if (pts.length === 0)
                    continue;
                const last = pts[pts.length - 1].balance_minor;
                // Red once it goes negative is the one piece of colour semantics worth having
                // here; with several lines the palette has to carry the identity, so only the end
                // marker turns red.
                const base = root.colourOf(i);
                const colour = single && last < 0 ? Theme.red : base;
                const past = pts.filter(p => root.todayIso.length === 0 || p.on <= root.todayIso);
                const future = pts.filter(p => root.todayIso.length > 0 && p.on >= root.todayIso);
                if (past.length > 0 && future.length > 0
                    && past[past.length - 1].on !== future[0].on)
                    future.unshift(past[past.length - 1]);
                canvas.stroke(ctx, line, past, colour);
                canvas.stroke(ctx, line, future, root._faded(colour));
                // Where it ends up.
                const end = pts[pts.length - 1];
                ctx.fillStyle = last < 0 ? Theme.red : colour;
                ctx.beginPath();
                ctx.arc(root.xOf(end.on), root.yOf(line, last), 2.5, 0, Math.PI * 2);
                ctx.fill();
            }

            // The crosshair, and a dot on every line at the balance it held that day.
            if (root.hovering && root.hoverIso.length > 0) {
                const hx = root.xOf(root.hoverIso);
                ctx.strokeStyle = Theme.textFaint;
                ctx.lineWidth = 1;
                ctx.setLineDash([2, 3]);
                ctx.beginPath();
                ctx.moveTo(hx, root.padTop);
                ctx.lineTo(hx, root.padTop + root.plotH);
                ctx.stroke();
                ctx.setLineDash([]);
                for (let j = 0; j < all.length; j++) {
                    const hv = root.balanceOn(all[j], root.hoverIso);
                    if (hv === null)
                        continue;
                    ctx.fillStyle = root.colourOf(j);
                    ctx.beginPath();
                    ctx.arc(hx, root.yOf(all[j], hv), 3, 0, Math.PI * 2);
                    ctx.fill();
                }
            }
        }
    }

    MouseArea {
        anchors.fill: parent
        hoverEnabled: true
        acceptedButtons: Qt.NoButton
        onPositionChanged: mouse => { root.hoverX = mouse.x; root.hoverY = mouse.y; }
        onExited: { root.hoverX = -1; root.hoverY = -1; }
    }

    // The readout: the hovered date and every line's exact balance on it, each in its own
    // currency. This is what replaces the value labels on the axis -- with the currencies scaled
    // apart, a height is no longer a number, so the number has to be asked for.
    Rectangle {
        id: tip
        visible: root.hovering && root.hoverRows.length > 0
        x: Math.max(4, Math.min(root.hoverX + 12, root.width - width - 4))
        y: Math.max(4, Math.min(root.hoverY + 12, root.height - height - 4))
        z: 10
        width: tipCol.implicitWidth + 16
        height: tipCol.implicitHeight + 12
        radius: 4
        color: Theme.surfaceRaised
        border.width: 1
        border.color: Theme.line

        Column {
            id: tipCol
            anchors.left: parent.left
            anchors.top: parent.top
            anchors.margins: 6
            spacing: 2

            Text {
                text: root.hoverIso
                color: Theme.textMuted
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 4
            }
            Repeater {
                model: root.hoverRows
                Row {
                    id: rowItem
                    required property var modelData
                    spacing: 8
                    Rectangle {
                        anchors.verticalCenter: parent.verticalCenter
                        width: 6
                        height: 6
                        radius: 3
                        color: rowItem.modelData.colour
                    }
                    Text {
                        width: 110
                        elide: Text.ElideRight
                        text: rowItem.modelData.label
                        color: Theme.text
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                    Text {
                        text: Money.format(rowItem.modelData.minor, rowItem.modelData.currency)
                              + " " + rowItem.modelData.currency
                        color: rowItem.modelData.minor < 0 ? Theme.red : Theme.text
                        font.family: Theme.monoFamily
                        font.pixelSize: Theme.fontSize - 3
                    }
                }
            }
        }
    }
}
