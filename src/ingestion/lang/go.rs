use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend};
use crate::ingestion::model::ImportDecl;
use crate::ingestion::walker::SourceFile;

pub struct GoFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static GO: GoFrontend = GoFrontend { compiled: OnceLock::new() };

// Scaffold: every hook is filled in by the go slice.
impl Frontend for GoFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        let _ = &self.compiled;
        anyhow::bail!("go frontend not implemented")
    }

    fn symbol_kind(&self, _def: Node, _src: &[u8]) -> Option<&'static str> {
        None
    }

    fn scope_chain(&self, _def: Node, _src: &[u8]) -> Vec<String> {
        Vec::new()
    }

    fn param_types(&self, _def: Node, _src: &[u8]) -> Vec<String> {
        Vec::new()
    }

    fn visibility(&self, _def: Node, _src: &[u8]) -> &'static str {
        "pub"
    }

    fn doc_comment(&self, _def: Node, _src: &[u8]) -> String {
        String::new()
    }

    fn is_entrypoint(&self, _def: Node, _src: &[u8]) -> bool {
        false
    }

    fn is_branch(&self, _node: Node, _src: &[u8]) -> bool {
        false
    }

    fn is_nested_def(&self, _node: Node) -> bool {
        false
    }

    fn flatten_import(&self, _node: Node, _src: &[u8], _out: &mut Vec<ImportDecl>) {}
}
