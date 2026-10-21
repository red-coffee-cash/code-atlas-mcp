//! Tarjan SCC condensation and topological levels (spec §5.1).

use std::collections::HashMap;

use anyhow::Result;
use cozo::DbInstance;
use petgraph::algo::tarjan_scc;
use petgraph::graph::DiGraph;

use crate::db::{mutations, queries};

#[derive(Debug, PartialEq, Eq)]
pub struct Topology {
    /// symbol_id -> (scc_group_id, topological_level)
    pub by_symbol: HashMap<String, (i64, i64)>,
    pub scc_count: usize,
    pub cyclic_scc_count: usize,
    pub max_level: i64,
}

/// `edges` are caller -> callee. Level 0 means no callee outside the symbol's own SCC, so
/// mapping can start there and callers always come after everything they depend on.
pub fn compute(symbols: &[String], edges: &[(String, String)]) -> Topology {
    let mut ids: Vec<&String> = symbols.iter().collect();
    ids.sort();
    ids.dedup();
    let pos: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

    let mut graph = DiGraph::<(), ()>::with_capacity(ids.len(), edges.len());
    let nodes: Vec<_> = ids.iter().map(|_| graph.add_node(())).collect();
    let mut sorted: Vec<(usize, usize)> =
        edges.iter().filter_map(|(f, t)| Some((*pos.get(f.as_str())?, *pos.get(t.as_str())?))).collect();
    sorted.sort_unstable();
    sorted.dedup();
    for &(f, t) in &sorted {
        graph.add_edge(nodes[f], nodes[t], ());
    }

    // For an edge u -> v across SCCs, v's SCC is emitted before u's, so one pass suffices.
    let sccs = tarjan_scc(&graph);
    let mut group_of = vec![0usize; ids.len()];
    for (g, comp) in sccs.iter().enumerate() {
        for n in comp {
            group_of[n.index()] = g;
        }
    }
    let mut level = vec![0i64; sccs.len()];
    for (g, comp) in sccs.iter().enumerate() {
        let mut lvl = 0;
        for n in comp {
            for callee in graph.neighbors(*n) {
                let cg = group_of[callee.index()];
                if cg != g {
                    lvl = lvl.max(level[cg] + 1);
                }
            }
        }
        level[g] = lvl;
    }

    let by_symbol =
        ids.iter().enumerate().map(|(i, s)| ((*s).clone(), (group_of[i] as i64, level[group_of[i]]))).collect();
    let cyclic = sccs.iter().filter(|c| c.len() > 1 || graph.contains_edge(c[0], c[0])).count();
    Topology {
        by_symbol,
        scc_count: sccs.len(),
        cyclic_scc_count: cyclic,
        max_level: level.iter().copied().max().unwrap_or(0),
    }
}

/// Recomputes and persists topology from the stored CALLS edges.
pub fn recompute(db: &DbInstance) -> Result<Topology> {
    let symbols: Vec<String> = queries::symbol_ids(db)?.into_iter().collect();
    let edges = queries::call_edges(db)?;
    let topo = compute(&symbols, &edges);
    mutations::write_topology(db, topo.by_symbol.iter().map(|(id, (g, l))| (id.clone(), *g, *l)).collect())?;
    Ok(topo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn e(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn chain_levels_run_leaf_to_root() {
        let t = compute(&s(&["root", "mid", "leaf"]), &e(&[("root", "mid"), ("mid", "leaf")]));
        assert_eq!(t.by_symbol["leaf"].1, 0);
        assert_eq!(t.by_symbol["mid"].1, 1);
        assert_eq!(t.by_symbol["root"].1, 2);
        assert_eq!(t.max_level, 2);
        assert_eq!(t.cyclic_scc_count, 0);
    }

    #[test]
    fn mutual_recursion_shares_a_group_and_level() {
        let t = compute(&s(&["a", "b", "c", "top"]), &e(&[("a", "b"), ("b", "a"), ("b", "c"), ("top", "a")]));
        assert_eq!(t.by_symbol["a"], t.by_symbol["b"]);
        assert_ne!(t.by_symbol["a"].0, t.by_symbol["c"].0);
        assert_eq!(t.by_symbol["c"].1, 0);
        assert_eq!(t.by_symbol["a"].1, 1);
        assert_eq!(t.by_symbol["top"].1, 2);
        assert_eq!(t.cyclic_scc_count, 1);
    }

    #[test]
    fn self_recursion_is_cyclic_but_stays_a_leaf() {
        let t = compute(&s(&["f"]), &e(&[("f", "f")]));
        assert_eq!(t.by_symbol["f"].1, 0);
        assert_eq!(t.cyclic_scc_count, 1);
    }

    #[test]
    fn diamond_takes_the_longest_path_and_output_is_deterministic() {
        let syms = s(&["a", "b", "c", "d", "e"]);
        let edges = e(&[("a", "b"), ("a", "c"), ("b", "d"), ("c", "e"), ("e", "d")]);
        let t = compute(&syms, &edges);
        assert_eq!(t.by_symbol["d"].1, 0);
        assert_eq!(t.by_symbol["a"].1, 3);
        assert_eq!(t, compute(&syms, &edges));
    }

    #[test]
    fn edges_to_unknown_symbols_are_ignored() {
        let t = compute(&s(&["a"]), &e(&[("a", "ghost")]));
        assert_eq!(t.by_symbol["a"].1, 0);
    }
}
