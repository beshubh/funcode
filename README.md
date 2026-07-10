# funcode

`funcode` is an early terminal coding-agent prototype built with Rust and Ratatui. It connects to
ChatGPT through the saved subscription login, streams model responses in the background, and keeps
successfully completed turns as conversation context for the current session. Tool execution is not
implemented yet.

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

- Enter: submit the composer
- Shift+Enter: insert a newline on terminals with enhanced keyboard reporting
- Ctrl+J: portable newline fallback
- PageUp/PageDown: scroll the transcript
- End: return to the latest transcript content when scrolled up
- Esc twice within 500 ms: interrupt the active response and continue with the next queued prompt
- Click Thinking or Tools: expand or collapse the widget while that activity is running
- `/auth`: open the authentication picker
- `/exit` or Ctrl+C: quit

Prompts submitted while the runner is busy are shown immediately and processed in FIFO order. Only
completed turns are included in later model context; failed or interrupted turns remain visible but
are not sent again. Thinking is shown until the first response text arrives. Tools is only shown
during an active tool call; the current agent does not call tools. The commands displayed on the home
screen are placeholders.
