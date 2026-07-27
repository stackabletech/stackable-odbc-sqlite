# Project Rules

Read and follow @AGENTS.md — it contains architecture, patterns, and procedures.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function
  whose behaviour you change. The generic FFI entry points live in
  `stackable-odbc-core`, but what this driver returns from `get_info`,
  `get_info_raw`, the catalog functions and the type-conversion paths is
  directly observable by applications, and each has a spec-defined shape and
  value range. Never claim a SQLSTATE or an info value is wrong without checking
  the actual spec table first. Pay attention to **(DM)** annotations — those
  SQLSTATEs are returned by the Driver Manager, not the driver.
- **Route every client error through `map_sqlite_error`.** Never hand-build an
  `OdbcError` or `SqliteError` from a `rusqlite::Error` at the call site; that
  function is the single place that decides the SQLSTATE. A new classified
  variant must carry the originating error in its `cause` field, or the
  diagnostic reports native code `0`.
- **One error type.** Every `Backend` and `StatementBackend` method returns
  `Result<_, SqliteError>`. An `OdbcError` core produced travels back through
  `SqliteError::Odbc` via `.into()` — never reclassify it, which would discard
  the SQLSTATE core chose.
- **Declare each capability once.** A `SQLGetInfo` value with a `Backend` hook
  is answered through the hook only, never also in `get_info_raw`. Two answers
  are a value that can disagree with itself.
- **Use `odbc-sys` types** — never redefine enums, structs, or constants it
  already provides. Reach them through `stackable_odbc_core::types`, or through
  `stackable_odbc_core::odbc_sys` for anything `types` does not re-export. Do
  **not** add `odbc-sys` as a direct dependency, and do not hand-roll a
  `#[repr(C)]` mirror of one of its structs.
- **Convert raw integers to typed enums at the boundary** — use the
  `xxx_from_raw()` functions from core, never `transmute`.
- **Do not make result-set fetching lazy.** `exec_direct` materialises every row
  before returning, and two reported ODBC capabilities
  (`SQL_CURSOR_COMMIT_BEHAVIOR`, `SQL_CURSOR_ROLLBACK_BEHAVIOR`) are only
  correct because of it. See the Transactions section of AGENTS.md.
- **Run `pre-commit run --all-files`** before every commit. This is the single
  source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring, or "improvements" beyond what was asked.
- If unsure whether something is in scope, ask.

## Data Retrieval

Never read entire files by default. Survey, locate, then extract.

1. **Survey first** — check file size before reading (`stat -c%s file`). Files
   >50 KB must be sliced, not read whole. `src/ffi_integration_tests.rs` (~4600
   lines), `src/backend/metadata.rs` (~1700), `src/backend/info.rs` (~1600) and
   `src/type_conversion.rs` (~1000) are all well over that.
2. **Navigate definitions with ctags** — run `ctags -R .` once to build a tags
   index, then `grep "^SymbolName" tags` to find the exact file and line of any
   function, struct, or trait — no file reading needed.
3. **Locate with Grep** — find patterns, keywords, or usages before reading. Use
   `-C` for context lines.
4. **Extract with Read (offset + limit)** — once you know the line range, read
   only that slice.
5. **Structured data** — use `jq` for JSON, `yq` for YAML; never read raw markup
   whole.
6. **Filesystem survey** — use `tree -L 2 -I '.git|target|node_modules'` instead
   of recursive `ls`.
7. **Verify edits with diff** — after editing, `git diff -u` to confirm changes
   instead of re-reading.
