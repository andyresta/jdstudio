# JDStudio for AI agents (MCP)

JDStudio ships an [MCP](https://modelcontextprotocol.io) server inside the app. An agent - Claude Code, Claude Desktop, or any other MCP client - starts it as `jdstudio --mcp` and can then create projects, import media, cut, add titles and captions, keyframe, apply effects, record the screen, look at frames and export, with the same edit commands the window uses. Nothing extra to install.

## Set it up

Point your client at the JDStudio binary with the one argument `--mcp`. JDStudio has no installer yet, so that is the `jdstudio` (`jdstudio.exe` on Windows) you built:

| Build | Binary, under the repository's `src` folder |
| --- | --- |
| `cargo build --release -p concat` | `target/release/jdstudio` |
| `cargo dev` (the quick profile) | `target/quick/jdstudio` |

**Claude Code:**

```sh
claude mcp add jdstudio -- "D:/path/to/jdstudio/src/target/release/jdstudio.exe" --mcp
```

**Claude Desktop** (Settings > Developer > Edit Config, `claude_desktop_config.json`), and most other clients, which take the same shape:

```json
{
  "mcpServers": {
    "jdstudio": {
      "command": "D:\\path\\to\\jdstudio\\src\\target\\release\\jdstudio.exe",
      "args": ["--mcp"]
    }
  }
}
```

On Windows, write the path with doubled backslashes in JSON, as above.

**A development build needs its libraries.** A binary built from source loads FFmpeg, ONNX Runtime and sherpa-onnx as shared libraries at start. Put their DLLs (`avcodec-*.dll` and the rest of FFmpeg's `bin`, `onnxruntime.dll`, the sherpa-onnx DLLs) next to `jdstudio.exe`, or on the `PATH` the MCP client starts it with - set `PATH` in the server's `env` in the client's config if the client does not inherit yours. Without them the process exits before it says anything, which a client shows as a server that failed to start.

## How it reaches JDStudio

`jdstudio --mcp` works one of two ways, and the `concat_version` tool says which (`mcp.mode`):

- **Through the window** (`window`). When JDStudio is open with **Settings > Automation > Scripts and apps** on, the window writes where its server is, and its token, to `automation.json` in JDStudio's settings folder, `app.jdstudio.editor` (`%APPDATA%\app.jdstudio.editor` on Windows, `~/Library/Application Support/app.jdstudio.editor` on macOS, `~/.config/app.jdstudio.editor` on Linux; readable only by you). The MCP server finds it and goes through the window's server. This is the safer way while you are editing: the agent cannot open the project you have on screen and save over it, and exports take turns with yours.
- **On its own** (`headless`). Otherwise the MCP server runs the editor's engine itself, with no window. Nothing has to be open. Every JDStudio process locks the project folders it has open (`concat.lock`, let go on close or when the process dies), so a project open in the window is refused here with a note to turn on Automation and use `--mcp window`, and the window refuses a project an agent has open.

Choose explicitly with `--mcp window` (fail rather than run on its own) or `--mcp headless`; the default is `--mcp auto`. To reach a server elsewhere, set `CONCAT_API_ADDRESS` (e.g. `127.0.0.1:7420`) and `CONCAT_API_TOKEN` in the client's `env` for the server.

Either way the agent writes files - projects, exports, frames, recordings - only under your home folder. Set `CONCAT_MCP_ROOTS` (a path list, `:`-separated, `;` on Windows) to allow other folders.

The agent edits projects of its own; it does not edit the project open in the window while you watch. That ("live mode") is planned upstream in [jub0t/Concat#337](https://github.com/jub0t/Concat/issues/337).

## What the agent can do

Tools are named for what they do, and the editing tools' argument schemas are generated from the API's own types, so they carry the same limits and descriptions the engine does.

| Area | Tools |
| --- | --- |
| Projects | `project_list`, `project_create`, `project_open`, `project_save`, `project_close`, `project_set_video` |
| Reading | `timeline_read` (compact: lanes, clips in time order, media, ids), `project_get` (everything), `concat_version` |
| Media | `media_probe`, `media_import` |
| Editing | `clip_add`, `clips_add_sequence`, `clip_trim`, `clips_split`, `clips_move`, `clips_remove`, `clip_update`, `clip_transform`, `clip_speed`, `clip_frame`, `track_add` |
| Picture | `clip_mask`, `clip_stabilize`, `timeline_background` |
| Titles and captions | `title_add`, `captions_add` |
| Keyframes and effects | `keyframe_set`, `keyframe_clear`, `effects_list`, `effect_layer_add`, `effect_key_set` |
| Anything else | `edit_apply` (any edit command, or a batch of them as one undo step), `edit_undo`, `edit_redo` |
| Templates | `template_list`, `template_instantiate` |
| Export | `export_start`, `export_status`, `export_wait` (with progress), `export_cancel` |
| Looking | `preview_frame` (the composited frame as an image) |
| Screen recorder | `recorder_devices`, `recorder_start`, `recorder_status`, `recorder_pause`, `recorder_stop`, `recorder_land` |

Edits stay in memory until `project_save`, as in the window. Times are seconds; a keyframe's position is a fraction of its clip (0 to 1); sizes and offsets are fractions of the frame.

### Recording the screen

The recorder tools drive the same recorder as the window's **Record screen**: the screen (or a part of it), and optionally a camera and a microphone, each into a file of its own so they land as separate clips.

1. `recorder_devices` lists the cameras (with the `id` to pass) and microphones (by name, the default first).
2. `recorder_start` with a `folder` to write into, or a project's `path` (the files go into its `recordings` folder), and optionally `area` (`{x, y, width, height}` in screen pixels), `fps` (1 to 60, 30 when absent), `camera` and `microphone`. It returns a `job` id. One recording runs at a time.
3. `recorder_status` says how long it has run; `recorder_pause` holds it (`paused: true`) and carries it on (`paused: false`), cutting the pause out of every file.
4. `recorder_stop` finishes the files and returns them: `screen`, `camera` and `voice`, each with its `file`, its `start` (seconds after the earliest source began, so the three line up), its length in `seconds` and its picture size.
5. `recorder_land` puts them into an open project as the window does: the screen on a lane from `start`, the camera over it scaled into the bottom-right corner (`cameraShape`: `rectangle`, `rounded` or `circle`), and the voice under both. Then `project_save`.

The recording runs in the MCP server's own process, on the machine it runs on, in either mode, so it records that machine's screen and devices; a recording still running when the client disconnects is stopped and its files finished. The recorder is tested on Windows (GDI screen capture, DirectShow cameras); macOS (AVFoundation) and Linux (X11, V4L2) build the same code but are untested, and macOS asks for Screen Recording, Camera and Microphone permission for whatever app started the server.

## Protocol

MCP over stdio, newline-delimited JSON-RPC 2.0. Both protocol eras are served: the `initialize` handshake (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05) and the per-request `_meta` of 2026-07-28 with `server/discover`. `ping`, `tools/list`, `tools/call`, progress notifications for `export_wait`, and `notifications/cancelled` are supported. Standard output carries only protocol; the server logs to standard error.

For scripts that do not speak MCP, the same API is served as JSON-RPC lines on a socket from Settings > Automation.
