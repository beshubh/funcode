# TODO

## Issues for agents to work on.
- [x] The pasted blocks when sent to funcode should appear normally like the visual text should appear as it is, that in the transcript, the actual text that I have pasted should be shown.
- [x] Fix token usage calculation: it can show only 2% used even at 250k tokens, which exceeds the context window of available models; determine whether the token count, context-window size, or percentage calculation is incorrect.
- [x] Improve UI performance for long agent conversations. After many messages, tool calls, and terminal commands accumulate in the transcript, scrolling and the input box begin to freeze, and user actions can take 10–15 seconds to take effect.


# Issues that need human, if you are agent, do not do anything about these issues
- [ ] Investigate prompt caching behavior and whether we are handling prompt caching optimally.
- [ ] Add a `/sessions` command to list sessions per project (ask Shubham to first review how opencode and pi manage sessions).
- [ ] Add reminder for session export to HTML and session sharing via GitHub Gists.
