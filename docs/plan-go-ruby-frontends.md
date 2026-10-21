# Plan: Go and Ruby frontends for `code-atlas-mcp`

## Context
The original build (Milestones 1–4) is done for Rust, Python and TypeScript. Adding a language is deliberately cheap: a `Language` variant, three `.scm` query files, one `Frontend` impl, optionally one `CfgLang` impl. The generic engine (`parser.rs`, `linker.rs`, `sync.rs`, `batch.rs`, the DB layer) needs no changes. `tree-sitter-go 0.25.0` is already in `Cargo.toml`; `tree-sitter-ruby` is not and must be added (pin whatever resolves against tree-sitter 0.27, typically 0.23.x).

Order: **Go first** (static types, closest to Rust, grammar present), then **Ruby** (dynamic, the harder linking problem). Each language is a separate, independently shippable slice.

## Shared wiring (once, in the Go slice)
- `src/ingestion/walker.rs`: add `Language::Go` / `Language::Ruby`; `from_path` (`go`; `rb`, `rake`, `gemspec`, plus `Rakefile`/`Gemfile` by file name, which needs a small `file_name()` check since it matches on extension today); `as_str` ("go", "ruby").
- `src/ingestion/lang/mod.rs`: `pub mod go; pub mod ruby;` and `frontend_for` arms. The compiler flags every non-exhaustive match, which is the checklist.
- `src/ingestion/cfg.rs`: `cfg_lang_for` arms. The `Option` return means CFG can ship later; unsupported languages already degrade.
- `src/ingestion/linker.rs` test-path heuristics (`is_test` in `db/queries.rs` / sync): add `_test.go` (Go) and `spec/`, `test/`, `*_spec.rb`, `*_test.rb` (Ruby). Check where the existing test-file patterns live (grep `_test`) and extend there.
- `README.md`: supported-languages list; remove Go from "not built".

## Delegation: what goes to Haiku subagents
Rule: Haiku gets work that is mechanical, has a clear pattern to copy, and a compiler or test as the judge. I keep anything that changes shared semantics or needs judgment, and I review every Haiku diff before moving on. Not a git repo, so there are no worktrees: agents must own **disjoint files** and never touch the shared ones.

**Me (before any fan-out), in order:**
1. Shared wiring: `walker.rs` variants and extension mapping, `lang/mod.rs` arms, `cfg_lang_for` arms, test-path patterns. Add `go.rs` and `ruby.rs` as compiling stubs (`compiled()` returning an error, trait methods `unimplemented`-free defaults) so the crate builds and each agent can work in its own file without edits elsewhere.
2. `Cargo.toml` (`tree-sitter-ruby`) and a first `cargo check` so agents don't race the dependency resolve.
3. Linker changes (per-language `CallKind::SymbolRef`, Ruby same-class tier, package-qualified Go receivers). Shared logic, regression risk for the other three languages.
4. All design decisions listed at the end, and the final review/integration pass.

**Haiku agents (parallel, one message, disjoint files):**
- **A: Go queries + risks** — `queries/go/{symbols,calls,risks}.scm`. Input: the Go sections above and `queries/python/*.scm` as the pattern. Done = the file compiles (a one-line test calling `GO.compiled(..)` passes) and each risk has a positive and a negative fixture snippet.
- **B: Ruby queries + risks** — same for `queries/ruby/*.scm`.
- **C: Go frontend tests + fixtures** — writes the test battery in `go.rs` (`#[cfg(test)]` module only) and the Go slice of the `poly/` fixture, from the spec above, *before* the implementation exists, so tests are written independently of it (tests first, then I or an agent fills the impl until green).
- **D: Ruby frontend tests + fixtures** — same for Ruby.
- **E: Real-corpus smoke** (after the slice is green) — copies a Go and a Ruby repo into the scratchpad, runs the release binary through `drive.py`, and reports the metrics from the Verification section as a table. Read-only on source; reports, doesn't fix.
- **F: README/docs** — supported-languages and limitations text, once behavior is final.

**Implementation of the `Frontend` impls and CFG classifiers** (`go.rs`, `ruby.rs`, `GoCfg`, `RubyCfg`): the hooks are mechanical when copied from `rust.rs`/`python.rs`, but Ruby visibility scoping, entrypoint heuristics and the CFG `Try`/`Match` mapping are judgment-heavy. Plan: Go impl and `GoCfg` also go to a Haiku agent (it is closest to Rust and fully test-driven by C); Ruby impl and `RubyCfg` stay with me.

