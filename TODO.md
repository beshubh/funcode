# TODO

- [ ] The pasted blocks when sent to funcode should appear normally like the visual text should appear as it is.
- [ ] If I send a message and request to the provider fails, that older message is not retained in history, so if I send retry after failure funcode gets confused as to retry what? which is wrong.
- [ ] If there is a provider failure funcode never retries, it should retry at least 20 times before failing or getting interrupted by user.
- [ ] When pasted block exceeds size limit, error shows raw byte count (e.g., 1048576 bytes); change message to display in MB instead of bytes.
- [ ] read_file randomly fails due to requesting invalid line ranges (e.g., asking 10 lines from a 2-line file); fix range calculation to handle short/empty files safely.
- [ ] When pasted block has no newline and is a single oversized line, insertion fails by buffering raw text instead of adding as a pasted block; fix this behavior so oversized single-line pastes are handled like other large blocks.
- [ ] Move system prompts for plan and build modes into dedicated files.
- [ ] Investigate prompt caching behavior and whether we are handling prompt caching optimally.
- [ ] Track per-session token usage and context utilization, and show it in a top-right rounded square widget.
- [ ] Add a `/sessions` command to list sessions per project (ask Shubham to first review how opencode and pi manage sessions).
- [ ] Add reminder for session export to HTML and session sharing via GitHub Gists.
- [ ] Make Fun window and chat window the same; launch Fun with Fun logo and a widget showing help and input text.
- [ ] Input text box:
  - [ ] cursor should be moveable using mouse.
  - [ ] command/option + left/right should move the cursor to the left of the line/word and right of the line/word.

