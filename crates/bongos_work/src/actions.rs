//! The `task`, `todo`, `project`, `artifact` and `work` actions that take a task or todo chosen by name. The namespace `task` is
//! shared with Zed's task runner (`task::Spawn`, `task::Rerun`); no name collides. Each action
//! opens the pickers and prompts it needs, so none carries data.

/// Actions in the `task` namespace.
pub mod task {
    use gpui::actions;

    actions!(
        task,
        [
            /// Asks for a goal and creates a task from it. Only the goal is required.
            New,
            /// Picks a task and asks for the goal of a new task under it.
            NewChild,
            /// Picks a task and completes its current todo.
            Advance,
            /// Picks a task and completes its ships todo although the task it is stacked on is
            /// not Done, leaving a marker.
            AdvanceOverride,
            /// Picks a task and marks it done, whatever its todos.
            Close,
            /// Picks a task and cancels it. A group with open tasks asks first.
            #[action(name = "Drop")]
            DropTask,
            /// Picks a closed or cancelled task and opens it again.
            Reopen,
            /// Picks a task and changes its goal and kind.
            Edit,
            /// Asks for an activity and a goal and creates a one-off task under that activity.
            NewOneOff,
            /// Picks a task, a sibling and the kind of edge (after or stacked on) from the task
            /// to it.
            AddEdge,
            /// Picks a task and one of its edges and removes it.
            RemoveEdge,
            /// Picks a blocked task and shows which tasks hold it back.
            ShowBlockers,
            /// Picks a task and one of its edges and sets how far the sibling must get before
            /// the edge unblocks.
            SetUnblockPoint,
            /// Picks a leaf task and sets whether a merged PR completes its ships todo.
            SetMergeShip,
            /// Picks a stacked task and points it at another sibling, or at main.
            Retarget,
            /// Picks a stacked task that needs a rebase and marks its parent's current head as
            /// seen, so it stops being flagged until the parent's branch moves again.
            ClearRebase,
            /// Picks a leaf task and sets its result: a PR, document, link or other.
            SetResult,
            /// Picks a task with a result and clears it.
            ClearResult,
            /// Picks a leaf task and moves its work into a new child, making the task a group.
            /// Asks first.
            Split,
            /// Picks a task and a task to move it under. Asks first.
            Move,
            /// Picks a task and a project and moves it to the top level of that project. Asks
            /// first.
            MoveToProject,
            /// Picks a task and an activity and files it as a one-off. Asks first.
            MakeOneOff,
            /// Picks an open leaf (pr-ship, pr-merge, spike or babysit) and creates its branch and
            /// worktree in its repository, then runs the repository's setup command there. A
            /// babysit task asks which of its PRs to check out.
            Start,
            /// Picks a task and switches the window to its worktree, or to its directory for a
            /// group or a leaf with no worktree. The workspace left behind stays open in the same
            /// window with its unsaved buffers, neither saved nor closed.
            Focus,
            /// Clears the focused task and goes back to the workspace that was active before the
            /// first focus, when focus left it.
            Unfocus,
            /// Picks a task with a worktree and adds the worktree as a folder of this workspace.
            OpenWorktree,
            /// Picks a task whose result is a PR and opens it in the browser.
            OpenPr,
            /// Picks a task and copies the path of its worktree, or of its directory when it has
            /// no worktree.
            CopyPath,
            /// Picks a task and sets the visibility its new files get: shared, or human-only
            /// (left out of what chat sessions are told).
            SetDefaultVisibility,
            /// Opens a new `untitled-<n>.md` document in the directory of the focused task, or of
            /// a task picked by name when none is focused. Starts nothing else.
            Think,
            /// Picks a task (the focused one when there is one) and a mode to work it in: chat
            /// (`chat: new`), run (`workflow: run`), manual (focuses its worktree or directory)
            /// or thinking (`task: think`). Chat and run say which feature provides them when
            /// it is not there yet.
            Mode,
        ]
    );
}

/// Actions in the `todo` namespace.
pub mod todo {
    use gpui::actions;

    actions!(
        todo,
        [
            /// Picks a task and adds a todo to the end of its list.
            Add,
            /// Picks a task and a todo and removes it.
            Remove,
            /// Picks a task and an open todo and skips it.
            Skip,
            /// Picks a task and a todo that is done or skipped and opens it again.
            Reopen,
            /// Picks a task and a todo and moves it to another place in the list.
            Move,
            /// Picks a task and a todo and sets the workflow a run of it starts.
            SetWorkflow,
            /// Picks a task and a todo and sets its role: produces-branch, ships or none.
            SetRole,
            /// Picks a task and an open todo and marks it as begun.
            Start,
        ]
    );
}

/// Actions in the `project` namespace. Zed's `projects` namespace is a different one.
pub mod project {
    use gpui::actions;

    actions!(
        project,
        [
            /// Asks for a name and creates the project.
            New,
            /// Picks a project and archives it.
            Archive,
            /// Picks an archived project and brings it back.
            Unarchive,
            /// Picks a project and opens its directory as a folder in this workspace.
            OpenFolder,
            /// Picks a project and focuses its top group, like `task: focus`.
            Focus,
        ]
    );
}

/// Actions in the `work` namespace.
pub mod work {
    use gpui::actions;

    actions!(
        work,
        [
            /// Picks a folder, asks for a name and a setup command, and adds the repository to
            /// `work.repositories` in the user settings.
            AddRepository,
            /// Opens the Work panel, or closes it when it already has focus.
            TogglePanel,
            /// In the Work panel, goes up one level (to the group the shown tasks are in).
            Up,
            /// In the Work panel, expands the selected row to show its todos and artifacts, or
            /// folds it again.
            ToggleRow,
            /// In the Work panel, picks a todo name and shows only tasks whose current todo is
            /// that; "Any todo" clears it.
            FilterTodo,
            /// In the Work panel, picks a kind and shows only tasks of it; "Any kind" clears it.
            FilterKind,
            /// In the Work panel, opens or folds the Stale section of the top level: open items
            /// with no activity for the stale threshold.
            ShowStale,
            /// In the Work panel, opens or folds the Closed section of the top level: items that
            /// are done, cancelled or archived.
            ShowClosed,
            /// In the Work panel, draws the group shown as a graph of boxes and arrows, or lists
            /// it again. Enter on a box drills into it or focuses it.
            Graph,
        ]
    );

    #[cfg(debug_assertions)]
    actions!(
        work,
        [
            /// Debug builds only: writes what a chat session on the focused task would be told
            /// to `work-session-context.json` in the logs directory.
            DumpSessionContext,
            /// Debug builds only: writes the rows of the level the Work panel shows to
            /// `work-view.txt` in the logs directory.
            DumpView,
        ]
    );
}

/// Actions in the `artifact` namespace: the files of a task and whether chat sessions are told
/// about them.
pub mod artifact {
    use gpui::actions;

    actions!(
        artifact,
        [
            /// Picks a task and one of its shared artifacts and makes it human-only: chat
            /// sessions are no longer told about it.
            Hide,
            /// Picks a task and one of its human-only artifacts and shares it.
            Share,
            /// Picks a task and makes every artifact of it human-only.
            HideAll,
            /// Picks a task and shares every artifact of it.
            ShareAll,
            /// Picks a task and asks for a URL and a title, and adds a link to that document
            /// (such as a Notion page) to the task's artifacts.
            Link,
        ]
    );
}
