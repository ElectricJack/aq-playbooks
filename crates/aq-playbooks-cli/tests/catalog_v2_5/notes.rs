use super::*;

pub(super) fn notes() -> Value {
    let mut notes: Value = serde_json::from_str(docs::CATALOG_2_3_NOTES).unwrap();
    let v24: Value = serde_json::from_str(docs::CATALOG_2_4_NOTES).unwrap();
    notes["about"] = json!(
        "What the host says about each contract of catalog 2.5.0, for an authoring agent. Rendered into CATALOG.md for the commands and events a source may use (aq_playbooks_compiler::docs::catalog). An empty string means the catalog does not say. Kept beside registry.json so that the registry, and every fingerprint, stays unchanged. 2.5.0 is catalog 2.3.0 with the changes of 2.4.0 (decision notes and `work.activate`), the sandbox environment commands and events, an optional `environment` on `work.route` and the environment facts of `work.read`; every other note is the one of 2.3.0."
    );
    for name in FROM_2_4 {
        if let Some(note) = v24["commands"].get(name) {
            notes["commands"][name] = note.clone();
        }
    }
    for name in CHANGED_EVENTS {
        notes["events"][name] = v24["events"][name].clone();
    }
    notes["commands"]["work.read"]["purpose"] = json!(
        "Reads the facts of a task as it is now, with those of its latest result (whether it changed anything, its branch and pull request, and how many times delivery has sent it back) and its sandbox environment facts: what it needs and which environments its project allows."
    );
    notes["commands"]["work.read"]["notes"]
        .as_array_mut()
        .unwrap()
        .push(json!(
            "The environment facts, always present: `needs` (at most 64 capability names the task asks for, from its `aq:needs` metadata; empty when it asks for none); `default_environment` (the id of the project's default environment, or `\"\"` when it has none); `environments` (the project's allowed environments, at most 64 `{id, provides}`, where `provides` lists at most 64 capability names the environment provides). To choose an environment, pick one whose `provides` holds every name in `needs`; the document says how to choose among them."
        ));
    notes["commands"]["work.route"]["input"]["environment"] = json!(
        "The id of the environment the task runs in, one of the `id` values of `environments` from `work.read`. Leave it out to keep the task's current environment, or the project's default when it has none. The host refuses an environment the project does not allow or one that lacks a capability in `needs`, and offers the task only to a node that can run it."
    );
    notes["commands"]["work.route"]["outcomes"]["refused"] = json!(
        "Nothing was written; end. `not_offered`: nothing at that level (or in a non-empty list) is offered, and the host raises the task itself; never try a route the document does not give. `owner_route`, `not_on_router`, `unknown_task`: end."
    );
    add_environment_commands(&mut notes);
    add_environment_events(&mut notes);
    notes
}

fn add_environment_commands(notes: &mut Value) {
    notes["commands"]["environment.build"] = json!({
        "purpose": "Builds the environment recipes under `.aq/environments/` whose bytes differ from the project's admitted version, for a task's result head or for the project's admitted recipes on the current base. It answers later, on `aq.environment.completed`.",
        "input": {
            "subject": "The task whose result head to build, at the revision of the trigger. Give `subject` or `project`, not both.",
            "project": "The project whose admitted recipes to rebuild on the current base (after `aq.environment.base_changed`). Give `project` or `subject`, not both."
        },
        "outcomes": {
            "accepted": "The build is queued on the builder; the edge binds `{handle}`. Join the handle under a key.",
            "unchanged": "No recipe differs from the admitted version; there is nothing to admit.",
            "built": "Every changed recipe was built, checked, scanned and published by digest. `builds` has one entry per recipe: its `name`, image `digest`, `recipe_hash` and `toolchains` (`{name, version}`). Pass `builds` to `environment.admit` unchanged.",
            "failed": "A recipe did not build. `recipe` names it, `step` the step that failed and `log` is the redacted log. The previous environment version stays in place; nothing was registered."
        },
        "notes": [
            "The build runs on the builder, outside attempt capacity, and compares recipe bytes itself: no fact about changed files is needed.",
            "Build before the merge, so that a broken recipe never merges: on `failed`, send the task back with `work.return` and the failure as its detail."
        ]
    });
    notes["commands"]["environment.admit"] = json!({
        "purpose": "Records each build as the project's next environment version, adds it to the project's environments and makes it available to nodes. It answers later, on `aq.environment.completed`.",
        "input": {
            "subject": "The task whose builds are admitted. Give `subject` or `project`, not both.",
            "project": "The project whose rebuilt environments are admitted. Give `project` or `subject`, not both.",
            "builds": "The `builds` of the `built` answer of `environment.build`, unchanged."
        },
        "outcomes": {
            "accepted": "The admission is queued; the edge binds `{handle}`. Join the handle under a key.",
            "admitted": "Every build is now the project's next environment version and is available to nodes. An attempt already placed keeps its environment.",
            "refused": "Nothing was admitted and the previous version stays. `reason` is `unknown_build` (no such build was published), `unknown_task`, `unknown_project` or `unreachable` (the registry did not answer); `step` names the failing step."
        },
        "notes": [
            "Admit after the merge: when `pull_request.merge` completes with `merged`. A squash merge makes a new commit with the same recipe bytes, so the builds still match."
        ]
    });
}

fn add_environment_events(notes: &mut Value) {
    notes["events"][COMPLETION] = json!({
        "purpose": "The answer to a sandbox environment command that answers later: `environment.build` or `environment.admit`.",
        "sent": "Only as the completion of an operation a playbook started with one of those commands, once per operation. Join the handle, which types the answer per outcome; do not wait on this event or start a playbook on it.",
        "payload": {
            "outcome": "The outcome of the command that was called: one of its business outcomes, never `accepted`.",
            "output": "That outcome's output: `builds` for `built`, `recipe`, `step` and `log` for `failed`, `reason` and `step` for `refused`."
        },
        "notes": ["It carries no task or revision: the join correlates it by the operation."]
    });
    notes["events"][BASE_CHANGED] = json!({
        "purpose": "A new base template was admitted, so a project's environments must be rebuilt on it.",
        "sent": "Today the host sends it once per project when it admits a new base template (a new OS or runtime image in the builder's configuration).",
        "payload": {
            "project": "The project's id. Pass it as `project` to `environment.build` and then `environment.admit`."
        },
        "notes": [
            "A project whose rebuild fails stays on its previous environment; report the failure to the project's supervisor. Other projects are not affected."
        ]
    });
}
