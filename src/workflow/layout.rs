//! Pure layered DAG layout: a run graph plus an area in, node rectangles and
//! edge direction bits out.
//!
//! Kept free of `src/ui` and of ratatui on purpose
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §8): the renderer
//! converts [`EdgeBits`] into its own line-cell type at draw time, and
//! [`LayoutRect`] into a ratatui `Rect`. Layout runs once, in the view
//! computation pass; render and hit-testing then read the same stored
//! geometry, so the clickable rectangles can never disagree with what was
//! drawn.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::workflow::model::{InstancePath, RunGraph, RunNodeIdx};

/// A terminal-cell rectangle. Mirrors ratatui's `Rect` without depending on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl LayoutRect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.height)
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        !self.is_empty()
            && col >= self.x
            && col < self.right()
            && row >= self.y
            && row < self.bottom()
    }

    pub fn intersects(&self, other: &Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Which directions an edge occupies in one cell. Deliberately not
/// `crate::ui`'s line-cell type: this module must not depend on the UI layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EdgeBits {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

impl EdgeBits {
    pub fn merged(self, other: Self) -> Self {
        Self {
            up: self.up || other.up,
            down: self.down || other.down,
            left: self.left || other.left,
            right: self.right || other.right,
        }
    }

    pub fn is_empty(self) -> bool {
        !(self.up || self.down || self.left || self.right)
    }
}

/// One edge's route, kept per edge rather than pre-merged into a single cell
/// map so that clipping can drop exactly the cells belonging to an edge whose
/// endpoint box did not survive — an edge stub pointing at a box that was never
/// drawn is worse than no edge at all.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRoute {
    pub from: RunNodeIdx,
    pub to: RunNodeIdx,
    /// Cells in ascending `(x, y)` order, so the same graph always produces the
    /// same route.
    pub cells: Vec<((u16, u16), EdgeBits)>,
}

/// The layout output stored in the view state and read by both the renderer
/// and the hit-test.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DagLayout {
    pub nodes: Vec<(RunNodeIdx, LayoutRect)>,
    pub edges: Vec<EdgeRoute>,
}

impl DagLayout {
    pub fn rect_of(&self, idx: RunNodeIdx) -> Option<LayoutRect> {
        self.nodes
            .iter()
            .find(|(node, _)| *node == idx)
            .map(|(_, rect)| *rect)
    }

    /// Hit-test against exactly the rectangles the renderer drew.
    pub fn node_at(&self, col: u16, row: u16) -> Option<RunNodeIdx> {
        self.nodes
            .iter()
            .find(|(_, rect)| rect.contains(col, row))
            .map(|(idx, _)| *idx)
    }

    /// Every surviving edge cell, merged across edges that share it. Derived
    /// rather than stored so it can never disagree with [`DagLayout::edges`].
    pub fn edge_cells(&self) -> HashMap<(u16, u16), EdgeBits> {
        let mut merged: HashMap<(u16, u16), EdgeBits> = HashMap::new();
        for route in &self.edges {
            for (position, bits) in &route.cells {
                let entry = merged.entry(*position).or_default();
                *entry = entry.merged(*bits);
            }
        }
        merged
    }

    /// Drops every edge whose source or target box is no longer part of the
    /// layout, then every remaining cell that falls outside `bounds`.
    ///
    /// Both halves matter: bounds alone leaves the orphan stubs that hang under
    /// a clipped-away node, and endpoint survival alone leaves cells drawn
    /// outside the graph band.
    pub fn retain_visible_edges(&mut self, bounds: LayoutRect) {
        let surviving: HashSet<RunNodeIdx> = self.nodes.iter().map(|(idx, _)| *idx).collect();
        self.edges
            .retain(|route| surviving.contains(&route.from) && surviving.contains(&route.to));
        for route in &mut self.edges {
            route.cells.retain(|((x, y), _)| bounds.contains(*x, *y));
        }
        self.edges.retain(|route| !route.cells.is_empty());
    }
}

// ── geometry constants ──────────────────────────────────────────────────────

/// Fixed node-box size: label + status glyph + one-line status
/// (`04-kvdag-and-execution.md` §8), similar density to the agent panel.
const NODE_WIDTH: u16 = 22;
const NODE_HEIGHT: u16 = 3;
/// Horizontal room between columns in the same layer.
const COLUMN_GAP: u16 = 2;
/// Vertical room between layers, reserved for the edge-routing "drop" band.
const ROW_GAP: u16 = 2;

