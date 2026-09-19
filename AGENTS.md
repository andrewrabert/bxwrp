# bxwrp

A sandboxing tool (CLI, MCP).

Uses [bashkit](https://bashkit.sh), [bubblewrap](https://github.com/containers/bubblewrap).

## dev
- use `just` for dev tasks (build, clippy, and fmt).
- `config.example.yaml`: complete example config. Any change to the config
  format MUST update this file to match.

## mounts
- Each mount is a layer applied in order over the view built so far, replacing
  everything under its target rather than merging with it, so the last layer
  covering a path is the only one that path can see.
