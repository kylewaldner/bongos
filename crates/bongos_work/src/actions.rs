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