/// Layered layout: layer by longest path from the roots, order within a layer
/// by a barycenter heuristic with an `instance_path` stability tiebreak, then
/// route every edge as an orthogonal drop/jog/drop through the gap between
/// layers (`04-kvdag-and-execution.md` §8).
///
/// An empty graph yields an empty layout, which renders and hit-tests as
/// "nothing here" rather than as stale geometry.
pub fn layout(graph: &RunGraph, area: LayoutRect) -> DagLayout {
    if graph.nodes.is_empty() {
        return DagLayout::default();
    }

    let layer_of = assign_layers(graph);
    let ordered = order_within_layers(graph, &layer_of);

    let mut rects: HashMap<RunNodeIdx, LayoutRect> = HashMap::new();
    for (layer_index, column) in ordered.iter().enumerate() {
        let y = area
            .y
            .saturating_add((layer_index as u16).saturating_mul(NODE_HEIGHT + ROW_GAP));
        for (position, idx) in column.iter().enumerate() {
            let x = area
                .x
                .saturating_add((position as u16).saturating_mul(NODE_WIDTH + COLUMN_GAP));
            rects.insert(*idx, LayoutRect::new(x, y, NODE_WIDTH, NODE_HEIGHT));
        }
    }

    // Iterate `graph.nodes` (not the per-layer buckets) so the output order is
    // the graph's own node order, independent of layering internals — part of
    // what makes the same graph always produce byte-identical output.
    let nodes: Vec<(RunNodeIdx, LayoutRect)> = graph
        .nodes
        .iter()
        .filter_map(|node| rects.get(&node.idx).map(|rect| (node.idx, *rect)))
        .collect();

    let mut edges: Vec<EdgeRoute> = Vec::new();
    for edge in &graph.edges {
        let (Some(&from_rect), Some(&to_rect)) = (rects.get(&edge.from), rects.get(&edge.to))
        else {
            continue;
        };
        let cells = route_edge(from_rect, to_rect);
        if cells.is_empty() {
            continue;
        }
        edges.push(EdgeRoute {
            from: edge.from,
            to: edge.to,
            cells,
        });
    }

    DagLayout { nodes, edges }
}

/// Layer assignment: longest path from the roots, computed with Kahn's
/// algorithm so a node's layer is only finalised once every predecessor's
/// layer already is.
fn assign_layers(graph: &RunGraph) -> HashMap<RunNodeIdx, usize> {
    let mut indegree: HashMap<RunNodeIdx, usize> = graph.nodes.iter().map(|n| (n.idx, 0)).collect();
    for edge in &graph.edges {
        if let Some(count) = indegree.get_mut(&edge.to) {
            *count += 1;
        }
    }

    let mut layer: HashMap<RunNodeIdx, usize> = graph.nodes.iter().map(|n| (n.idx, 0)).collect();
    let mut queue: VecDeque<RunNodeIdx> = graph
        .nodes
        .iter()
        .filter(|n| indegree.get(&n.idx).copied().unwrap_or(0) == 0)
        .map(|n| n.idx)
        .collect();

    let mut processed: HashSet<RunNodeIdx> = HashSet::new();
    while let Some(idx) = queue.pop_front() {
        if !processed.insert(idx) {
            continue;
        }
        let current_layer = layer.get(&idx).copied().unwrap_or(0);
        for edge_index in graph.outbound(idx) {
            let Some(edge) = graph.edges.get(edge_index) else {
                continue;
            };
            if let Some(entry) = layer.get_mut(&edge.to) {
                *entry = (*entry).max(current_layer + 1);
            }
            if let Some(count) = indegree.get_mut(&edge.to) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    queue.push_back(edge.to);
                }
            }
        }
    }

    // A node left over here sits on a cycle, which `Kvdag::try_new` already
    // rejects for any graph that reaches this layout — this is a defensive
    // fallback so a pure function never loops or panics on malformed input,
    // not a real code path for valid graphs.
    let max_processed = layer.values().copied().max().unwrap_or(0);
    let mut leftover: Vec<RunNodeIdx> = graph
        .nodes
        .iter()
        .map(|n| n.idx)
        .filter(|idx| !processed.contains(idx))
        .collect();
    leftover.sort();
    for idx in leftover {
        layer.insert(idx, max_processed + 1);
    }

    layer
}

