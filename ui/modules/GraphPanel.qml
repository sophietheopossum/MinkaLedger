pragma ComponentBehavior: Bound
import QtQuick
import "../services"

// Payments as a graph of ACCOUNT VISITS, drawn like a notes graph: circles for the places money
// was, arrows for the payments that moved it, pulled into shape by a spring layout.
//
// WHAT A NODE IS. Not an account and not a payment: one visit to an account. An unlinked payment
// is two nodes and an arrow, because nothing says its arrival at Current was the same visit as
// any other payment's. A link between two payments that touch the same account says exactly
// that, so the core fuses their two visits into one node, and a chain becomes its arrows joined
// end to end. Two chains that converge on the same visit share the node, which carries every
// arrow in and out and is drawn as large as its traffic. Income, expense and equity accounts
// are one shared node each -- every salary diverges from the one Salary, every shop merges into
// the one Groceries -- because those are where a chain begins and ends and one visit there is
// not distinguishable from the next. The core builds all of that (link.graph); this file only
// lays it out and lets you touch it.
//
// LINKING IS A GESTURE ON VISITS. Drag one visit onto another visit to the SAME account and the
// panel links the two payments that make them one: money arrived here, then left from here. That
// is how two chains are made to converge -- drop the arrival of the second chain onto the visit
// the first chain already passes through. Selecting a fused node lists the links that hold it
// together, each with an unlink.
//
// THE LAYOUT IS A SPRING SIMULATION with a bias, not a time axis: shared income nodes are
// drawn leftwards and shared expense nodes rightwards, so flow reads left to right, and
// everything else finds its place between them by what it is connected to. Positions survive a
// reload where the node still exists, so a link made by dragging fuses two circles in place
// rather than reshuffling the picture to explain it.
//
// The whole picture is one Canvas and one MouseArea, with hit-testing done by arithmetic: a
// node is a circle around a point and an arrow is a segment between two, and a few hundred of
// each is nothing to test on every mouse move. Items per node would be the QML way and would
// also mean a few hundred of them re-laid on every tick of the simulation.
Rectangle {
    id: root

    property var accounts: []
    // The window of payments drawn, inclusive ISO dates; blank means unbounded.
    property string from: ""
    property string to: ""
    signal done

    color: Theme.surface
    border.width: 1
    border.color: Theme.line
    radius: 8

    // ---- what the core said ----
    property var nodes: []
    property var edges: []
    property var links: []
    property int shown: 0
    property int total: 0
    property string note: ""

    // ---- the simulation ----
    // Parallel arrays indexed like `nodes`, mutated in place on every tick. Not one object per
    // node in a QML list: nothing binds to a position, the canvas reads them all when it paints,
    // and `tick` is what tells it to.
    property var sim: ({ x: [], y: [], vx: [], vy: [], r: [], held: [] })
    property real alpha: 0
    property int tick: 0

    // ---- the view ----
    property real zoom: 1
    property real panX: 0
    property real panY: 0

    // ---- the pointer ----
    property int hoverNode: -1
    property int hoverEdge: -1
    property int selectedNode: -1
    property int selectedEdge: -1
    property int dragNode: -1
    property int dropTarget: -1     // node under a dragged visit, valid or not
    property bool panning: false
    property real pressX: 0
    property real pressY: 0
    property real lastX: 0
    property real lastY: 0
    property bool moved: false
    property real flowPhase: 0

    readonly property bool sideOpen: root.selectedNode >= 0 || root.selectedEdge >= 0
    // The side box takes a third of the width; the picture is fitted into what is left rather
    // than losing whatever was on the right.
    onSideOpenChanged: root.fit()

    Component.onCompleted: if (root.visible) root.load()
    onVisibleChanged: if (root.visible) root.load()
    onFromChanged: if (root.visible) root.load()
    onToChanged: if (root.visible) root.load()
    Connections {
        target: Ledger
        function onRevisionChanged() { if (root.visible) root.load(); }
    }

    function load() {
        const params = { limit: 400 };
        if (root.from.length > 0) params.from = root.from;
        if (root.to.length > 0) params.to = root.to;
        Ledger.request("link.graph", params, (r, e) => {
            if (e) { root.note = e.message; return; }
            root.note = "";
            root.rebuild(r.nodes || [], r.edges || [], r.links || []);
            root.shown = r.payments;
            root.total = r.total;
        });
    }

    // ---- node geometry and colour ----
    function radiusOf(n) {
        // Proportional to traffic, as asked: a visit two chains converge on is visibly bigger
        // than a stop on one. Capped so a year of salaries does not become the whole picture.
        return Math.min(7 + 3 * ((n["in"] || 0) + (n["out"] || 0)), 44);
    }
    // A link the core could not fuse into a visit, with two nodes to draw it between.
    function loose(l) {
        return !l.shared && typeof l.from_node === "number" && typeof l.to_node === "number";
    }
    function colourOf(n) {
        switch (n.kind) {
        case "income": return Theme.okGreen;
        case "expense": return Theme.redDim;
        case "equity": return Theme.textMuted;
        case "liability": return Theme.warnAmber;
        default: return Theme.purple;
        }
    }
    // Where a shared node wants to sit. Beginnings left, endings right, everything else finds
    // its own place between them.
    function homeX(n) {
        if (n.kind === "income" || n.kind === "equity") return canvas.width * 0.08;
        if (n.kind === "expense") return canvas.width * 0.92;
        return canvas.width * 0.5;
    }

    // A stable name for a node's position across reloads: shared nodes by account, visits by
    // account and any one of their payments, so a visit fused out of two keeps either's place.
    function keysOf(n) {
        if (n.shared) return ["s" + n.account_id];
        return (n.txn_ids || []).map(t => "o" + n.account_id + ":" + t);
    }

    function rebuild(nodes, edges, links) {
        const old = {};
        for (let i = 0; i < root.nodes.length; i++)
            for (const k of root.keysOf(root.nodes[i]))
                old[k] = { x: root.sim.x[i], y: root.sim.y[i] };
        const s = { x: [], y: [], vx: [], vy: [], r: [], held: [] };
        const placed = [];
        for (let i = 0; i < nodes.length; i++) {
            let at = null;
            for (const k of root.keysOf(nodes[i]))
                if (old[k]) { at = old[k]; break; }
            placed.push(at !== null);
            s.x.push(at ? at.x : 0);
            s.y.push(at ? at.y : 0);
            s.vx.push(0);
            s.vy.push(0);
            s.r.push(root.radiusOf(nodes[i]));
            s.held.push(false);
        }
        // New nodes start beside something they are joined to, or failing that on a spiral
        // around the middle (shared ones at their home side), so nothing is born on top of
        // anything else and the springs have a direction to pull in.
        const cx = canvas.width / 2, cy = canvas.height / 2;
        let k = 0;
        for (let i = 0; i < nodes.length; i++) {
            if (placed[i]) continue;
            let near = -1;
            for (const e of edges) {
                if (e.from === i && placed[e.to]) { near = e.to; break; }
                if (e.to === i && placed[e.from]) { near = e.from; break; }
            }
            const angle = k * 2.399963;          // the golden angle: no two on one ray
            const spread = 12 * Math.sqrt(k + 1);
            if (near >= 0) {
                s.x[i] = s.x[near] + Math.cos(angle) * (s.r[near] + 60);
                s.y[i] = s.y[near] + Math.sin(angle) * (s.r[near] + 60);
            } else {
                const hx = nodes[i].shared ? root.homeX(nodes[i]) : cx;
                s.x[i] = hx + Math.cos(angle) * spread * (nodes[i].shared ? 0.4 : 1);
                s.y[i] = cy + Math.sin(angle) * spread;
            }
            placed[i] = true;
            k++;
        }
        // Pointer state first: a hover index into the old arrays must not be read against the
        // new ones by a binding that wakes when they change.
        root.hoverNode = -1;
        root.hoverEdge = -1;
        root.dropTarget = -1;
        if (root.selectedNode >= nodes.length) root.selectedNode = -1;
        if (root.selectedEdge >= edges.length) root.selectedEdge = -1;
        root.sim = s;
        root.nodes = nodes;
        root.edges = edges;
        root.links = links;
        root.alpha = Math.max(root.alpha, 0.6);
        if (root.zoom === 1 && root.panX === 0 && root.panY === 0 && nodes.length > 0)
            settleThenFit.restart();
        canvas.requestPaint();
    }

    function relayout() {
        const keep = root.nodes;
        root.nodes = [];
        root.rebuild(keep, root.edges, root.links);
        root.alpha = 1;
    }

    // ---- the simulation, one tick ----
    // Springs along the arrows, repulsion between every pair, a pull toward home for the shared
    // ends and a weak one toward the middle for everything else. Cooled by `alpha`, which a drag
    // or a reload warms up again, so the picture moves when there is a reason and then stops.
    function step() {
        const s = root.sim, n = root.nodes.length;
        if (n === 0) return;
        const a = root.alpha;
        const fx = new Array(n).fill(0), fy = new Array(n).fill(0);
        for (let i = 0; i < n; i++) {
            for (let j = i + 1; j < n; j++) {
                let dx = s.x[j] - s.x[i], dy = s.y[j] - s.y[i];
                let d2 = dx * dx + dy * dy;
                if (d2 < 1) { dx = (i % 2 ? 1 : -1); dy = (j % 2 ? 1 : -1); d2 = 2; }
                const d = Math.sqrt(d2);
                const push = Math.min(2200 * (1 + (s.r[i] + s.r[j]) / 30) / d2, 60);
                fx[i] -= dx / d * push; fy[i] -= dy / d * push;
                fx[j] += dx / d * push; fy[j] += dy / d * push;
            }
        }
        for (const e of root.edges) {
            const i = e.from, j = e.to;
            if (i === j) continue;
            const dx = s.x[j] - s.x[i], dy = s.y[j] - s.y[i];
            const d = Math.max(Math.sqrt(dx * dx + dy * dy), 1);
            const rest = 70 + s.r[i] + s.r[j];
            const pull = (d - rest) * 0.04;
            fx[i] += dx / d * pull; fy[i] += dy / d * pull;
            fx[j] -= dx / d * pull; fy[j] -= dy / d * pull;
        }
        for (const l of root.links) {
            if (!root.loose(l)) continue;
            const i = l.from_node, j = l.to_node;
            if (i === j || i >= n || j >= n) continue;
            const dx = s.x[j] - s.x[i], dy = s.y[j] - s.y[i];
            const d = Math.max(Math.sqrt(dx * dx + dy * dy), 1);
            const pull = (d - (110 + s.r[i] + s.r[j])) * 0.02;
            fx[i] += dx / d * pull; fy[i] += dy / d * pull;
            fx[j] -= dx / d * pull; fy[j] -= dy / d * pull;
        }
        const cy = canvas.height / 2;
        for (let i = 0; i < n; i++) {
            const node = root.nodes[i];
            const g = node.shared ? 0.12 : 0.003;
            fx[i] += (root.homeX(node) - s.x[i]) * g;
            fy[i] += (cy - s.y[i]) * 0.005;
        }
        for (let i = 0; i < n; i++) {
            if (s.held[i]) { s.vx[i] = 0; s.vy[i] = 0; continue; }
            s.vx[i] = (s.vx[i] + fx[i] * a) * 0.55;
            s.vy[i] = (s.vy[i] + fy[i] * a) * 0.55;
            s.x[i] += s.vx[i];
            s.y[i] += s.vy[i];
        }
        root.alpha = a * 0.975;
        root.tick++;
    }

    Timer {
        interval: 16
        repeat: true
        running: root.visible && root.alpha > 0.01
        onTriggered: { root.step(); canvas.requestPaint(); }
    }
    // The first picture is fitted once it has had a moment to take shape.
    Timer {
        id: settleThenFit
        interval: 900
        onTriggered: root.fit()
    }
    // Moving dots along the arrow under the pointer, from where the money left to where it went.
    Timer {
        interval: 33
        repeat: true
        running: root.visible && (root.hoverEdge >= 0 || root.selectedEdge >= 0)
        onTriggered: { root.flowPhase = (root.flowPhase + 0.018) % 1; canvas.requestPaint(); }
    }

    // ---- view transform ----
    function toWorldX(sx) { return (sx - root.panX) / root.zoom; }
    function toWorldY(sy) { return (sy - root.panY) / root.zoom; }
    function fit() {
        const s = root.sim, n = root.nodes.length;
        if (n === 0) return;
        let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
        for (let i = 0; i < n; i++) {
            x0 = Math.min(x0, s.x[i] - s.r[i]); x1 = Math.max(x1, s.x[i] + s.r[i]);
            y0 = Math.min(y0, s.y[i] - s.r[i]); y1 = Math.max(y1, s.y[i] + s.r[i]);
        }
        const bw = Math.max(x1 - x0, 1) + 90, bh = Math.max(y1 - y0, 1) + 90;
        root.zoom = Math.min(Math.min(canvas.width / bw, canvas.height / bh), 1.6);
        root.panX = canvas.width / 2 - (x0 + x1) / 2 * root.zoom;
        root.panY = canvas.height / 2 - (y0 + y1) / 2 * root.zoom;
        canvas.requestPaint();
    }
    function zoomAt(sx, sy, factor) {
        const next = Math.max(0.2, Math.min(root.zoom * factor, 4));
        const wx = root.toWorldX(sx), wy = root.toWorldY(sy);
        root.zoom = next;
        root.panX = sx - wx * next;
        root.panY = sy - wy * next;
        canvas.requestPaint();
    }

    // ---- hit testing, in screen space ----
    function hitNode(sx, sy, except) {
        const s = root.sim;
        for (let i = root.nodes.length - 1; i >= 0; i--) {
            if (i === except) continue;
            const dx = sx - (s.x[i] * root.zoom + root.panX);
            const dy = sy - (s.y[i] * root.zoom + root.panY);
            if (dx * dx + dy * dy <= Math.pow(s.r[i] * root.zoom + 3, 2)) return i;
        }
        return -1;
    }
    // Where an arrow starts and ends: on the rims of its two circles, not their centres.
    function ends(i, j) {
        const s = root.sim;
        const dx = s.x[j] - s.x[i], dy = s.y[j] - s.y[i];
        const d = Math.max(Math.sqrt(dx * dx + dy * dy), 1);
        const ux = dx / d, uy = dy / d;
        return { x1: s.x[i] + ux * s.r[i], y1: s.y[i] + uy * s.r[i],
                 x2: s.x[j] - ux * (s.r[j] + 2), y2: s.y[j] - uy * (s.r[j] + 2) };
    }
    function hitEdge(sx, sy) {
        const wx = root.toWorldX(sx), wy = root.toWorldY(sy);
        const tol = 6 / root.zoom;
        let best = -1, bestD = tol;
        for (let k = 0; k < root.edges.length; k++) {
            const e = root.edges[k];
            if (e.from === e.to) continue;
            const p = root.ends(e.from, e.to);
            const vx = p.x2 - p.x1, vy = p.y2 - p.y1;
            const len2 = vx * vx + vy * vy;
            if (len2 < 1) continue;
            let t = ((wx - p.x1) * vx + (wy - p.y1) * vy) / len2;
            t = Math.max(0, Math.min(1, t));
            const dx = wx - (p.x1 + vx * t), dy = wy - (p.y1 + vy * t);
            const d = Math.sqrt(dx * dx + dy * dy);
            if (d < bestD) { bestD = d; best = k; }
        }
        return best;
    }

    // ---- linking by dropping one visit on another ----
    // The two payments to link: one that arrived at this account and one that left it, closest
    // in time, so the link says what a chain says -- the money came here, then went on. Failing
    // an arrival-and-departure pair, the two closest in time, older first.
    function paymentsAt(i) {
        const out = [];
        for (const e of root.edges) {
            if (e.to === i) out.push({ txn: e.txn_id, on: e.occurred_on, arrives: true });
            if (e.from === i) out.push({ txn: e.txn_id, on: e.occurred_on, arrives: false });
        }
        return out;
    }
    function linkPair(a, b) {
        let best = null, bestScore = Infinity;
        for (const p of root.paymentsAt(a)) {
            for (const q of root.paymentsAt(b)) {
                if (p.txn === q.txn) continue;
                const days = Math.abs(Date.parse(p.on) - Date.parse(q.on)) / 86400000;
                const score = (p.arrives !== q.arrives ? 0 : 100000) + days;
                if (score < bestScore) { bestScore = score; best = [p, q]; }
            }
        }
        if (!best) return null;
        const [p, q] = best;
        if (p.arrives && !q.arrives) return { from_txn: p.txn, to_txn: q.txn };
        if (!p.arrives && q.arrives) return { from_txn: q.txn, to_txn: p.txn };
        return p.on <= q.on ? { from_txn: p.txn, to_txn: q.txn } : { from_txn: q.txn, to_txn: p.txn };
    }
    function canFuse(a, b) {
        if (a < 0 || b < 0 || a === b) return false;
        const na = root.nodes[a], nb = root.nodes[b];
        return !na.shared && !nb.shared && na.account_id === nb.account_id;
    }
    function fuse(a, b) {
        const pair = root.linkPair(a, b);
        if (!pair) return;
        Ledger.write("link.create", pair, (r, e) => {
            root.note = e ? e.message : "";
        });
    }
    function unlink(fromTxn, toTxn) {
        Ledger.write("link.delete", { from_txn: fromTxn, to_txn: toTxn }, (r, e) => {
            root.note = e ? e.message : "";
        });
    }

    // ---- words for the side box and the tooltip ----
    function edgeAmount(e) {
        const head = Money.format(e.amount_minor, e.currency) + " " + e.currency;
        if (e.to_currency === undefined) return head;
        return head + " → " + Money.format(e.to_minor, e.to_currency) + " " + e.to_currency;
    }
    function edgeRoute(e) {
        return root.nodes[e.from].account + " → " + root.nodes[e.to].account;
    }
    function kindWord(n) {
        return n.shared ? n.kind + " account" : "visit to " + n.kind + " account";
    }
    // The payments touching a node, as drawn: one entry per arrow in or out.
    function nodePayments(i) {
        const rows = [];
        for (const e of root.edges)
            if (e.from === i || e.to === i)
                rows.push({ txn_id: e.txn_id, occurred_on: e.occurred_on, description: e.description,
                            amount: root.edgeAmount(e), route: root.edgeRoute(e), leaves: e.from === i });
        rows.sort((x, y) => x.occurred_on === y.occurred_on ? x.txn_id - y.txn_id
                          : (x.occurred_on < y.occurred_on ? -1 : 1));
        return rows;
    }
    // The links that hold a fused visit together: both ends among its payments.
    function nodeLinks(i) {
        const ids = root.nodes[i].txn_ids || [];
        return root.links.filter(l => ids.indexOf(l.from_txn) >= 0 && ids.indexOf(l.to_txn) >= 0);
    }
    readonly property var selectedPayments: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                             ? root.nodePayments(root.selectedNode) : []
    readonly property var selectedLinks: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                         ? root.nodeLinks(root.selectedNode) : []
    // Re-evaluated when the data does; the tick alone never changes them.
    onNodesChanged: root.tick++
    onEdgesChanged: root.tick++

    Column {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 8

        Row {
            width: parent.width
            spacing: 8
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: "GRAPH"
                color: Theme.textMuted
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: root.shown + " of " + root.total + " payments"
                      + (root.from.length > 0 ? "  since " + root.from : "")
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 3
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                width: Math.max(60, parent.width - 480)
                elide: Text.ElideRight
                text: "drag a visit onto another visit to the same account to link them · wheel zooms · double-click fits"
                color: Theme.textFaint
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize - 4
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "fit"
                onClicked: root.fit()
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "shake"
                onClicked: root.relayout()
            }
            PushButton {
                anchors.verticalCenter: parent.verticalCenter
                label: "Done"
                onClicked: root.done()
            }
        }

        Row {
            width: parent.width
            height: parent.height - 60
            spacing: 10

            // ---- the picture ----
            Rectangle {
                width: root.sideOpen ? parent.width - 270 : parent.width
                height: parent.height
                color: Theme.ground
                radius: 5
                border.width: 1
                border.color: Theme.line
                clip: true

                Canvas {
                    id: canvas
                    anchors.fill: parent
                    anchors.margins: 1
                    onWidthChanged: requestPaint()
                    onHeightChanged: requestPaint()

                    function edgeColour(e, hot) {
                        if (hot) return Theme.purple;
                        if (root.hoverNode >= 0 && (e.from === root.hoverNode || e.to === root.hoverNode))
                            return Theme.textMuted;
                        if (root.selectedNode >= 0 && (e.from === root.selectedNode || e.to === root.selectedNode))
                            return Theme.textMuted;
                        return e.to_currency !== undefined ? Theme.warnAmber : Theme.line;
                    }

                    onPaint: {
                        const ctx = getContext("2d");
                        ctx.reset();
                        ctx.clearRect(0, 0, width, height);
                        const s = root.sim, z = root.zoom;
                        if (root.nodes.length === 0) {
                            ctx.fillStyle = Theme.textFaint;
                            ctx.font = (Theme.fontSize - 1) + "px '" + Theme.fontFamily + "'";
                            ctx.textAlign = "center";
                            ctx.fillText("no payments in this window", width / 2, height / 2);
                            return;
                        }
                        ctx.save();
                        ctx.translate(root.panX, root.panY);
                        ctx.scale(z, z);

                        // Links that fuse nothing: an assertion between two visits, drawn dashed.
                        ctx.setLineDash([4 / z, 4 / z]);
                        ctx.lineWidth = 1 / z;
                        ctx.strokeStyle = Theme.textFaint;
                        for (const l of root.links) {
                            if (!root.loose(l) || l.from_node === l.to_node) continue;
                            const p = root.ends(l.from_node, l.to_node);
                            ctx.beginPath();
                            ctx.moveTo(p.x1, p.y1);
                            ctx.lineTo(p.x2, p.y2);
                            ctx.stroke();
                        }
                        ctx.setLineDash([]);

                        // Arrows, the hot one last so it sits on top.
                        const hot = root.hoverEdge >= 0 ? root.hoverEdge : root.selectedEdge;
                        const order = [];
                        for (let k = 0; k < root.edges.length; k++) if (k !== hot) order.push(k);
                        if (hot >= 0 && hot < root.edges.length) order.push(hot);
                        for (const k of order) {
                            const e = root.edges[k];
                            if (e.from === e.to) continue;
                            const p = root.ends(e.from, e.to);
                            const isHot = k === hot;
                            const colour = canvas.edgeColour(e, isHot);
                            ctx.strokeStyle = colour;
                            ctx.fillStyle = colour;
                            ctx.lineWidth = (isHot ? 2.2 : 1.2) / z;
                            ctx.beginPath();
                            ctx.moveTo(p.x1, p.y1);
                            ctx.lineTo(p.x2, p.y2);
                            ctx.stroke();
                            // The head: a small triangle on the rim of the arriving circle.
                            const dx = p.x2 - p.x1, dy = p.y2 - p.y1;
                            const d = Math.max(Math.sqrt(dx * dx + dy * dy), 1);
                            const ux = dx / d, uy = dy / d;
                            const h = (isHot ? 9 : 7) / Math.sqrt(z);
                            ctx.beginPath();
                            ctx.moveTo(p.x2, p.y2);
                            ctx.lineTo(p.x2 - ux * h - uy * h * 0.5, p.y2 - uy * h + ux * h * 0.5);
                            ctx.lineTo(p.x2 - ux * h + uy * h * 0.5, p.y2 - uy * h - ux * h * 0.5);
                            ctx.closePath();
                            ctx.fill();
                            // Flow: dots travelling from where the money left toward where it
                            // arrived, brighter as they go, so the direction reads at a glance.
                            if (isHot && d > 8) {
                                const count = Math.max(3, Math.min(8, Math.round(d / 40)));
                                for (let i = 0; i < count; i++) {
                                    const t = (root.flowPhase + i / count) % 1;
                                    ctx.fillStyle = Qt.alpha(Theme.purple, 0.35 + 0.65 * t);
                                    ctx.beginPath();
                                    ctx.arc(p.x1 + dx * t, p.y1 + dy * t, (2 + 2 * t) / Math.sqrt(z), 0, Math.PI * 2);
                                    ctx.fill();
                                }
                            }
                        }

                        // Circles.
                        for (let i = 0; i < root.nodes.length; i++) {
                            const n = root.nodes[i];
                            const colour = root.colourOf(n);
                            const lit = i === root.hoverNode || i === root.selectedNode || i === root.dragNode;
                            const target = i === root.dropTarget && root.dragNode >= 0;
                            ctx.beginPath();
                            ctx.arc(s.x[i], s.y[i], s.r[i], 0, Math.PI * 2);
                            ctx.fillStyle = Qt.alpha(colour, n.shared ? 0.4 : (lit ? 0.45 : 0.28));
                            ctx.fill();
                            if (target) {
                                ctx.strokeStyle = root.canFuse(root.dragNode, i) ? Theme.okGreen : Theme.red;
                                ctx.lineWidth = 3 / z;
                            } else {
                                ctx.strokeStyle = lit ? Theme.text : colour;
                                ctx.lineWidth = (lit ? 2 : 1.2) / z;
                            }
                            ctx.stroke();
                        }

                        // Names: always on shared and busy nodes, otherwise only when pointed at,
                        // so a year of small visits is not a year of overlapping words.
                        ctx.textAlign = "center";
                        ctx.textBaseline = "top";
                        for (let i = 0; i < root.nodes.length; i++) {
                            const n = root.nodes[i];
                            const lit = i === root.hoverNode || i === root.selectedNode || i === root.dragNode;
                            const busy = n.shared || s.r[i] >= 16;
                            if (!lit && !busy && z < 0.9) continue;
                            const px = Math.round((busy ? Theme.fontSize - 2 : Theme.fontSize - 4) / Math.sqrt(z));
                            ctx.font = px + "px '" + Theme.fontFamily + "'";
                            ctx.fillStyle = lit || busy ? Theme.text : Theme.textMuted;
                            ctx.fillText(n.account, s.x[i], s.y[i] + s.r[i] + 3);
                            if (!n.shared && (n.txn_ids || []).length > 1) {
                                ctx.font = Math.max(px - 2, 6) + "px '" + Theme.monoFamily + "'";
                                ctx.fillStyle = Theme.textFaint;
                                ctx.fillText(n.txn_ids.length + " payments", s.x[i], s.y[i] + s.r[i] + 3 + px + 1);
                            }
                        }
                        ctx.restore();
                    }
                }

                MouseArea {
                    id: pointer
                    anchors.fill: parent
                    hoverEnabled: true
                    acceptedButtons: Qt.LeftButton
                    cursorShape: root.dragNode >= 0 ? Qt.ClosedHandCursor
                               : root.hoverNode >= 0 ? Qt.OpenHandCursor
                               : root.hoverEdge >= 0 ? Qt.PointingHandCursor
                               : root.panning ? Qt.ClosedHandCursor : Qt.ArrowCursor

                    onPressed: mouse => {
                        root.pressX = mouse.x; root.pressY = mouse.y;
                        root.lastX = mouse.x; root.lastY = mouse.y;
                        root.moved = false;
                        const hn = root.hitNode(mouse.x, mouse.y, -1);
                        if (hn >= 0) {
                            root.dragNode = hn;
                            root.sim.held[hn] = true;
                        } else {
                            root.panning = true;
                        }
                    }
                    onPositionChanged: mouse => {
                        if (root.dragNode >= 0) {
                            const i = root.dragNode;
                            root.sim.x[i] = root.toWorldX(mouse.x);
                            root.sim.y[i] = root.toWorldY(mouse.y);
                            root.moved = root.moved || Math.abs(mouse.x - root.pressX) + Math.abs(mouse.y - root.pressY) > 3;
                            root.dropTarget = root.moved ? root.hitNode(mouse.x, mouse.y, i) : -1;
                            root.alpha = Math.max(root.alpha, 0.15);
                            canvas.requestPaint();
                        } else if (root.panning) {
                            root.panX += mouse.x - root.lastX;
                            root.panY += mouse.y - root.lastY;
                            root.moved = root.moved || Math.abs(mouse.x - root.pressX) + Math.abs(mouse.y - root.pressY) > 3;
                            canvas.requestPaint();
                        } else {
                            const hn = root.hitNode(mouse.x, mouse.y, -1);
                            const he = hn >= 0 ? -1 : root.hitEdge(mouse.x, mouse.y);
                            if (hn !== root.hoverNode || he !== root.hoverEdge) {
                                root.hoverNode = hn;
                                root.hoverEdge = he;
                                canvas.requestPaint();
                            }
                        }
                        root.lastX = mouse.x; root.lastY = mouse.y;
                    }
                    onReleased: mouse => {
                        if (root.dragNode >= 0) {
                            const i = root.dragNode;
                            root.sim.held[i] = false;
                            if (root.moved) {
                                if (root.dropTarget >= 0) {
                                    if (root.canFuse(i, root.dropTarget))
                                        root.fuse(i, root.dropTarget);
                                    else if (root.nodes[root.dropTarget].shared || root.nodes[i].shared)
                                        root.note = "only visits can be joined: " + root.nodes[root.dropTarget].account
                                                  + " is a shared " + root.nodes[root.dropTarget].kind + " node";
                                    else
                                        root.note = "these are visits to different accounts ("
                                                  + root.nodes[i].account + " and "
                                                  + root.nodes[root.dropTarget].account + ") and cannot be one";
                                }
                            } else {
                                root.selectedNode = root.selectedNode === i ? -1 : i;
                                root.selectedEdge = -1;
                            }
                            root.dragNode = -1;
                            root.dropTarget = -1;
                            root.alpha = Math.max(root.alpha, 0.1);
                        } else if (root.panning) {
                            root.panning = false;
                            if (!root.moved) {
                                const he = root.hitEdge(mouse.x, mouse.y);
                                root.selectedEdge = he >= 0 && he !== root.selectedEdge ? he : -1;
                                root.selectedNode = -1;
                            }
                        }
                        canvas.requestPaint();
                    }
                    onExited: {
                        if (root.dragNode < 0 && !root.panning) {
                            root.hoverNode = -1;
                            root.hoverEdge = -1;
                            canvas.requestPaint();
                        }
                    }
                    onWheel: wheel => {
                        root.zoomAt(wheel.x, wheel.y, wheel.angleDelta.y > 0 ? 1.15 : 1 / 1.15);
                    }
                    onDoubleClicked: root.fit()
                }

                // What the pointer is over, beside it.
                Rectangle {
                    id: tip
                    visible: root.dragNode < 0 && !root.panning && (root.hoverEdge >= 0 || root.hoverNode >= 0)
                    x: Math.min(pointer.mouseX + 16, parent.width - width - 6)
                    y: Math.min(pointer.mouseY + 16, parent.height - height - 6)
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
                        spacing: 1
                        Text {
                            text: root.hoverEdge >= 0 ? root.edges[root.hoverEdge].description
                                : root.hoverNode >= 0 ? root.nodes[root.hoverNode].account : ""
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            text: root.hoverEdge >= 0
                                  ? root.edges[root.hoverEdge].occurred_on + "  " + root.edgeAmount(root.edges[root.hoverEdge])
                                : root.hoverNode >= 0
                                  ? root.kindWord(root.nodes[root.hoverNode]) + " · "
                                    + root.nodes[root.hoverNode]["in"] + " in, " + root.nodes[root.hoverNode]["out"] + " out"
                                  : ""
                            color: Theme.textMuted
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Text {
                            visible: root.hoverEdge >= 0
                            text: root.hoverEdge >= 0 ? root.edgeRoute(root.edges[root.hoverEdge]) : ""
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                    }
                }
            }

            // ---- what is selected ----
            Rectangle {
                visible: root.sideOpen
                width: 260
                height: parent.height
                color: Theme.ground
                radius: 5
                border.width: 1
                border.color: Theme.purple

                Flickable {
                    anchors.fill: parent
                    anchors.margins: 8
                    contentHeight: sideCol.implicitHeight
                    clip: true

                    Column {
                        id: sideCol
                        width: parent.width
                        spacing: 4

                        Row {
                            width: parent.width
                            spacing: 6
                            Text {
                                anchors.verticalCenter: parent.verticalCenter
                                width: parent.width - 70
                                elide: Text.ElideRight
                                text: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                      ? root.nodes[root.selectedNode].account.toUpperCase()
                                    : root.selectedEdge >= 0 && root.selectedEdge < root.edges.length
                                      ? "PAYMENT #" + root.edges[root.selectedEdge].txn_id : ""
                                color: Theme.purple
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                            }
                            PushButton {
                                implicitHeight: 22
                                label: "close"
                                onClicked: { root.selectedNode = -1; root.selectedEdge = -1; }
                            }
                        }

                        // A node: what it is, the payments through it, and the links holding a
                        // fused visit together.
                        Text {
                            visible: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                  ? root.kindWord(root.nodes[root.selectedNode]) + ", "
                                    + root.nodes[root.selectedNode].currency + " · "
                                    + root.nodes[root.selectedNode]["in"] + " in, "
                                    + root.nodes[root.selectedNode]["out"] + " out"
                                  : ""
                            color: Theme.textMuted
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 3
                        }
                        Text {
                            visible: root.selectedNode >= 0 && root.selectedPayments.length > 0
                            text: "PAYMENTS"
                            color: Theme.textMuted
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Repeater {
                            model: root.selectedNode >= 0 ? root.selectedPayments : []
                            Column {
                                id: pay
                                required property var modelData
                                width: parent.width
                                spacing: 0
                                Row {
                                    width: parent.width
                                    spacing: 6
                                    Text {
                                        text: pay.modelData.leaves ? "↗" : "↘"
                                        color: pay.modelData.leaves ? Theme.warnAmber : Theme.okGreen
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                    Text {
                                        text: pay.modelData.occurred_on
                                        color: Theme.textFaint
                                        font.family: Theme.monoFamily
                                        font.pixelSize: Theme.fontSize - 4
                                    }
                                    Text {
                                        width: parent.width - 100
                                        elide: Text.ElideRight
                                        text: pay.modelData.description
                                        color: Theme.text
                                        font.family: Theme.fontFamily
                                        font.pixelSize: Theme.fontSize - 3
                                    }
                                }
                                Text {
                                    x: 16
                                    width: parent.width - 16
                                    elide: Text.ElideRight
                                    text: pay.modelData.amount + "   " + pay.modelData.route
                                    color: Theme.textFaint
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 5
                                }
                            }
                        }
                        Text {
                            visible: root.selectedNode >= 0 && root.selectedLinks.length > 0
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: "LINKS THROUGH THIS VISIT"
                            color: Theme.textMuted
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Repeater {
                            model: root.selectedNode >= 0 ? root.selectedLinks : []
                            Row {
                                id: lnk
                                required property var modelData
                                spacing: 6
                                Text {
                                    text: "#" + lnk.modelData.from_txn + " → #" + lnk.modelData.to_txn
                                        + (lnk.modelData.note ? "  " + lnk.modelData.note : "")
                                    color: Theme.textFaint
                                    font.family: Theme.monoFamily
                                    font.pixelSize: Theme.fontSize - 4
                                }
                                Text {
                                    text: "unlink"
                                    color: uhov.containsMouse ? Theme.red : Theme.textFaint
                                    font.family: Theme.fontFamily
                                    font.pixelSize: Theme.fontSize - 4
                                    MouseArea {
                                        id: uhov
                                        anchors.fill: parent
                                        anchors.margins: -3
                                        hoverEnabled: true
                                        cursorShape: Qt.PointingHandCursor
                                        onClicked: root.unlink(lnk.modelData.from_txn, lnk.modelData.to_txn)
                                    }
                                }
                            }
                        }
                        Text {
                            visible: root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                     && !root.nodes[root.selectedNode].shared
                                     && root.selectedLinks.length === 0
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: "One visit, one payment. Drag it onto another visit to "
                                + (root.selectedNode >= 0 && root.selectedNode < root.nodes.length
                                   ? root.nodes[root.selectedNode].account : "this account")
                                + " to say they were the same stop."
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }

                        // An arrow: the payment it is.
                        Text {
                            visible: root.selectedEdge >= 0 && root.selectedEdge < root.edges.length
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: root.selectedEdge >= 0 && root.selectedEdge < root.edges.length
                                  ? root.edges[root.selectedEdge].description : ""
                            color: Theme.text
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                        Text {
                            visible: root.selectedEdge >= 0 && root.selectedEdge < root.edges.length
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: root.selectedEdge >= 0 && root.selectedEdge < root.edges.length
                                  ? root.edges[root.selectedEdge].occurred_on + "\n"
                                    + root.edgeAmount(root.edges[root.selectedEdge]) + "\n"
                                    + root.edgeRoute(root.edges[root.selectedEdge])
                                    + (root.edges[root.selectedEdge].links > 0
                                       ? "\n⛓ " + root.edges[root.selectedEdge].links + " link"
                                         + (root.edges[root.selectedEdge].links === 1 ? "" : "s")
                                       : "")
                                  : ""
                            color: Theme.textMuted
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                        Text {
                            visible: root.selectedEdge >= 0
                            width: parent.width
                            wrapMode: Text.Wrap
                            text: "Each end is one visit to that account. To make this payment part of a chain, "
                                + "drag one of its ends onto the visit the chain already passes through."
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 4
                        }
                    }
                }
            }
        }

        Text {
            width: parent.width
            wrapMode: Text.Wrap
            visible: root.note.length > 0
            text: root.note
            color: Theme.red
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
        }
    }
}
