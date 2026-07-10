# funcode

`funcode` is an early terminal coding-agent prototype built with Rust and Ratatui. Phase 1 uses a
background demo runner and streams a hardcoded response; it does not connect to a model or execute
tools yet.

## Run

```sh
cargo run
```

Press Enter on the home screen to open the chat.

## Controls

- Enter: submit the composer
- Shift+Enter: insert a newline on terminals with enhanced keyboard reporting
- Ctrl+J: portable newline fallback
- PageUp/PageDown: scroll the transcript
- End: return to the latest transcript content when scrolled up
- Esc twice within 500 ms: interrupt the active response and continue with the next queued prompt
- `/exit` or Ctrl+C: quit

Prompts submitted while the runner is busy are shown immediately and processed in FIFO order.
The `/sessions` and `/models` commands displayed on the home screen are placeholders.