/// Orders every layer left to right by a barycenter heuristic over already-
/// ranked predecessor layers, tiebroken by `instance_path` so a node's column
/// only moves when its predecessors actually do.
fn order_within_layers(
    graph: &RunGraph,
    layer_of: &HashMap<RunNodeIdx, usize>,
) -> Vec<Vec<RunNodeIdx>> {
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut buckets: Vec<Vec<RunNodeIdx>> = vec![Vec::new(); max_layer + 1];
    for node in &graph.nodes {
        if let Some(&layer) = layer_of.get(&node.idx) {
            buckets[layer].push(node.idx);
        }
    }

    let paths: HashMap<RunNodeIdx, &InstancePath> = graph
        .nodes
        .iter()
        .map(|node| (node.idx, &node.path))
        .collect();

    let mut rank: HashMap<RunNodeIdx, usize> = HashMap::new();
    for bucket in &mut buckets {
        bucket.sort_by(|a, b| {
            barycenter(graph, &rank, *a)
                .total_cmp(&barycenter(graph, &rank, *b))
                .then_with(|| paths.get(a).cmp(&paths.get(b)))
        });
        for (position, idx) in bucket.iter().enumerate() {
            rank.insert(*idx, position);
        }
    }

    buckets
}

/// Mean column rank of `idx`'s predecessors in their (already-ranked) layers.
/// A node with no ranked predecessor yet (only layer 0's roots) sorts purely
/// by `instance_path`.
fn barycenter(graph: &RunGraph, rank: &HashMap<RunNodeIdx, usize>, idx: RunNodeIdx) -> f64 {
    let predecessor_ranks: Vec<f64> = graph
        .inbound(idx)
        .filter_map(|edge_index| graph.edges.get(edge_index))
        .filter_map(|edge| rank.get(&edge.from))
        .map(|&r| r as f64)
        .collect();
    if predecessor_ranks.is_empty() {
        return 0.0;
    }
    predecessor_ranks.iter().sum::<f64>() / predecessor_ranks.len() as f64
}

/// One edge's orthogonal drop/jog/drop route as its own cell list, in ascending
/// `(x, y)` order. Cells the route visits twice are merged here; cells shared
/// with *another* edge are merged later by [`DagLayout::edge_cells`].
fn route_edge(from: LayoutRect, to: LayoutRect) -> Vec<((u16, u16), EdgeBits)> {
    let route = edge_route_points(from, to);
    let mut cells: BTreeMap<(u16, u16), EdgeBits> = BTreeMap::new();
    if route.len() == 1 {
        let (x, y) = route[0];
        merge_edge_bit(
            &mut cells,
            x,
            y,
            EdgeBits {
                up: true,
                down: true,
                left: false,
                right: false,
            },
        );
    } else {
        for pair in route.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            merge_edge_bit(&mut cells, a.0, a.1, direction(a, b));
            merge_edge_bit(&mut cells, b.0, b.1, direction(b, a));
        }
    }
    cells.into_iter().collect()
}

fn merge_edge_bit(cells: &mut BTreeMap<(u16, u16), EdgeBits>, x: u16, y: u16, bits: EdgeBits) {
    let entry = cells.entry((x, y)).or_default();
    *entry = entry.merged(bits);
}

/// Direction of the unit step from `from` to `to`. Callers only ever pass
/// orthogonal neighbours, so exactly one of the four bits is set.
fn direction(from: (u16, u16), to: (u16, u16)) -> EdgeBits {
    EdgeBits {
        up: to.1 < from.1,
        down: to.1 > from.1,
        left: to.0 < from.0,
        right: to.0 > from.0,
    }
}

