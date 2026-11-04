# GUI smoke test for the gitforest desktop app

Drives the real Electron app through Chrome DevTools Protocol: no new
dependencies, just Node 22 (`fetch` + `WebSocket`) and a display.

## Run it

On a machine with a display (or `Xvfb` installed):

```sh
cargo build --release --bin gitforest
cd desktop && npm run build:renderer && npm run build:electron && npm run build:preload
./gui-test/run.sh
```

`run.sh` uses a scratch home, pre-seeds identity + onboarding, launches the
app with a debug port, seeds one fixture repo through the app's own daemon,
walks all six pages (Repos, Network, Analytics, Compute, Profile, Settings),
publishes a display name and submits a compute job through the real UI, and
fails on any page-console error. Screenshots land in `gui-test/out/`.

Environment overrides: `GUI_OUT`, `GUI_CDP_PORT`, `DISPLAY`.

## Letting an agent operate the GUI via MCP

`mcp.js` is a zero-dependency stdio MCP server (`gitforest-gui`) on top of
`driver.js`. It launches the app itself under a scratch home, so the agent
gets a full drive-and-verify loop with no manual setup:

```json
{
  "mcpServers": {
    "gitforest-gui": {
      "command": "node",
      "args": ["/path/to/git-forest/gui-test/mcp.js"]
    }
  }
}
```

Tools: `gui_launch`, `gui_click_text`, `gui_click`, `gui_fill`,
`gui_screenshot` (returns the PNG as an image block, also saved under
`gui-test/out/`), `gui_body_text`, `gui_wait_text`, `gui_wait_selector`,
`gui_evaluate`, `gui_console_errors`, `gui_stop`.

`gui_launch` needs a display (`DISPLAY` or Xvfb), a built desktop app,
and a free port 7878 (the app spawns its own daemon). `driver.js` is
also usable as a library (`connect()` returns click, type, fill,
evaluate, screenshot, waitForText).

Keep the CDP port on loopback only: anyone who can reach it can drive
the app.
