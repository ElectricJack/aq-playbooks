use super::*;

#[test]
fn actual_engine_requires_protection_before_admission_and_encrypts_sensitive_dispatch_and_output() {
    let n = namespace("sensitive-engine");
    let artifact = sensitive_artifact(&n);
    let authority = Authority {
        actor: "fixture".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let input = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
    let plain = Engine::new(store(), namespace("no-protection"), "plain").unwrap();
    plain.install(artifact.clone()).unwrap();
    plain
        .activate("sensitive", &artifact.digest, authority.clone())
        .unwrap();
    assert!(
        plain
            .admit("sensitive", "main", input.clone(), authority.clone(), 0)
            .is_err()
    );
    assert!(plain.snapshot().unwrap().runs.is_empty());
    let root = {
        let engine = Engine::new(protected("old", false), &n, "protected").unwrap();
        engine.install(artifact.clone()).unwrap();
        engine
            .activate("sensitive", &artifact.digest, authority.clone())
            .unwrap();
        let root = engine
            .admit("sensitive", "main", input, authority, 0)
            .unwrap();
        let mut host = SecretHost { calls: 0 };
        // Up to the step after the result is bound, so the protected values
        // are still live (a finished run leaves the State; history holds
        // public projections only).
        engine.advance(&root, 0).unwrap();
        engine.dispatch_next(&mut host, 0).unwrap();
        engine.advance(&root, 0).unwrap();
        let state = engine.snapshot().unwrap();
        assert_eq!(state.runs[&root].step, "return");
        assert_eq!(host.calls, 1);
        assert_eq!(
            state.runs[&root].variables["private_result"]["secret"],
            "SYNTHETIC_PROTECTED_SENTINEL"
        );
        root
    };
    verify_restored_dispatch(&n, root);
}

fn verify_restored_dispatch(n: &str, root: String) {
    let restored = Engine::new(protected("old", false), n, "reopened").unwrap();
    assert_eq!(
        restored
            .snapshot()
            .unwrap()
            .intents
            .values()
            .next()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .1["secret"],
        "SYNTHETIC_PROTECTED_SENTINEL"
    );
    restored.drive(&mut SecretHost { calls: 0 }, 1, 20).unwrap();
    assert!(restored.snapshot().unwrap().runs.is_empty());
    let query = |kind| InspectionQuery {
        kind,
        run: Some(root.clone()),
        after: None,
        limit: 100,
    };
    let runs = restored
        .store
        .inspect(n, &query(InspectionKind::Runs))
        .unwrap();
    assert_eq!(runs.items[0]["outcome"], "ok");
    let page = restored
        .store
        .inspect(n, &query(InspectionKind::Receipts))
        .unwrap();
    assert!(!page.items.is_empty());
    assert!(!contains_secret(&serde_json::to_vec(&page).unwrap()));
    assert!(
        page.items
            .iter()
            .all(|receipt| receipt["detail"].get("input_digest").is_none())
    );
    let effects = restored
        .store
        .inspect(n, &query(InspectionKind::Effects))
        .unwrap();
    assert!(!contains_secret(&serde_json::to_vec(&effects).unwrap()));
    assert!(store().load(n).is_err());
}

fn sensitive_artifact(n: &str) -> Artifact {
    let mut content = artifact(n).content;
    let mut command = boundary(n).intents["intent"].command.clone();
    command.name = "fixture.secret".into();
    let schema = Schema::object(BTreeMap::from([("secret".into(), Schema::string(256))]));
    command.input = schema.clone();
    command.outcomes = BTreeMap::from([("ok".into(), schema.clone())]);
    command.sensitive_input_fields = BTreeSet::from(["secret".into()]);
    command.sensitive_output_fields = BTreeSet::from(["secret".into()]);
    command.public_result_fields.clear();
    let reference = ContractRef {
        name: command.name.clone(),
        fingerprint: command.fingerprint().unwrap(),
    };
    content
        .registry
        .commands
        .insert(command.name.clone(), command);
    content.playbook.steps.insert(
        "invoke".into(),
        Step {
            op: Operation::Invoke {
                command: reference,
                input: Expr::reference("input"),
                bind: "private_result".into(),
                retry: None,
            },
            edges: BTreeMap::from([("ok".into(), "return".into())]),
            on_error: Some("failed".into()),
        },
    );
    content.playbook.steps.insert(
        "failed".into(),
        Step {
            op: Operation::Return {
                outcome: "failed".into(),
                output: Expr::literal(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    let entry = content.playbook.entries.get_mut("main").unwrap();
    entry.start = "invoke".into();
    entry.input = schema;
    entry.outcomes.insert("failed".into(), Schema::Null);
    let artifact = Artifact::seal(content).unwrap();
    artifact.verify().unwrap();
    artifact
}

struct SecretHost {
    calls: usize,
}
impl Host for SecretHost {
    fn authorize(&self, a: &Authority, c: &aq_playbooks_model::CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.calls += 1;
        assert_eq!(i.input["secret"], "SYNTHETIC_PROTECTED_SENTINEL");
        DispatchResult::Completed {
            outcome: "ok".into(),
            output: i.input.clone(),
        }
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.dispatch(i)
    }
}