/// Every terminal cell from just below `from`'s box to just above `to`'s box,
/// as a drop (vertical), jog (horizontal), drop (vertical) route centred on
/// each box — or a straight vertical run when the boxes already share a
/// column. Skip-layer edges are not routed around intermediate boxes; Phase 1
/// accepts that simplification (`04-kvdag-and-execution.md` §8: re-layout is
/// cheap, no incremental layout).
fn edge_route_points(from: LayoutRect, to: LayoutRect) -> Vec<(u16, u16)> {
    let start_x = from.x as i32 + from.width as i32 / 2;
    let start_y = from.bottom() as i32;
    let end_x = to.x as i32 + to.width as i32 / 2;
    let end_y = to.y as i32 - 1;

    if end_y < start_y {
        // Target is not below the source — malformed/cyclic input that
        // `Kvdag::try_new` would already have rejected. Draw nothing rather
        // than a nonsensical route.
        return Vec::new();
    }

    let turn_y = start_y + (end_y - start_y) / 2;
    let waypoints = if start_x == end_x {
        vec![(start_x, start_y), (start_x, end_y)]
    } else {
        vec![
            (start_x, start_y),
            (start_x, turn_y),
            (end_x, turn_y),
            (end_x, end_y),
        ]
    };

    let mut points: Vec<(i32, i32)> = Vec::new();
    for pair in waypoints.windows(2) {
        let segment = segment_points(pair[0], pair[1]);
        if points.last() == segment.first() {
            points.extend(segment.into_iter().skip(1));
        } else {
            points.extend(segment);
        }
    }

    points
        .into_iter()
        // Coordinates are non-negative by construction (guarded above); the
        // clamp is a cheap belt-and-suspenders against a future refactor.
        .map(|(x, y)| (x.max(0) as u16, y.max(0) as u16))
        .collect()
}

