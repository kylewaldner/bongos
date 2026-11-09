//! The format reference a chat is given: the same text as the header comment of a proposal file
//! and `~/.bongos/task-builder/format.txt`.

/// About 30 lines. Every line is a `#` comment except `format 1`, so the text can lead a proposal
/// file: the parser reads `format 1` as the optional first header line and skips the comments.
pub const FORMAT: &str = "\
# bongos task proposal, format 1. Lines starting with # are comments; blank lines are skipped.
format 1
# A proposal is one file: a header, then one root task with its subtree, indented two spaces a level
# (no tabs). The header lines go in this order:
#   proposal N               the number of this file (required)
#   for ID                   the id of the task whose directory holds the file (optional)
#   by kyle | chat ID | workflow NAME   who wrote it (optional)
#   why TEXT                 one line on the reason (optional)
# Then:
#   task LABEL: GOAL | id ID | kind KIND | keeps work | drop | state WORD
#       LABEL is lowercase letters, digits and '-' and unique among its siblings.
#       KIND is pr-ship, pr-merge, document, spike, babysit or other.
#       A line with an id is that existing task; a line without one is new.
#       'state' is printed for reading and changes nothing when edited.
#   result TEXT              the one result a leaf produces, in words
#   todo NAME | role ROLE | workflow NAME | done | skipped | cancelled | started
#       ROLE is produces-branch, ships or plans. A leaf with no todo lines gets its kind's todos.
#       done, skipped, cancelled and started are printed for reading and change nothing.
#   after TASK | unblock POINT       run after the sibling TASK (default point: merged)
#   stacked on TASK | unblock POINT  branch from the sibling TASK (default point: review)
#       POINT is implemented, review or merged. TASK is a sibling's label, never a path.
# Under a task go its result, todos and edges, then its child tasks. A task holds either child
# tasks (a group) or todos and a result (a leaf).
# Anything that has its own result, a PR, a document or a spike's findings, is a task; the steps
# on the way to that result (implement, review, ship) are todos of that task.
# Each problem is reported with its line. A proposal with an error is not applied.
";
