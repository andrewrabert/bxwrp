# bxwrp

A sandboxing tool (CLI, MCP).

Supports [bashkit](https://bashkit.sh), [bubblewrap](https://github.com/containers/bubblewrap) (Linux), and seatbelt / `sandbox-exec` (macOS).

## Usage

```
Usage: bxwrp [OPTIONS] <COMMAND>

Commands:
  config
  exec
  mcp
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>
      --session <PATH>                             [env: BXWRP_SESSION=]
      --backend <BACKEND>                          [possible values: bashkit, bwrap, seatbelt]
      --exclude <PATTERN>
      --include <PATTERN>
      --cwd <VFS_PATH>
      --username <NAME>
      --hostname <NAME>
      --env <NAME=VALUE>
  -m, --mount <[HOST_PATH][:VFS_PATH[:rm|ro|rw]]>
      --tool <NAME[=PATH]>
  -h, --help                                       Print help
```

Can also be used as a drop-in, non-interactive shell when symlinked to `bxwrp-sh` or `sh`.
