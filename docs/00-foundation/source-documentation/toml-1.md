# toml 1.x Reference

> Urd dependency: `toml = "1"` (resolves to 1.1.2)
> Previous: `toml = "0.8"`
> MSRV: 1.85

## Breaking Changes (0.8 -> 1.x)

### 0.8 -> 0.9 (the big jump)

- `from_str`, `Deserializer` no longer preserve order by default
  (use `preserve_order` feature if needed)
- `impl FromStr for Value` now parses TOML *values*, not documents.
  Use `toml::from_str` or `Table::from_str` for documents.
- `Deserializer::new` deprecated -> use `Deserializer::parse`
- `Serializer::new` takes `&mut Buffer` not `&mut String`
  (invisible if using `to_string`/`to_string_pretty` convenience functions)
- Serde support requires `serde` feature (default, so no change for most users)

### 0.9 -> 1.0

- `Time::second` and `Time::nanosecond` are now `Option`
  (only relevant if manipulating TOML datetime types directly)
- Borrowed `&str` deserialization now supported

### 1.0 -> 1.1

- MSRV bumped to 1.85
- No API changes

## Current API

```rust
// Deserialization (unchanged from 0.8 for common usage)
let config: Config = toml::from_str(&contents)?;

// Serialization
let s = toml::to_string(&config)?;
let s = toml::to_string_pretty(&config)?;

// Direct value access
let value: toml::Value = toml::from_str(&contents)?;
let name = value["section"]["key"].as_str();
```

## TOML 1.1 Support (since 0.9.10)

The parser now supports TOML 1.1 features:
- Multi-line inline tables
- Trailing commas in inline tables
- `\e` and `\xHH` string escape sequences
- Optional seconds in time values

## Feature Flags

| Feature | Default | Purpose |
|---------|---------|---------|
| `serde` | yes | Serialize/Deserialize support |
| `std` | yes | Standard library support |
| `preserve_order` | no | Insertion-order maps |
| `fast_hash` | no | Performance optimization |
| `debug` | no | Debug output |

## Urd-Specific Notes

Urd uses `toml::from_str` for config parsing and `toml::to_string_pretty` for
migration output. These convenience functions are unchanged across the 0.8 -> 1.x
boundary. No code changes were needed for the bump.

The TOML 1.1 multi-line inline tables and trailing commas could improve config
readability if adopted in future config schema iterations.
