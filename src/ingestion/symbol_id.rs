//! Deterministic symbol IDs (spec §3.1):
//! `{relative_file_path}::{scope_chain}::{symbol_name}#{kind}:{sig_hash}`

/// First 4 hex characters of the blake3 hash of the normalized parameter list.
/// Arity is part of the hash so `f(a)` and `f(a, b)` never collide even when types are elided.
pub fn sig_hash(param_types: &[&str]) -> String {
    let normalized: Vec<String> =
        param_types.iter().map(|t| t.chars().filter(|c| !c.is_whitespace()).collect()).collect();
    let input = format!("{}({})", normalized.len(), normalized.join(","));
    blake3::hash(input.as_bytes()).to_hex()[..4].to_string()
}

/// Scope-chain segment for `impl Trait for Type` blocks.
pub fn trait_impl_scope(trait_name: &str, type_name: &str) -> String {
    format!("{trait_name}_for_{type_name}")
}

/// An empty `scope_chain` (top-level symbol) yields `path::::name`, as in the spec's Python example.
pub fn build(relative_file_path: &str, scope_chain: &[&str], name: &str, kind: &str, param_types: &[&str]) -> String {
    format!("{relative_file_path}::{}::{name}#{kind}:{}", scope_chain.join("::"), sig_hash(param_types))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_symbol_has_empty_scope_segment() {
        let id = build("app/billing/stripe.py", &[], "charge_card", "fn", &["Card"]);
        assert!(id.starts_with("app/billing/stripe.py::::charge_card#fn:"), "{id}");
    }

    #[test]
    fn trait_impl_method_matches_spec_shape() {
        let scope = trait_impl_scope("TokenVerifier", "JwtService");
        let id = build("src/auth/jwt.rs", &[&scope], "verify", "method", &["&self", "&str"]);
        assert!(id.starts_with("src/auth/jwt.rs::TokenVerifier_for_JwtService::verify#method:"), "{id}");
    }

    #[test]
    fn nested_scopes_join_with_double_colon() {
        let id = build("a.rs", &["outer", "Inner"], "f", "fn", &[]);
        assert!(id.starts_with("a.rs::outer::Inner::f#fn:"), "{id}");
    }

    #[test]
    fn hash_is_four_hex_chars_and_deterministic() {
        let h = sig_hash(&["i32", "String"]);
        assert_eq!(h.len(), 4);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, sig_hash(&["i32", "String"]));
    }

    #[test]
    fn overloads_do_not_collide() {
        assert_ne!(sig_hash(&["int"]), sig_hash(&["String"]));
        assert_ne!(sig_hash(&["int"]), sig_hash(&["int", "int"]));
        assert_ne!(sig_hash(&[]), sig_hash(&[""]));
    }

    #[test]
    fn whitespace_in_types_is_normalized() {
        assert_eq!(sig_hash(&["Vec<u8, A>"]), sig_hash(&["Vec<u8,A>"]));
    }
}
