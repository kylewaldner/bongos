Something is wrong with the change for this goal: either the tests fail or a review asked for changes.

Goal:

{{inputs.goal}}

Find out what is wrong. Run the project's tests to see failures, and read `git diff` to see the change so far. Fix the cause rather than the symptom: do not weaken or delete a test to make it pass unless the test itself is wrong, and say so if it is. If a review left comments, address each one or say why you did not.

Commit your work on the current branch with a short message in the style of the repository's history; the run brings that branch into the task branch. Report a short summary of what you fixed.
