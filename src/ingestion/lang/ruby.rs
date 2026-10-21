use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, text};
use crate::ingestion::model::{ImplRel, ImportDecl};
use crate::ingestion::walker::SourceFile;

pub struct RubyFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static RUBY: RubyFrontend = RubyFrontend { compiled: OnceLock::new() };

const CONTROLLER_BASES: &[&str] = &["ApplicationController", "ActionController::Base", "ActionController::API"];
const JOB_BASES: &[&str] = &["ApplicationJob", "ActiveJob::Base", "Sidekiq::Worker", "Sidekiq::Job"];
/// Comments that configure the interpreter or a type checker rather than document the code.
const MAGIC_COMMENTS: &[&str] = &["frozen_string_literal:", "typed:", "encoding:", "coding:", "-*-", "warn_indent:"];

impl Frontend for RubyFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                Compiled::build(
                    tree_sitter_ruby::LANGUAGE.into(),
                    include_str!("../../../queries/ruby/symbols.scm"),
                    include_str!("../../../queries/ruby/calls.scm"),
                    include_str!("../../../queries/ruby/risks.scm"),
                    "ruby",
                )
                .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, _src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "singleton_method" => "method",
            "method" if enclosing_type(def).is_some() => "method",
            "method" => "fn",
            "class" | "module" => "class",
            "assignment" => "const",
            _ => return None,
        })
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = def.parent();
        while let Some(n) = cur {
            if matches!(n.kind(), "class" | "module")
                && let Some(name) = n.child_by_field_name("name")
            {
                chain.push(text(name, src).to_string());
            }
            cur = n.parent();
        }
        chain.reverse();
        // `class Foo::Bar` is the symbol `Bar` scoped under `Foo`.
        if matches!(def.kind(), "class" | "module")
            && let Some(scope) = def.child_by_field_name("name").and_then(|n| n.child_by_field_name("scope"))
        {
            chain.push(text(scope, src).to_string());
        }
        chain
    }

    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String> {
        let Some(params) = def.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = params.walk();
        params
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .map(|p| match p.kind() {
                "splat_parameter" => "*".to_string(),
                "hash_splat_parameter" => "**".to_string(),
                "block_parameter" => "&".to_string(),
                "forward_parameter" => "...".to_string(),
                "keyword_parameter" => {
                    format!("kw:{}", p.child_by_field_name("name").map_or("", |n| text(n, src)))
                }
                _ => "_".to_string(),
            })
            .collect()
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        if !matches!(def.kind(), "method" | "singleton_method") {
            return "pub";
        }
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        // `private def x` / `private_class_method def self.x`
        let wrapper = def
            .parent()
            .filter(|p| p.kind() == "argument_list")
            .and_then(|a| a.parent())
            .filter(|c| c.kind() == "call" && c.child_by_field_name("receiver").is_none());
        if let Some(call) = wrapper
            && let Some(level) = call.child_by_field_name("method").and_then(|m| keyword_level(text(m, src)))
        {
            return level;
        }
        let anchor = wrapper.unwrap_or(def);
        let Some(scope) = anchor.parent() else {
            return "pub";
        };

        // `private :a, :b` / `private_class_method :x`, anywhere in the same body
        let mut cursor = scope.walk();
        for sib in scope.named_children(&mut cursor) {
            if sib.kind() != "call" || sib.child_by_field_name("receiver").is_some() {
                continue;
            }
            let Some(level) = sib.child_by_field_name("method").and_then(|m| keyword_level(text(m, src))) else {
                continue;
            };
            let Some(args) = sib.child_by_field_name("arguments") else {
                continue;
            };
            let mut ac = args.walk();
            let named = args.named_children(&mut ac).any(|a| {
                matches!(a.kind(), "simple_symbol" | "string")
                    && text(a, src).trim_start_matches(':').trim_matches(|c| c == '"' || c == '\'') == name
            });
            let class_method = text(sib.child_by_field_name("method").unwrap(), src).ends_with("_class_method");
            if named && class_method == (def.kind() == "singleton_method") {
                return level;
            }
        }

        // A bare `private` applies to the instance methods that follow it, not to `def self.x`.
        if def.kind() == "method" {
            let mut prev = anchor.prev_sibling();
            while let Some(p) = prev {
                if p.kind() == "identifier"
                    && let Some(level) = keyword_level(text(p, src))
                {
                    return level;
                }
                prev = p.prev_sibling();
            }
        }
        "pub"
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let anchor = def
            .parent()
            .filter(|p| p.kind() == "argument_list")
            .and_then(|a| a.parent())
            .filter(|c| c.kind() == "call")
            .unwrap_or(def);
        let mut lines = Vec::new();
        let mut row = anchor.start_position().row;
        let mut prev = anchor.prev_sibling();
        while let Some(c) = prev.filter(|c| c.kind() == "comment" && c.end_position().row + 1 >= row) {
            let raw = text(c, src);
            let body = raw.trim_start_matches('#').trim();
            if !MAGIC_COMMENTS.iter().any(|m| body.starts_with(m)) {
                let stripped = raw.trim_start_matches('#');
                lines.push(stripped.strip_prefix(' ').unwrap_or(stripped).trim_end().to_string());
            }
            row = c.start_position().row;
            prev = c.prev_sibling();
        }
        lines.reverse();
        lines.join("\n")
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        match def.kind() {
            "method" => match enclosing_type(def) {
                None => name == "main" && def.parent().is_some_and(|p| p.kind() == "program"),
                Some(class) if class.kind() == "class" => {
                    if self.visibility(def, src) != "pub" || name == "initialize" {
                        return false;
                    }
                    let base = superclass(class, src);
                    let last = class_name(class, src).rsplit("::").next().unwrap_or("").to_string();
                    if name == "perform" {
                        return JOB_BASES.contains(&base.as_str()) || includes_sidekiq(class, src);
                    }
                    last.ends_with("Controller") || CONTROLLER_BASES.contains(&base.as_str()) || base == "Thor"
                }
                Some(_) => false,
            },
            _ => false,
        }
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        // keyword tokens (`if`, `when`, `rescue`) are anonymous nodes named like the constructs
        if !node.is_named() {
            return false;
        }
        match node.kind() {
            "if" | "elsif" | "unless" | "while" | "until" | "for" | "when" | "rescue" | "conditional"
            | "if_modifier" | "unless_modifier" | "while_modifier" | "until_modifier" | "rescue_modifier"
            | "in_clause" => true,
            "binary" => node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(text(op, src), "&&" | "||" | "and" | "or")),
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        matches!(node.kind(), "method" | "singleton_method" | "class" | "module")
    }

    fn implements(&self, root: Node, src: &[u8]) -> Vec<ImplRel> {
        let mut out = Vec::new();
        crate::ingestion::parser::walk(root, |n| {
            if !matches!(n.kind(), "class" | "module") {
                return true;
            }
            let Some(name) = n.child_by_field_name("name") else {
                return true;
            };
            let type_name = last_segment(text(name, src)).to_string();
            let line = n.start_position().row + 1;
            if let Some(sup) = n.child_by_field_name("superclass").and_then(|s| s.named_child(0))
                && matches!(sup.kind(), "constant" | "scope_resolution")
            {
                out.push(ImplRel {
                    type_name: type_name.clone(),
                    trait_name: last_segment(text(sup, src)).to_string(),
                    line,
                });
            }
            if let Some(body) = n.child_by_field_name("body") {
                let mut cursor = body.walk();
                for call in body.named_children(&mut cursor).filter(|c| c.kind() == "call") {
                    let method = call.child_by_field_name("method").map_or("", |m| text(m, src));
                    if call.child_by_field_name("receiver").is_some()
                        || !matches!(method, "include" | "extend" | "prepend")
                    {
                        continue;
                    }
                    let Some(args) = call.child_by_field_name("arguments") else {
                        continue;
                    };
                    let mut ac = args.walk();
                    for a in
                        args.named_children(&mut ac).filter(|a| matches!(a.kind(), "constant" | "scope_resolution"))
                    {
                        out.push(ImplRel {
                            type_name: type_name.clone(),
                            trait_name: last_segment(text(a, src)).to_string(),
                            line: call.start_position().row + 1,
                        });
                    }
                }
            }
            true
        });
        out
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        if node.kind() != "call" {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let mut cursor = args.walk();
        let Some(path) = args.named_children(&mut cursor).find(|a| a.kind() == "string").and_then(|s| literal(s, src))
        else {
            return;
        };
        let path = path.strip_suffix(".rb").unwrap_or(&path);
        let (module_path, name) = match path.rsplit_once('/') {
            Some((m, n)) => (m.to_string(), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        out.push(ImportDecl { module_path, name, alias: None });
    }
}

fn keyword_level(name: &str) -> Option<&'static str> {
    match name {
        "private" | "protected" | "private_class_method" => Some("private"),
        "public" | "public_class_method" => Some("pub"),
        _ => None,
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// The nearest enclosing `class`/`module`, looking through wrapper calls and `class << self`.
fn enclosing_type(def: Node) -> Option<Node> {
    let mut cur = def.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class" | "module") {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

fn class_name<'a>(class: Node, src: &'a [u8]) -> &'a str {
    class.child_by_field_name("name").map_or("", |n| text(n, src))
}

fn superclass(class: Node, src: &[u8]) -> String {
    class
        .child_by_field_name("superclass")
        .and_then(|s| s.named_child(0))
        .map_or(String::new(), |s| text(s, src).to_string())
}

fn includes_sidekiq(class: Node, src: &[u8]) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor).any(|c| {
        c.kind() == "call"
            && c.child_by_field_name("method").is_some_and(|m| text(m, src) == "include")
            && text(c, src).contains("Sidekiq::")
    })
}

