# funcode

`funcode` is an early terminal coding-agent prototype built with Rust and Ratatui. It connects to
ChatGPT through the saved subscription login, streams model responses in the background, and keeps
successfully completed turns as conversation context for the current session. The agent can inspect,
search, edit, create, and verify files through workspace-scoped tools. Composer suggestions are
available for registered commands and workspace files.

## Run

```sh
cargo run
```

Press Enter on the home screen to open the chat.

To open the authentication picker directly:

```sh
cargo run -- auth
```

Choose **ChatGPT subscription** with the arrow keys and Enter, or click it with the mouse. Funcode
opens the OpenAI sign-in page in your browser and waits for the localhost callback. If a browser
cannot be opened automatically, copy the displayed URL. Credentials are stored in
`~/.funcode/auth.json`; on Unix, the directory and file are restricted to the current user.

The default model is `gpt-5.4`. Override it when starting Funcode:

```sh
FUNCODE_MODEL=gpt-5.3-instant cargo run
```

If credentials are missing or the saved refresh token is rejected, the failed turn tells you to run
`/auth`. Expired access tokens are refreshed automatically and replaced atomically on disk.

## Controls

- Up/Down: choose an open command or file suggestion
- Tab: switch between persistent Plan and Build modes without changing composer text
- Enter: submit the composer or activate the selected suggestion
- Shift+Enter: insert a newline on terminals with enhanced keyboard reporting
- Ctrl+J: portable newline fallback
- Ctrl+C: clear all composer text
- PageUp/PageDown: scroll the transcript
- End: return to the latest transcript content when scrolled up
- Esc twice within 500 ms: interrupt the active response and continue with the next queued prompt
- Type `/` at the start of the composer: browse registered commands
- Type `@` at the start of a token: insert a highlighted workspace-file reference in place
- Unmatched `@text` stays plain text
- Move the mouse over a suggestion to highlight it; click to activate it
- Drag across terminal text to select and automatically copy it to the clipboard
- Click a sent message: open a modal and copy its text and attached paths
- Click a Thinking or tool block: expand its persistent activity summary
- `/auth`: open the authentication picker
- `/models`: choose a model from every configured provider; use arrows or hover, then Enter/click
- `/theme`: preview and select a bundled color theme
- `/exit`: quit

Prompts submitted while the runner is busy are shown immediately and processed in FIFO order. Only
completed turns are included in later model context; failed or interrupted turns remain visible but
are not sent again. Thinking is shown until the first response text arrives. Tool calls remain as
expandable transcript blocks after completion. New composer commands implement
the `Command` trait and are added through `App::register_command`; command actions can update app
state and optionally return an `AppAction` for the runtime to dispatch. The commands displayed on the
home screen and command popup both read from this registry, so they stay in sync. Plan/Build is
session state controlled with Tab or the composer tabs, rather than composer text or slash commands.

## Agent tools

Build mode exposes `read_file`, `search_files`, `edit_file`, and `terminal`. File tools reject paths
that escape the directory where Funcode was launched. Edits use exact text replacements or create a
new file, write atomically, and display a unified diff. Terminal commands run through non-interactive
Bash from the opened project; stdout and stderr stream into a terminal-style block with the command
and exit status.

Plan mode omits `edit_file` but allows reads, searches, and terminal inspection. The terminal's
read-only Plan behavior is instruction-enforced rather than an operating-system sandbox, so arbitrary
Bash still has the permissions of the Funcode process.

Model discovery runs outside the terminal event loop. Providers use their live model-catalog API
when one is available; provider adapters can return a built-in catalog when no discovery endpoint
exists. The current ChatGPT subscription provider reads its live Codex model catalog after sign-in.
The active model is shown in the composer border and applies to subsequent model requests.

## Themes

FunCode starts with the `terminal` theme, which leaves foreground and background colors to the
terminal emulator and uses its ANSI cyan accent. `/theme` opens the bundled Terminal, Fun Dark,
Midnight, and Paper themes. Arrow keys or mouse movement preview a theme, Enter or click saves it,
and Escape restores the previous selection. The selected theme ID is stored atomically in
`~/.funcode/config.json`.

Theme colors are resolved through semantic roles rather than widget-specific color values. Accent
drives activity, commands, selections, attachments, and the Fun logo; Plan and Build keep distinct
orange and GitHub-style green mode colors across themes.

Provider catalogs are cached in `~/.funcode/models.json` for 24 hours. In the model picker, press
`r` or click **Refresh** to bypass the cache, query authenticated providers, and replace the saved
catalog.
