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
