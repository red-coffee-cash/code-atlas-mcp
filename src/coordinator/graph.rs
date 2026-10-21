//! In-memory view of the live mental-model graph for audit-time analysis.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use cozo::DbInstance;

use crate::db::queries::{self, Contract, StoredEdge, SymbolInfo};

pub struct AtlasGraph {
    pub symbols: HashMap<String, SymbolInfo>,
    pub contracts: HashMap<String, Contract>,
    /// Edges excluding LLM_PRUNED and excluding endpoints that no longer exist.
    pub edges: Vec<StoredEdge>,
    pub out: HashMap<String, Vec<usize>>,
    pub inn: HashMap<String, Vec<usize>>,
}

impl AtlasGraph {
    pub fn load(db: &DbInstance) -> Result<Self> {
        let symbols: HashMap<String, SymbolInfo> =
            queries::all_symbols(db)?.into_iter().map(|s| (s.symbol_id.clone(), s)).collect();
        let contracts = queries::all_contracts(db)?;
        let mut edges: Vec<StoredEdge> = queries::stored_edges(db)?
            .into_iter()
            .filter(|e| e.confidence != "LLM_PRUNED" && symbols.contains_key(&e.from) && symbols.contains_key(&e.to))
            .collect();
        // Stable adjacency order keeps every traversal deterministic.
        edges.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
        let mut out: HashMap<String, Vec<usize>> = HashMap::new();
        let mut inn: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            out.entry(e.from.clone()).or_default().push(i);
            inn.entry(e.to.clone()).or_default().push(i);
        }
        Ok(Self { symbols, contracts, edges, out, inn })
    }

    pub fn risk(&self, id: &str) -> i64 {
        self.contracts.get(id).map_or(0, |c| c.risk_score)
    }

    pub fn is_trust_boundary(&self, id: &str) -> bool {
        self.contracts.get(id).is_some_and(|c| c.trust_boundary)
    }

    /// A sink is anything whose contract records a sensitive operation or a state mutation.
    pub fn is_sink(&self, id: &str) -> bool {
        self.contracts.get(id).is_some_and(|c| !c.taint_sinks.is_empty() || !c.state_mutations.is_empty())
    }

    pub fn call_in_degree(&self, id: &str) -> usize {
        self.inn.get(id).map_or(0, |v| v.iter().filter(|&&i| self.edges[i].kind == "CALLS").count())
    }

    /// Symbols reachable from any entrypoint over CALLS edges (entrypoints included).
    pub fn reachable_from_entrypoints(&self) -> HashSet<String> {
        let mut seen: HashSet<String> =
            self.symbols.values().filter(|s| s.is_entrypoint).map(|s| s.symbol_id.clone()).collect();
        let mut queue: VecDeque<String> = seen.iter().cloned().collect();
        while let Some(n) = queue.pop_front() {
            for &i in self.out.get(&n).into_iter().flatten() {
                let e = &self.edges[i];
                if e.kind == "CALLS" && seen.insert(e.to.clone()) {
                    queue.push_back(e.to.clone());
                }
            }
        }
        seen
    }
}
