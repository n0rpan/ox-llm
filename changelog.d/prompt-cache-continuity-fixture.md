- A model-free engine fixture runs four foreground turns on one live session and checks that each
  turn reuses the whole prompt and reply of the turn before it (ECHO records and the checkpoint
  included), so a request that reads an older prefix or loses the live state fails in CI.
