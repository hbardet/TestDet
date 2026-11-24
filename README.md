# TestDet

TestDet is a git-native GraphQL/REST API testing tool. It validates a strictly
typed YAML config, executes one operation end to end (templating, auth, cookies,
and uploads), and exposes a terminal UI for exploration plus a JSON-emitting CLI
for agents and CI.

## Build

```sh
cargo build
```

## Test

```sh
cargo test --workspace
```

## Why this YAML parser

TestDet reads its configuration from YAML. The parser must produce an untyped
document tree that carries a line and column for every node and every key, so a
single validation pass can report all errors with source positions at once. It
must reject duplicate mapping keys, treat YAML 1.2 scalars (so `NO`, `yes`, and
unquoted dates stay strings rather than being coerced to booleans or timestamps),
and preserve integers beyond the precision of an `f64`.

We use [saphyr](https://crates.io/crates/saphyr) `0.4.0`. Its `MarkedYaml` nodes
attach a span (line/column) to every scalar, mapping, sequence, and key, and its
loader rejects duplicate keys by default; our probe verified all three properties
plus exact storage of `9007199254740993`.

`serde_norway` was eliminated: its untyped `Value` exposes no per-node span (the
loader's source marks are private), and a scalar like `a: 1` deserialised into a
`String` is silently coerced to `"1"` instead of failing. `serde_yaml` is
archived and excluded. Typed conversion happens downstream over a
`serde_json::Value` (which does not coerce an integer into a string), so the
parser itself needs no serde support.
