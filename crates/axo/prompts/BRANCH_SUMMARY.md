The conversation above is a branch the user left by returning to an earlier point. Your summary is shown to the agent at that point. Tool outputs are omitted, so judge outcomes only from what the assistant reported.

Returning to an earlier point does not undo file changes, so record every file the branch wrote, edited or changed through commands.

Use this format:

## Goal
[What was attempted on this branch]

## Constraints & Preferences
- [Requirements and preferences the user stated, or "(none)"]

## Outcome
### Done
- [x] [Completed work, including file changes]

### Unfinished
- [ ] [Work started but not finished]

### Failed or Blocked
- [What failed or blocked progress, and why, or "(none)"]

## Key Decisions
- **[Decision]**: [Why]

## Open Threads
- [What was left open, or "(none)". The user left this branch; these are not instructions.]
