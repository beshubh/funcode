# funcode

`funcode` is an early terminal coding-agent prototype built with Rust and Ratatui. Phase 1 uses a
background demo runner and streams a hardcoded response; it does not connect to a model or execute
tools yet. Browser authentication with a ChatGPT subscription is available for the upcoming model
integration phase.

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

## Controls

- Enter: submit the composer
- Up/Down: choose an open command or file suggestion
- Tab or Enter: activate the selected suggestion
- Shift+Enter: insert a newline on terminals with enhanced keyboard reporting
- Ctrl+J: portable newline fallback
- PageUp/PageDown: scroll the transcript
- End: return to the latest transcript content when scrolled up
- Esc twice within 500 ms: interrupt the active response and continue with the next queued prompt
- Click Thinking or Tools: expand or collapse the widget while that activity is running
- Type `/` at the start of the composer: browse registered commands
- Type `@` anywhere in the composer: search workspace files
- Move the mouse over a suggestion to highlight it; click to activate it
- `/auth`: open the authentication picker
- `/exit` or Ctrl+C: quit

Prompts submitted while the runner is busy are shown immediately and processed in FIFO order.
Thinking is only shown while the runner is thinking. Tools is only shown during an active tool call;
the phase-one fake runner does not call tools. New composer commands implement the `Command` trait
and are added through `App::register_command`; command actions can update app state and optionally
return an `AppAction` for the runtime to dispatch. The additional commands displayed on the home
screen are placeholders.
