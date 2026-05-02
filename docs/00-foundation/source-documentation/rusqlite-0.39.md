# rusqlite 0.39 Reference

> Urd dependency: `rusqlite = { version = "0.39", features = ["bundled"] }`
> Previous: `rusqlite = "0.32"`
> Bundled SQLite: 3.51.3 (was 3.46.0)

## Breaking Changes (0.32 -> 0.39)

### High Impact for Urd

**`execute()` and `prepare()` reject multi-statement SQL (0.35)**
If any call passes SQL with multiple statements separated by `;`, it now errors.
Use `execute_batch()` for multi-statement SQL.

```rust
// Error in 0.35+:
conn.execute("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)", [])?;

// Correct:
conn.execute_batch("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")?;
```

**u64/usize ToSql/FromSql disabled by default (0.38)**
Storing or reading `u64`/`usize` values requires explicit casting to `i64` or
opting in via the `i128_blob` feature. Prevents silent data loss on values
exceeding i64 range.

**Extended result codes enabled by default (0.32)**
`SQLITE_OPEN_EXRESCODE` is always active. Error codes are more specific.
If matching on error codes, values may differ from before.

### Lower Impact

- `FnMut` -> `Fn` in `create_scalar_function` (0.33)
- `release_memory` feature removed (0.33)
- Hook ownership checking (0.38, 0.39)
- Statement cache made optional (0.38)

## Current API Patterns

```rust
// Connection
let conn = Connection::open("path.db")?;
let conn = Connection::open_in_memory()?;

// Execute (single statement only since 0.35)
conn.execute("INSERT INTO t VALUES (?1, ?2)", params![val1, val2])?;
conn.execute("INSERT INTO t VALUES (?1, ?2)", (val1, val2))?;  // tuple syntax
conn.execute_batch("CREATE TABLE ...; INSERT ...")?;  // multi-statement

// Query
let name: String = conn.query_row(
    "SELECT name FROM t WHERE id=?1", [id], |row| row.get(0)
)?;

// Multiple rows
let mut stmt = conn.prepare("SELECT id, name FROM t")?;
let rows = stmt.query_map([], |row| {
    Ok(MyStruct { id: row.get(0)?, name: row.get(1)? })
})?;

// Transactions
let tx = conn.transaction()?;
tx.execute("...", [])?;
tx.commit()?;

// Parameter binding
params![val1, val2]             // positional
named_params!{"@name": val}     // named
(val1, val2)                    // tuple
&[&val1 as &dyn ToSql]          // slice
```

## New Features Since 0.32

| Version | Feature |
|---------|---------|
| 0.39 | Unix timestamp support for chrono; `TryFrom` for `Value` |
| 0.38 | wasm32 support; virtual table transactions; 64-bit length params |
| 0.37 | `FromSqlError::other` convenience |
| 0.36 | `query_one` method; column metadata; `Name` trait for `&str`/`&CStr` |
| 0.35 | Column metadata from prepared statements |
| 0.34 | `BindIndex` trait; `Deserialize` impls; flexible named params |

## Urd-Specific Concerns

Urd's `state.rs` uses straightforward open/execute/query patterns. Key things:
- Verify no multi-statement `execute()` calls exist (use `execute_batch()` if so)
- Verify no `u64`/`usize` values stored without casting to `i64`
- The `bundled` feature bundles SQLite 3.51.3 — no system SQLite dependency
