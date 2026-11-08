//! The `task` and `todo` actions that take a task or todo chosen by name. The namespace `task` is
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
            /// Asks for a name, creates the project and runs `git init` in its directory.
            New,
            /// Picks a project and archives it.
            Archive,
            /// Picks an archived project and brings it back.
            Unarchive,
            /// Picks a project and opens its directory as a folder in this workspace.
            OpenFolder,
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
        ]
    );
}
