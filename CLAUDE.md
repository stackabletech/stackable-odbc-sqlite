# Project Rules

Read and follow @AGENTS.md. It holds the architecture, the patterns and the
procedures, and it is where the reasoning behind every rule below lives.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function
  whose behaviour you change. The generic FFI entry points live in
  `stackable-odbc-core`, but what this driver returns from `get_info`,
  `get_info_raw`, the catalog functions and the type-conversion paths is
  directly observable by applications, and each has a spec-defined shape and
  value range. Never claim a SQLSTATE or an info value is wrong without
  checking the actual spec table first. Pay attention to **(DM)** annotations:
  those SQLSTATEs are returned by the Driver Manager, not the driver.
- **Route every client error through `map_sqlite_error`.** It is the single
  place that decides the SQLSTATE, and a new classified variant must carry the
  originating error in its `cause` field. See
  [Backend error mapping](AGENTS.md#backend-error-mapping).
- **One error type.** Every `Backend` and `StatementBackend` method returns
  `Result<_, SqliteError>`. An `OdbcError` core produced travels back through
  `SqliteError::Odbc` via `.into()`. Never reclassify it, which would discard
  the SQLSTATE core chose.
- **Declare each capability once.** A `SQLGetInfo` value with a `Backend` hook
  is answered through the hook only, never also in `get_info_raw`. See
  [Declaring capabilities](AGENTS.md#declaring-capabilities).
- **Use `odbc-sys` types**, re-exported from `stackable_odbc_core::types`, or
  from `stackable_odbc_core::odbc_sys` for anything `types` does not re-export.
  Never redefine what it provides, never add an `odbc-sys` dependency to this
  crate's `Cargo.toml`, and never hand-roll a `#[repr(C)]` mirror of one of its
  structs. See [Named constants](AGENTS.md#named-constants).
- **Convert raw integers to typed enums at the boundary** with core's
  `xxx_from_raw()` functions, never `transmute`.
- **Do not make result-set fetching lazy.** `exec_direct` materialises every
  row before returning, and two reported ODBC capabilities
  (`SQL_CURSOR_COMMIT_BEHAVIOR`, `SQL_CURSOR_ROLLBACK_BEHAVIOR`) are only
  correct because of it. See
  [Result sets are materialised eagerly](AGENTS.md#result-sets-are-materialised-eagerly)
  and [Transactions](AGENTS.md#transactions).
- **Run `pre-commit run --all-files` before every commit.** It is the single
  source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring, or "improvements" beyond what was asked.
- If unsure whether something is in scope, ask.

## Data Retrieval

Never read entire files by default. Survey, locate, then extract.

1. Survey first. Check the file size with `stat -c%s file` before reading it.
   Anything over 50 KB must be sliced, not read whole; several modules in
   `src/` are.
2. Navigate definitions with ctags. Run `ctags -R .` once to build the index,
   then `grep "^SymbolName" tags` for the exact file and line of any function,
   struct or trait. No file reading needed.
3. Locate with Grep. Find patterns, keywords or usages before reading. Use `-C`
   for context lines.
4. Extract with Read, using `offset` and `limit` once you know the line range.
5. Read structured data with a tool that understands it: `jq` for JSON, `yq`
   for YAML. Never read raw markup whole.
6. Survey the filesystem with `tree -L 2 -I '.git|target|node_modules'`, not a
   recursive `ls`.
7. Verify edits with `git diff -u` rather than re-reading the file.
