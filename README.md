# TestDet

TestDet is a git-native GraphQL/REST API testing tool. It validates a strictly
typed YAML config, executes one operation end to end (templating, auth, cookies,
and uploads), and exposes a terminal UI for exploration plus a JSON-emitting CLI
for agents and CI. Captured values are redacted wherever they can be printed.

## Install

```sh
cargo install --path crates/testdet
```

`testdet` walks up from the working directory to `testdet/testdet.yaml`; `--config PATH` overrides that and `--env NAME` picks an environment.

## The config

```yaml
version: 1
name: my-api
baseUrl: https://api.example.test/
collections: [collections]
defaultEnvironment: local
environments:
  local: { secrets: { source: dotenv, file: secrets/local.yaml } }
  staging: { baseUrl: https://staging.example.test/ }
```

`collections:` lists directories scanned recursively for operation files, one
request per file, its `id` matching the file stem. A `users.yaml` beside the
manifest declares sessions; `secrets/<env>.yaml` declares variables. The worked
example is `tests/fixtures/e2e/testdet/`: a GraphQL login that captures a token,
a query that spends it, and a REST read that puts a captured value in its path.

## validate

```console
$ testdet validate
$ testdet validate --format json
[]
```

Text format prints nothing on success and sends diagnostics to stderr; JSON
always writes one array to stdout. Warnings alone still exit 0.
```console
$ testdet validate
testdet/collections/projects/projects.yaml:15:5
  headers.Authorization
E_UNKNOWN_VAR  `{{session.tokn}}` in `projects` is not captured by any operation (logins: `login`)
hint: known captures: `projectId`, `token`
```

## run

```console
$ testdet run --all --format json --no-body | sed -n 2p
{"operation":"projects","user":null,"request":{"method":"POST","url":"http://127.0.0.1:8735/graphql","headers":[["Authorization","Bearer <CAPTURE_TOKEN>-CENSORED"]],"body_size":97},"status":200,"headers":[["server","BaseHTTP/0.6 Python/3.14.9"],["date","Wed, 25 Nov 2026 12:15:03 GMT"],["content-type","application/json"],["content-length","59"]],"cookies":[],"body":"eyJkYXRhIjp7InByb2plY3RzIjpbeyJpZCI6IjxDQVBUVVJFX1BST0pFQ1RJRD4tQ0VOU09SRUQiLCJuYW1lIjoiRGVtbyJ9XX19","body_retained":true,"body_size":59,"graphql_errors":[],"timing":{"total":1,"connect":null,"ttfb":null},"transport_error":null,"envelope":"ok"}
```

Select by id, `--tag`, or `--all` (id order); `--user` picks a session. Every
selected operation runs and the exit code is the worst outcome. `--format json`
prints one object for one operation and ndjson for several, so `jq` reads a
stream; text format prints a header and body unless `--no-body` drops it.

## env

```console
$ testdet env
E2E_PASSWORD  required by 2 operation(s)  set <- declared default
  E2E_UNUSED  required by 1 operation(s)  set <- declared default
```

Origins are `environment`, the dotenv path, or `declared default`; an unset variable reads `NOT SET`, and the value is never printed.

## show and schema

```console
$ testdet show projects
  id: projects
  kind: graphql
  query: query Projects {
  tags: projects, smoke
$ testdet schema
{ "__schema": { "queryType": { "name": "Query" }, … } }
```

`show --format json` prints the whole operation; `schema` POSTs the standard introspection query (`--out FILE` writes it, `--legacy` sends the older form).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Everything executed; no transport failures |
| 1 | Config invalid; nothing ran |
| 2 | One or more requests failed at transport level |
| 3 | Usage error (bad flag, unknown subcommand, unknown operation id) |
| 4 | Internal error (I/O, panic caught at the boundary) |

## Redaction

A secret from the shell, a dotenv file, or a declaration `default:`, and any
value from a `capture:`, is registered with the redactor and replaced by a
`<NAME>-CENSORED` marker in every surface: text, JSON, the TUI, `config`, `env`.
Redaction happens when printing, so `--format json` is as safe to paste into a
ticket as the TUI is to screen-share; nothing is written to the config directory.

## TUI keys

| Key | Action |
|---|---|
| `Enter` | run the selected operation |
| `Esc` | cancel the run in flight |
| `u` | switch user |
| `e` | switch environment |
| `s` | store the selected response node as a variable |
| `c` | collapse / expand the selected tree |
| `Tab` / `BackTab` | switch tab |
| `/` | filter |
| `q` | quit |

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

## Not in v1

- `expect:` assertions and pass/fail reporting (they arrive in v2)
- Scenario testing: multi-step flows with capture and dependencies
- Load testing, a web UI, or a daemon
- GraphQL schema validation of queries
- `generate`, and importing Postman / Insomnia / OpenAPI collections
- Editing config: TestDet never writes into the config directory
