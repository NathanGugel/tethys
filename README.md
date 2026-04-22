# Tethys

Tethys is going to be a UI for running multiple claude code instances in parallel. this is what i have so far:

there is a concept of a workspace

- a workspace has:
  - a worktree for both the frontend + backend + other repos that are paired
    - when creating a workspace, i can select which of these i want to create. they will each have their own startup script that does stuff like install deps
  - some number (typically 1) claude instances that i can easily find and resume
- i can delete the worktrees when i'm done and it cleans everything up
- see pr status for whatever prs are attached to that workspace
- i can open vscode for any of the repos in the worktree easily
- if it's my turn (claude is asking question, or completed), its flagged
- i can mark a workspace as "paused", it will not be flagged, until i revive one of the claude chats
