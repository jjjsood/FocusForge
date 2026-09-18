# GUI MCP

FocusForge exposes Slint's built-in, development-only MCP server for UI
inspection and interaction. It is available only in a binary built with the
`mcp` feature; normal and release builds do not include it.

## Start it

From the repository root, run:

```bash
tools/run-gui.sh
```

The launcher builds `focusforge-gui` with `--features mcp`, opens the fixture
project, and starts Slint's Streamable HTTP endpoint at
`http://localhost:8080/mcp`. Keep that process running, then start Codex. The
harness `.mcp.json` contains the matching client entry.

To use another free port, set it before launching and change the MCP client
URL to match:

```bash
SLINT_MCP_PORT=8081 tools/run-gui.sh
```

## Available UI tools

Slint's built-in UI inspection server supplies these tools. Callers first call `list_windows`, then can read
window properties and the element tree, find elements by ID, capture a
screenshot, click or drag an element, set an accessible value, and dispatch
keyboard input. The running GUI is the source of truth: inspect it after each
interaction.

The endpoint binds only while the development launcher is running. Do not use
it in production builds or expose it beyond the local machine.