/// Contents of a string literal without interpolation.
fn literal(s: Node, src: &[u8]) -> Option<String> {
    let mut cursor = s.walk();
    let mut out = String::new();
    for c in s.named_children(&mut cursor) {
        if c.kind() != "string_content" {
            return None;
        }
        out.push_str(text(c, src));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use crate::ingestion::model::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parse_at(path: &str, src: &str) -> ParsedFile {
        let f = SourceFile {
            rel_path: path.into(),
            language: Language::Ruby,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        parse_file(&f).expect("ruby queries must compile").unwrap()
    }

    fn parse(src: &str) -> ParsedFile {
        parse_at("app/models/account.rb", src)
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {:?}", p.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    #[test]
    fn symbols_kinds_ids_docs_and_scopes() {
        let p = parse(
            r#"# frozen_string_literal: true

MAX = 3
counter = 0

# Billing core.
# Second line.
module Billing
  RATE = 2

  class Account::Ledger < Base
    LIMIT = 10
    @state = 1

    # Posts an entry.
    def post(entry, amount = 1, *rest, key:, opt: 2, **kw, &blk)
      @x = 1
    end

    def self.build; end
    def build; end
  end
end

def helper(x); end
"#,
        );
        let billing = sym(&p, "Billing");
        assert_eq!(billing.kind, "class");
        assert_eq!(
            billing.doc_comment,
            "Billing core.
Second line."
        );
        assert_eq!(sym(&p, "MAX").kind, "const");
        assert_eq!(sym(&p, "RATE").qualified_name, "Billing::RATE");
        assert!(!p.symbols.iter().any(|s| s.name == "counter" || s.name == "@state"));

        let ledger = sym(&p, "Ledger");
        assert_eq!((ledger.kind, ledger.qualified_name.as_str()), ("class", "Billing::Account::Ledger"));
        assert_eq!(sym(&p, "LIMIT").qualified_name, "Billing::Account::Ledger::LIMIT");

        let post = sym(&p, "post");
        assert_eq!((post.kind, post.visibility), ("method", "pub"));
        assert!(post.symbol_id.contains("::Billing::Account::Ledger::post#method:"), "{}", post.symbol_id);
        assert_eq!(post.doc_comment, "Posts an entry.");
        assert_eq!(post.signature, "def post(entry, amount = 1, *rest, key:, opt: 2, **kw, &blk)");
        assert_eq!(sym(&p, "helper").kind, "fn");

        let builds: Vec<_> = p.symbols.iter().filter(|s| s.name == "build").collect();
        assert_eq!(builds.len(), 2);
        assert_ne!(builds[0].symbol_id, builds[1].symbol_id, "def self.build vs build must not collide");
    }

    #[test]
    fn param_shapes_feed_the_signature_hash() {
        let a = parse(
            "def f(a, b = 1, *r, k:, **o, &blk); end
",
        );
        let b = parse(
            "def f(a, b = 2, *r, k:, **o, &blk); end
",
        );
        let c = parse(
            "def f(a, *r, j:, **o, &blk); end
",
        );
        assert_eq!(sym(&a, "f").symbol_id, sym(&b, "f").symbol_id);
        assert_ne!(sym(&a, "f").symbol_id, sym(&c, "f").symbol_id);
    }

    #[test]
    fn visibility_keywords_scope_correctly() {
        let p = parse(
            r#"class Svc
  def open_one; end

  def self.klass_pub; end

  private

  def hidden_a; end
  def self.still_public; end

  protected

  def guarded; end

  public

  def open_two; end

  private def wrapped; end
  def after_wrapper; end

  def explicit_one; end
  def explicit_two; end
  private :explicit_one, :explicit_two

  def self.secret; end
  private_class_method :secret
  private_class_method def self.secret_wrapped; end
end
"#,
        );
        let vis = |n: &str| sym(&p, n).visibility;
        for n in ["open_one", "klass_pub", "still_public", "open_two", "after_wrapper"] {
            assert_eq!(vis(n), "pub", "{n}");
        }
        for n in ["hidden_a", "guarded", "wrapped", "explicit_one", "explicit_two", "secret", "secret_wrapped"] {
            assert_eq!(vis(n), "private", "{n}");
        }
    }

    #[test]
    fn complexity_counts_ruby_branches_and_skips_nested_defs() {
        let p = parse(
            r#"def f(x, y)
  def inner(z)
    z if z
  end
  if x && y
    1
  elsif x or y
    2
  end
  puts "a" unless x
  for i in x do end
  while y do break end
  case x
  when 1 then 1
  when 2 then 2
  end
  r = x ? 1 : 2
  begin
    1
  rescue ArgumentError
    2
  end
end
"#,
        );
        // if, &&, elsif, or, unless-modifier, for, while, 2 whens, ternary, rescue
        assert_eq!(sym(&p, "f").cyclomatic_complexity, 12);
        assert_eq!(sym(&p, "inner").cyclomatic_complexity, 2);
    }

    #[test]
    fn risk_flags_have_positive_and_negative_cases() {
        let p = parse(
            r#"def a(c); system(c); end
def b(c); `ls #{c}`; end
def c(x); Open3.capture2(x); end
def d(m); send(m); end
def d2; send(:known); end
def e(s); Marshal.load(s); end
def f(s); YAML.load(s); end
def f2(s); YAML.safe_load(s); end
def g(n); User.where("name = '#{n}'"); end
def g2(n); User.where("name = ?", n); end
def g3(n); User.where(name: n); end
def h(s); raw(s); end
def h2(s); s.html_safe; end
def i(p); File.read(p); end
def i2(p); File.join(p, "x"); end
def j; Thread.new { 1 }; end
def k; begin; x; rescue; end; end
def k2; x rescue nil; end
def k3; begin; x; rescue Exception => e; log(e); end; end
def k4; begin; x; rescue ArgumentError => e; log(e); end; end
def l(v); v.instance_eval("1"); end
def m(c); c.constantize; end
def plain(a, b); a + b; end
"#,
        );
        let flags = |n: &str| sym(&p, n).risk_flags.clone();
        for n in ["a", "b", "c", "d", "l", "m"] {
            assert!(flags(n).contains(&"command_exec".to_string()), "{n}: {:?}", flags(n));
        }
        assert!(flags("d2").is_empty(), "{:?}", flags("d2"));
        assert_eq!(flags("e"), ["unsafe_deserialization"]);
        assert_eq!(flags("f"), ["unsafe_deserialization"]);
        assert!(flags("f2").is_empty(), "{:?}", flags("f2"));
        assert_eq!(flags("g"), ["raw_sql"]);
        assert!(flags("g2").is_empty() && flags("g3").is_empty());
        assert_eq!(flags("h"), ["xss_sink"]);
        assert_eq!(flags("h2"), ["xss_sink"]);
        assert_eq!(flags("i"), ["external_io"]);
        assert!(flags("i2").is_empty(), "{:?}", flags("i2"));
        assert_eq!(flags("j"), ["concurrency_primitive"]);
        for n in ["k", "k2", "k3"] {
            assert_eq!(flags(n), ["error_swallow"], "{n}");
        }
        assert!(flags("k4").is_empty(), "{:?}", flags("k4"));
        assert!(flags("plain").is_empty());
        assert!(sym(&p, "a").requires_strict_contract);
    }

    #[test]
    fn entrypoints() {
        let p = parse_at(
            "app/controllers/accounts_controller.rb",
            r#"class AccountsController < ApplicationController
  def index; end
  def show; end

  private

  def set_account; end
end

class Report
  def run; end
end

class SyncJob < ApplicationJob
  def perform(id); end
  def helper; end
end

class Legacy
  include Sidekiq::Worker
  def perform; end
end

class Cli < Thor
  def deploy; end
end

def main; end
"#,
        );
        for n in ["index", "show", "perform", "deploy", "main"] {
            assert!(p.symbols.iter().filter(|s| s.name == n).all(|s| s.is_entrypoint), "{n}");
        }
        assert!(p.symbols.iter().filter(|s| s.name == "perform").count() == 2);
        for n in ["set_account", "run", "helper"] {
            assert!(!sym(&p, n).is_entrypoint, "{n}");
        }
    }

    #[test]
    fn calls_symbol_callbacks_and_imports() {
        let p = parse(
            r#"require 'json'
require 'net/http'
require_relative '../lib/helper'
load 'tasks/seed.rb'
autoload :Foo, 'foo/bar'
require "dyn/#{name}"

class Ctl < Base
  before_action :authorize
  def go(items)
    helper_call(1)
    items.map(&:name)
    Other.build(items)
    send(:authorize)
    bare_local
    items.each { |i| i.save }
  end
  def authorize; end
end
"#,
        );
        let go = sym(&p, "go").symbol_id.clone();
        let find = |name: &str| p.calls.iter().find(|c| c.from_symbol_id == go && c.target_name == name);
        assert_eq!(find("helper_call").unwrap().kind, CallKind::Direct);
        assert_eq!(find("helper_call").unwrap().receiver, None);
        assert_eq!(find("build").unwrap().receiver.as_deref(), Some("Other"));
        assert_eq!(find("save").unwrap().receiver.as_deref(), Some("i"));
        assert!(find("bare_local").is_none(), "paren-less bare identifiers are not call sites");
        let sym_refs: Vec<_> = p.calls.iter().filter(|c| c.kind == CallKind::SymbolRef).collect();
        assert!(sym_refs.iter().any(|c| c.target_name == "authorize" && c.via.as_deref() == Some("send")));
        assert!(sym_refs.iter().any(|c| c.target_name == "authorize" && c.via.as_deref() == Some("before_action")));
        assert!(sym_refs.iter().any(|c| c.target_name == "name" && c.via.as_deref() == Some("map")));

        let imp = |m: &str, n: &str| p.imports.iter().any(|i| i.module_path == m && i.name == n);
        assert!(imp("", "json") && imp("net", "http") && imp("../lib", "helper"));
        assert!(imp("tasks", "seed") && imp("foo", "bar"));
        assert_eq!(p.imports.len(), 5, "interpolated require is skipped: {:?}", p.imports);
    }

    #[test]
    fn inheritance_and_mixins_become_impl_relations() {
        let p = parse(
            r#"module Mixin; end
class Base; end
class Foo::Bar < Base
  include Mixin
  extend Helpers::Other
  prepend Wrap
end
class Plain; end
"#,
        );
        let rel: Vec<_> = p.impls.iter().map(|r| (r.type_name.as_str(), r.trait_name.as_str())).collect();
        assert_eq!(rel, [("Bar", "Base"), ("Bar", "Mixin"), ("Bar", "Other"), ("Bar", "Wrap")]);
    }

    #[test]
    fn ast_hash_ignores_comments_and_formatting() {
        let a = parse(
            "def f(x)
  x + 1
end
",
        );
        let b = parse(
            "# note
def f( x )
  # inner
  x +  1
end
",
        );
        let c = parse(
            "def f(x)
  x + 2
end
",
        );
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }
}