/// Every point on the straight (horizontal or vertical) segment from `a` to
/// `b`, inclusive, in travel order.
fn segment_points(a: (i32, i32), b: (i32, i32)) -> Vec<(i32, i32)> {
    if a.1 == b.1 {
        let (lo, hi) = (a.0.min(b.0), a.0.max(b.0));
        let mut xs: Vec<i32> = (lo..=hi).collect();
        if a.0 > b.0 {
            xs.reverse();
        }
        xs.into_iter().map(|x| (x, a.1)).collect()
    } else {
        let (lo, hi) = (a.1.min(b.1), a.1.max(b.1));
        let mut ys: Vec<i32> = (lo..=hi).collect();
        if a.1 > b.1 {
            ys.reverse();
        }
        ys.into_iter().map(|y| (a.0, y)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{
        EdgeKind, EdgePayload, GrowthLimits, KvdagVersionId, NodeKey, NodeStatus, NodeUsage,
        ProgressTracker, RunEdge, RunId, RunNode, RunStatus,
    };
    use crate::workflow::tier::{Assignment, Effort, ModelAlias, Tier};

    fn test_node(idx: usize, key: &str, path: &str, depth: u16) -> RunNode {
        RunNode {
            idx: RunNodeIdx(idx),
            key: NodeKey::new(key),
            path: InstancePath::new(path),
            label: String::new(),
            inputs: std::collections::BTreeMap::new(),
            parent: None,
            depth,
            status: NodeStatus::Pending,
            assignment: Assignment {
                model: ModelAlias::Sonnet,
                effort: Effort::Low,
            },
            assignment_reason: String::new(),
            attempt: 1,
            binding: None,
            result: None,
            usage: NodeUsage::default(),
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            progress: ProgressTracker::default(),
            succession: None,
            checkpoint_seq: 0,
        }
    }

    fn test_edge(from: usize, to: usize) -> RunEdge {
        RunEdge {
            from: RunNodeIdx(from),
            to: RunNodeIdx(to),
            kind: EdgeKind::Sequence,
            condition: None,
            payload: EdgePayload::default(),
            port: None,
            condition_result: None,
            fired: false,
        }
    }

    fn test_graph(nodes: Vec<RunNode>, edges: Vec<RunEdge>) -> RunGraph {
        RunGraph {
            run_id: RunId::new("run-1"),
            version_id: KvdagVersionId::new("v1"),
            tier: Tier::Auto,
            growth: GrowthLimits::default(),
            assignments: std::collections::BTreeMap::new(),
            nodes,
            edges,
            status: RunStatus::Running,
            seq: 0,
        }
    }

    /// `start` -> `{left, right}` -> `end`.
    fn diamond() -> RunGraph {
        test_graph(
            vec![
                test_node(0, "start", "start", 0),
                test_node(1, "left", "left", 1),
                test_node(2, "right", "right", 1),
                test_node(3, "end", "end", 2),
            ],
            vec![
                test_edge(0, 1),
                test_edge(0, 2),
                test_edge(1, 3),
                test_edge(2, 3),
            ],
        )
    }

    #[test]
    fn diamond_layer_assignment() {
        let graph = diamond();
        let layer_of = assign_layers(&graph);
        assert_eq!(layer_of[&RunNodeIdx(0)], 0);
        assert_eq!(layer_of[&RunNodeIdx(1)], 1);
        assert_eq!(layer_of[&RunNodeIdx(2)], 1);
        assert_eq!(layer_of[&RunNodeIdx(3)], 2);
    }

    #[test]
    fn diamond_layout_places_layers_top_to_bottom() {
        let graph = diamond();
        let dag = layout(&graph, LayoutRect::new(0, 0, 200, 50));

        let start = dag.rect_of(RunNodeIdx(0)).expect("start rect");
        let left = dag.rect_of(RunNodeIdx(1)).expect("left rect");
        let right = dag.rect_of(RunNodeIdx(2)).expect("right rect");
        let end = dag.rect_of(RunNodeIdx(3)).expect("end rect");

        assert!(start.y < left.y);
        assert_eq!(left.y, right.y);
        assert!(left.y < end.y);
        assert_ne!(left.x, right.x);
    }

    #[test]
    fn rects_never_overlap() {
        let graph = test_graph(
            vec![
                test_node(0, "root", "root", 0),
                test_node(1, "a", "root/a", 1),
                test_node(2, "b", "root/b", 1),
                test_node(3, "c", "root/c", 1),
                test_node(4, "join", "join", 2),
                test_node(5, "tail", "tail", 3),
            ],
            vec![
                test_edge(0, 1),
                test_edge(0, 2),
                test_edge(0, 3),
                test_edge(1, 4),
                test_edge(2, 4),
                test_edge(3, 4),
                test_edge(4, 5),
            ],
        );
        let dag = layout(&graph, LayoutRect::new(0, 0, 200, 50));

        for i in 0..dag.nodes.len() {
            for j in (i + 1)..dag.nodes.len() {
                assert!(!dag.nodes[i].1.intersects(&dag.nodes[j].1));
            }
        }
    }

    #[test]
    fn layout_is_deterministic_for_the_same_graph() {
        let graph = diamond();
        let area = LayoutRect::new(1, 2, 100, 40);
        assert_eq!(layout(&graph, area), layout(&graph, area));
    }

    #[test]
    fn crossing_reduction_is_stable_under_node_insertion() {
        let area = LayoutRect::new(0, 0, 200, 50);
        let before = layout(
            &test_graph(
                vec![
                    test_node(0, "root", "root", 0),
                    test_node(1, "b", "root/b", 1),
                    test_node(2, "c", "root/c", 1),
                    test_node(3, "d", "root/d", 1),
                ],
                vec![test_edge(0, 1), test_edge(0, 2), test_edge(0, 3)],
            ),
            area,
        );
        let b_before = before.rect_of(RunNodeIdx(1)).expect("b rect").x;
        let c_before = before.rect_of(RunNodeIdx(2)).expect("c rect").x;
        let d_before = before.rect_of(RunNodeIdx(3)).expect("d rect").x;
        assert!(b_before < c_before && c_before < d_before);

        // A new sibling ("root/a" sorts first among instance paths) makes
        // room for itself without reordering the nodes that were already
        // there — the stability `04-kvdag-and-execution.md` §8 asks for.
        let after = layout(
            &test_graph(
                vec![
                    test_node(0, "root", "root", 0),
                    test_node(1, "b", "root/b", 1),
                    test_node(2, "c", "root/c", 1),
                    test_node(3, "d", "root/d", 1),
                    test_node(4, "a", "root/a", 1),
                ],
                vec![
                    test_edge(0, 1),
                    test_edge(0, 2),
                    test_edge(0, 3),
                    test_edge(0, 4),
                ],
            ),
            area,
        );
        let a_after = after.rect_of(RunNodeIdx(4)).expect("a rect").x;
        let b_after = after.rect_of(RunNodeIdx(1)).expect("b rect").x;
        let c_after = after.rect_of(RunNodeIdx(2)).expect("c rect").x;
        let d_after = after.rect_of(RunNodeIdx(3)).expect("d rect").x;
        assert!(a_after < b_after && b_after < c_after && c_after < d_after);
    }

    #[test]
    fn empty_graph_yields_empty_layout() {
        let graph = test_graph(Vec::new(), Vec::new());
        assert_eq!(
            layout(&graph, LayoutRect::new(0, 0, 80, 24)),
            DagLayout::default()
        );
    }

    #[test]
    fn rect_contains_its_own_cells_only() {
        let rect = LayoutRect::new(2, 3, 4, 2);
        assert!(rect.contains(2, 3));
        assert!(rect.contains(5, 4));
        assert!(!rect.contains(6, 4));
        assert!(!rect.contains(5, 5));
        assert!(!LayoutRect::new(2, 3, 0, 2).contains(2, 3));
    }

    #[test]
    fn rects_intersect_only_when_they_overlap() {
        let rect = LayoutRect::new(0, 0, 4, 2);
        assert!(rect.intersects(&LayoutRect::new(3, 1, 4, 2)));
        assert!(!rect.intersects(&LayoutRect::new(4, 0, 4, 2)));
        assert!(!rect.intersects(&LayoutRect::new(0, 2, 4, 2)));
    }

    #[test]
    fn edge_bits_merge_by_direction() {
        let down = EdgeBits {
            down: true,
            ..EdgeBits::default()
        };
        let right = EdgeBits {
            right: true,
            ..EdgeBits::default()
        };
        let merged = down.merged(right);
        assert!(merged.down && merged.right);
        assert!(!merged.up && !merged.left);
        assert!(EdgeBits::default().is_empty());
        assert!(!merged.is_empty());
    }

    #[test]
    fn hit_test_reads_the_stored_rects() {
        let layout = DagLayout {
            nodes: vec![
                (RunNodeIdx(0), LayoutRect::new(0, 0, 10, 3)),
                (RunNodeIdx(1), LayoutRect::new(0, 4, 10, 3)),
            ],
            edges: Vec::new(),
        };
        assert_eq!(layout.node_at(1, 1), Some(RunNodeIdx(0)));
        assert_eq!(layout.node_at(1, 5), Some(RunNodeIdx(1)));
        assert_eq!(layout.node_at(1, 3), None);
        assert_eq!(
            layout.rect_of(RunNodeIdx(1)),
            Some(LayoutRect::new(0, 4, 10, 3))
        );
    }

    #[test]
    fn every_routed_edge_names_the_boxes_it_joins() {
        let dag = layout(&diamond(), LayoutRect::new(0, 0, 200, 50));
        assert_eq!(dag.edges.len(), 4);
        for route in &dag.edges {
            assert!(dag.rect_of(route.from).is_some());
            assert!(dag.rect_of(route.to).is_some());
            assert!(!route.cells.is_empty());
        }
        // The derived cell map is exactly the union of the per-edge routes.
        let merged = dag.edge_cells();
        for route in &dag.edges {
            for (position, _) in &route.cells {
                assert!(merged.contains_key(position), "{position:?}");
            }
        }
    }

    #[test]
    fn dropping_a_box_drops_the_edges_that_pointed_at_it() {
        let mut dag = layout(&diamond(), LayoutRect::new(0, 0, 200, 50));
        let bounds = LayoutRect::new(0, 0, 200, 50);
        assert!(dag
            .edges
            .iter()
            .any(|route| route.to == RunNodeIdx(3) || route.from == RunNodeIdx(3)));

        // Clip `end` away, exactly as a short graph band would.
        dag.nodes.retain(|(idx, _)| *idx != RunNodeIdx(3));
        dag.retain_visible_edges(bounds);

        assert!(
            dag.edges
                .iter()
                .all(|route| route.from != RunNodeIdx(3) && route.to != RunNodeIdx(3)),
            "an edge survived its own endpoint: {:?}",
            dag.edges
        );
        // And no cell of a dropped edge is left behind in the merged map:
        // `end` sat in the third band, so nothing below the first gap remains.
        let merged = dag.edge_cells();
        assert!(merged.keys().all(|(_, y)| *y < 8), "{merged:?}");
    }

    #[test]
    fn edge_cells_outside_the_bounds_are_dropped() {
        let mut dag = layout(&diamond(), LayoutRect::new(0, 0, 200, 50));
        // A band that stops above the first gap keeps no edge cells at all.
        dag.retain_visible_edges(LayoutRect::new(0, 0, 200, 3));
        assert!(dag.edge_cells().is_empty(), "{:?}", dag.edges);
    }
}