**Escalation rule:** if a Haiku agent's tests are still red after two iterations, or it proposes touching a file outside its set, stop it and take the task over myself rather than re-prompting. Every agent prompt must include: its file set, the pattern file to copy, the exact test command to run (`cargo test -p code-atlas-mcp <filter>`; one filter only), and "do not run `cargo fmt` or `clippy --fix` on the whole crate" (past fmt passes broke later text edits; I run both once at the end).

**Cargo contention:** all agents share one `target/` and serialize on its lock. Use debug `cargo test` filters during development; the one release build happens at the end, run by me.

## Slice 1: Go

**Files:** `queries/go/{symbols,calls,risks}.scm`, `src/ingestion/lang/go.rs` (`GoFrontend`, `pub static GO`).

**Symbols (`symbols.scm`)**
- `function_declaration` → `fn`; `method_declaration` → `method` (receiver in `receiver` field); `type_spec` whose type is `struct_type` → `struct`, `interface_type` → `interface`, otherwise (named types, aliases) → `struct`-less `class`? Decision: other named types become `struct`; `const_spec` → `const`; package-level `var_spec` → `global`. `symbol_kind` returns `None` for locals (only match children of `source_file`, or the `const_declaration`/`var_declaration` at top level).
- Grouped declarations (`type ( ... )`, `const ( ... )`) need the capture on the `*_spec`, not the declaration, so each gets its own symbol. Doc comment lookup must then climb to the enclosing declaration for the preceding comment.
- `scope_chain`: Go has no nesting; use the receiver base type for methods (`Server`, with `*` stripped), so ID is `path::Server::Handle#method:xxxx`. Package name is not in the chain (the path already identifies it). Closures are not symbols.
- `param_types`: receiver excluded; `parameter_declaration` can declare several names with one type (`a, b int`), so emit the type once per name. Variadic → `...T`.
- `visibility`: exported iff the first rune of the name is uppercase → `"pub"`, else `"private"`. (`internal/` directories could map to `"crate"`; cheap and accurate, include it.)
- `doc_comment`: contiguous `//` comments immediately above; strip `//`. Same shape as `rust.rs::doc_comment`.
- `is_entrypoint`: `func main` in `package main`; `func init()`; `TestXxx` excluded as test code; exported funcs with handler signature `(http.ResponseWriter, *http.Request)` or `(*gin.Context)` / `echo.Context` / `fiber.Ctx` / `*fiber.Ctx`; `//export` cgo comment; `http.HandleFunc` / `mux.Handle*` registration is handled by the linker's route-registrar rule (add `HandleFunc`, `Handle`, `GET`, `POST`, ... to its list, check the current list first).
- `is_branch`: `if_statement`, `for_statement`, `expression_case`, `type_case`, `communication_case`, `default_case` excluded, `&&`/`||` `binary_expression`.
- `is_nested_def`: `function_declaration`, `method_declaration` (func literals count toward the enclosing function, as arrow functions do in TS? check the TS choice and mirror it: closures are inlined into the parent's complexity).
- `flatten_import`: `import_spec` with optional `name` (alias, `.`, `_`) and `path` string literal; `module_path` = path with the last segment as `name`; blank/dot imports map to name `*`.
- `implements`: Go interfaces are structural, so there is no declaration to read. Emit nothing in v1. v2 option: after parsing, match method-name sets against interface method sets within the package. Deliberately deferred; note in README.

**Calls (`calls.scm`)**
- `(call_expression function: (identifier) @call.target_name)`; `(call_expression function: (selector_expression operand: (_) @call.receiver field: (field_identifier) @call.target_name))`.
- Callbacks: identifiers and selector fields passed in `argument_list` (`http.HandleFunc("/x", h)`, `go worker`, `defer cleanup`), same `@call.callback_via` / `@call.callback_ref` pair as Python. `go f()` and `defer f()` are plain call expressions already.
- Types: `type_identifier` and `qualified_type` inside parameter/result lists → `@type.reference`.
- Imports: `import_spec` → `@import.path`.
- Linker caveat: a package-qualified call (`strings.ToUpper`) has a receiver that is an import alias, not a value. The existing receiver narrowing uses imports, so check that `narrow_by_receiver` treats an identifier matching an imported package name as a module path. If not, add that case (small, benefits Python/TS too).

**Risks (`risks.scm`)**
- `exec`: `exec.Command`, `exec.CommandContext`, `syscall.Exec`. `unsafe`: any `unsafe.*` selector, `unsafe_usage`. `memory`: `unsafe.Pointer`, `reflect.SliceHeader`. `io`: `os.Open/Create/ReadFile/WriteFile/Remove*`, `ioutil.*`, `net.Dial`, `http.Get/Post`. `raw_sql`: `db.Query/Exec/QueryRow` (+`Context` variants) whose first-or-second arg is a `binary_expression` or a `fmt.Sprintf` call. `concurrency`: `sync.Mutex/RWMutex/WaitGroup`, `go_statement`, `chan` types. `error_swallow`: assignment to blank `_ =` where the right side is a call (heuristic; keep narrow) and `recover()` call. `deserialization`: `gob.NewDecoder`, `yaml.Unmarshal` into `interface{}` skipped (too noisy). `xss`: `template.HTML(...)`, `template.JS(...)`.
- `unwrap` has no Go analogue; skip. Idiomatic `if err != nil` is just a branch.

**CFG (`GoCfg`)**
- Map `if_statement` (with `initializer`, `alternative` chaining to `else if`), `for_statement` → `Loop`, `expression_switch_statement`/`type_switch_statement`/`select_statement` → `Match { breakable: true }`, `return_statement` → `Return`, `break`/`continue` (labels ignored → treated as innermost), `panic(...)` call → `Panic`, `defer` → `Simple`. `goto` → `Simple` (documented limitation). No `Try`; `is_try` false; `is_panicking_call` = `panic`/`log.Fatal*`/`os.Exit`.
- `mutated`: `short_var_declaration` left names, `assignment_statement` left roots, `inc_statement`/`dec_statement`.

**Tests (inside `go.rs`, mirroring `rust.rs`/`python.rs`)**
symbols+kinds+IDs+visibility+docs (incl. grouped `type (...)` and methods on pointer receivers); complexity; each risk; entrypoints (`main`, handler signature); calls/callbacks/types/imports (aliased, dot, blank); AST-hash stability under reformatting and comment edits; plus `cfg.rs` tests (`go()` helper like `py()`).

## Slice 2: Ruby

**Files:** `Cargo.toml` (`tree-sitter-ruby`), `queries/ruby/{symbols,calls,risks}.scm`, `src/ingestion/lang/ruby.rs`.

**Symbols**
- `method` (instance, in `class`/`module`) → `method`; top-level `method` → `fn`; `singleton_method` (`def self.x`) → `method`; `class` → `class`; `module` → `class` (no `module` kind exists; keep `class` and rely on scope for disambiguation); `assignment` at class/module/top level whose left is a `constant` → `const`; skip locals and `@ivar`s (instance variables are state, not symbols; the Cartographer can record them as MUTATES_STATE manually).
- `scope_chain`: enclosing `class`/`module` names, outermost first, with `Foo::Bar` scope names kept as written (`name` field can be `scope_resolution`). `def self.x` and instance `x` can share a name: kind is the same, so the `@L<line>` collision marker already handles it, but add `self.` into the scope chain segment? Decision: no; rely on the collision marker, add a test that proves distinct IDs.
- `param_types`: Ruby has no types. Emit `_` per positional param, `*` splat, `**` kwargs, `&` block, `name:` keyword params tagged `kw` so arity/shape feeds `sig_hash` (arity is already part of the hash). Reuse Python's approach.
- `visibility`: track `private` / `protected` / `public` keywords. Bare `private` (no args) affects all following `def`s in the same body, so `visibility()` walks `prev_sibling`s of the def inside its `body_statement` looking for the last bare visibility call; `private :a, :b` / `private def x` (wrapper form) also handled; `private_class_method`. Default `pub`; `private`/`protected` → `private`; names starting with `_` are not private in Ruby, ignored.
- `doc_comment`: contiguous `#` comments above (stop at blank line), strip `# `. Skip `# frozen_string_literal` / `# typed:` magic comments.
- `is_entrypoint`: Rails controller public actions (class whose name ends `Controller` or inherits `ApplicationController`/`ActionController::Base`, public methods); Sinatra/Grape-style block routes (`get '/x' do ... end`) are not symbols, so the handler is the block; handle by marking the enclosing symbol, if any, and otherwise skipping (document); Rake tasks; Sidekiq `perform` in a class that includes `Sidekiq::Worker`/`Sidekiq::Job`; ActiveJob `perform`; Thor commands; `if __FILE__ == $0` is not a symbol; top-level script `main`.
- `is_branch`: `if`, `elsif`, `unless`, `while`, `until`, `for`, `when`, `rescue`, `conditional` (ternary), `if_modifier`/`unless_modifier`/`while_modifier`/`until_modifier`, `&&`/`||`/`and`/`or` binary, `&.` safe navigation (skip: noisy).
- `is_nested_def`: `method`, `singleton_method`, `class`, `module`. Blocks/lambdas count toward the parent.
- `flatten_import`: `require 'a/b'` → module `a`, name `b`; `require_relative '../x'` → relative module path; `load`, `autoload :Foo, 'foo'`. These are `call` nodes, so `@import.path` must be captured with `#any-of? @_m "require" "require_relative" "load" "autoload"`. `include Foo` / `extend Foo` / `prepend Foo` are not imports; they are IMPLEMENTS edges (below).
- `implements`: `class Foo < Bar` → (Foo, Bar); `include`/`extend`/`prepend Mod` inside a class → (class, Mod). Inheritance + mixins are the only static type info Ruby has, and the linker uses `ImplRel` for method narrowing, so this matters for resolution quality.

**Calls**
- Ruby calls are `call` nodes (`receiver`, `method`) and bare identifiers/`call` without receiver (`foo` with no parens is an `identifier`, indistinguishable from a local variable). Capture `(call method: (identifier) @call.target_name)` with and without `receiver`. For paren-less bare identifiers, do **not** emit call sites (too many local-variable false positives); accept missing no-arg, no-paren self-calls, and add a v2 filter that checks the identifier is not assigned in the enclosing method.
- Callbacks: `method(:foo)`, `&method(:foo)`, `&:foo` symbol-to-proc, `before_action :authorize`, `validate :check`, `after_commit :notify`, `scope :x, -> {}`, `send(:foo)`/`public_send`/`__send__` with a literal symbol. Capture `(simple_symbol)` arguments of calls whose method is in an allowlist (`before_action`, `after_action`, `around_action`, `skip_before_action`, `before_save`, `after_save`, `validate`, `validates_with`, `after_commit`, `method`, `send`, `public_send`, `try`, `alias_method`, `delegate`) as `@call.callback_ref` with `@call.callback_via`. Linker restriction: callback refs resolve to `fn`, but Ruby callbacks name **methods**; extend the callback rule so Ruby callback refs may resolve to `method` within the same class (same-file/same-scope tier). Make this per-language through a flag on `CallSite` (e.g. a `CallKind::SymbolRef` variant) rather than relaxing the rule globally, so Python/TS/Rust stay strict.
- Types: none (no annotations in plain Ruby; skip RBS/Sorbet sigs in v1). `@type.reference` file is present but nearly empty; keep a `(constant)`-in-`rescue` capture out.
- Heuristic edges will be the majority in Ruby (duck typing, `method_missing`, metaprogramming). Expected and acceptable: the Cartographer prunes. Note limits in README (dynamic dispatch, `define_method`, `method_missing`, `Module#prepend` ordering, `Struct.new` classes not symbols).

**Risks**
- `exec`: `system`, `exec`, `spawn`, backticks (`subshell` node), `%x()`, `Open3.*`, `IO.popen`, `Kernel.system`, `eval`, `instance_eval`, `class_eval`, `send` with non-literal first arg, `constantize`, `public_send` with non-literal. `deserialization`: `Marshal.load`, `YAML.load` (not `safe_load`), `Oj.load`, `ERB.new(...)` with non-literal. `raw_sql`: `execute`/`select_all`/`find_by_sql`/`where`/`order`/`pluck`/`joins`/`having` whose first arg is a `string` containing `interpolation` or a `binary` `+`. `xss`: `raw`, `html_safe`, `content_tag` with interpolation. `io`: `File.open/read/write/delete`, `IO.*`, `Dir.*`, `FileUtils.*`, `Net::HTTP`, `URI.open`, `open(`. `concurrency`: `Thread.new`, `Mutex`, `Queue`, `Ractor`, `Concurrent::*`. `error_swallow`: bare `rescue` with empty body and `rescue Exception`, `rescue => e` followed by nothing, `ensure`-less `rescue nil` modifier. `unwrap`: `.fetch` without default is *safe* in Ruby; no analogue, skip. `unsafe`/`memory`/`alloc`: n/a (FFI `Fiddle`, `ffi` gem → `unsafe`).

**CFG (`RubyCfg`)**
- `if`/`unless` (+ `elsif`, `else`, modifiers), `while`/`until`/`for`/`loop do` (the last is a call with a block: classify as `Loop` only when the method is `loop`), `case`/`when` → `Match`, `begin/rescue/else/ensure` and method-level `rescue` (method `body_statement` carries `rescue` clauses directly) → `Try`, `return` → `Return`, `break`/`next` → `Break`/`Continue`, `raise` → `Panic`. Implicit return (last expression) is not modelled; falls through to exit, same as Python.
- `mutated`: `assignment`, `operator_assignment`, `@ivar` and `@@cvar` targets (flag these; instance-variable writes are the most useful MUTATES_STATE signal Ruby gives).

**Tests:** same battery as Go, plus: visibility keyword scoping (bare `private`, `private :x`, `private def x`), `def self.x` vs instance collision IDs, `include`/`<` ImplRels, `require`/`require_relative` flattening, symbol callbacks resolving to same-class methods, Rails controller entrypoints, `.rake`/`Rakefile` discovery in the walker test.

## Cross-cutting concerns
- **Grammar gaps:** the TS lesson applies: if a query references a node the grammar lacks, the whole `Query::new` fails and every file of that language errors. Compile-check each `.scm` file early with the first frontend test, and keep optional constructs in separate files (appended conditionally) rather than one monolith.
- **Query compile cost:** queries compile lazily in `OnceLock`, so a repo with no Go pays nothing.
- **Walker false positives:** vendored dirs. `ignore` honours `.gitignore`, but `vendor/` (Go) and `node_modules`-like Ruby `vendor/bundle` are often committed. Add `vendor` to the skipped directory names in `walker.rs` alongside `.git`/`.code_atlas`? Behaviour change for existing languages too (Rust `vendor/` crates). Decision for the user at implementation time; default: skip `vendor/` only for Go and Ruby files by checking path components in `from_path` callers.
- **Linker rules:** keep the existing "receiver-less → `fn` only / receiver → `method` only" rule for Go. For Ruby, receiver-less calls inside a class are usually self-calls to **methods**, so add a same-class-first tier for Ruby (and only Ruby) before the global tier: resolve receiver-less names against methods whose `scope_chain` equals the caller's. Implement through the same per-language flag as callbacks.
- **Mixed-language repos:** both languages must not break `poly/`-style tests; extend that fixture with `.go` and `.rb` files.

## Verification
1. Per slice: `cargo test` (new frontend + CFG tests, all 113 existing stay green), `cargo clippy --all-targets`, `cargo fmt --check`.
2. Real-corpus smoke, release build: copy a medium Go repo (e.g. a few thousand lines of a well-known CLI or HTTP service) and a Ruby one (e.g. a small Sinatra/Rails app or a gem) into the scratchpad, run `sync_repository_skeleton`, and check: zero parse failures, symbol counts roughly match `grep -c '^func '` / `grep -c '^\s*def '`, entrypoints list is plausible, heuristic-edge ratio (expect Go ≲ 30%, Ruby higher), no spurious self-cycles.
3. Drive the stdio loop with the existing `drive.py`/`e2e.py` against a mixed `rs+py+ts+go+rb` fixture: batch → submit → atlas → `query_audit_surface` finds a seeded Go `exec.Command` taint path and a Ruby `system` / `find_by_sql` path.
4. Timing check: cold sync of the mixed corpus stays within the current ~1.5 s per 124K lines envelope.

## Decisions to confirm before coding
- Ruby `module` mapped to kind `class` (no new kind), versus adding a `module` kind (touches schema vocabulary, renderer and docs). Recommend `class`.
- Skip `vendor/` for Go/Ruby by default (recommended) or index it.
- Ship Go and Ruby as two sequential slices (recommended), or together.
