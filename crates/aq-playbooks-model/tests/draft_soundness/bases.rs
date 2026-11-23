use super::*;

struct Fixtures(Registry);

#[derive(Clone, Copy)]
enum Expected {
    Present(&'static str),
    MissingField(&'static str),
    MissingCarrier(&'static str),
}
use Expected::*;
impl Expected {
    fn name(self) -> &'static str {
        match self {
            Present(name) | MissingField(name) | MissingCarrier(name) => name,
        }
    }
}

struct Joined<'a> {
    members: &'a [&'a str],
    mode: JoinMode,
    accepted: &'a [&'a str],
}

const TO_JOIN: &[(&str, &str)] = &[
    ("accepted", "read"),
    ("assessed", "end"),
    ("failed", "end"),
    ("refused", "end"),
];
const ALL_END: &[(&str, &str)] = &[
    ("accepted", "end"),
    ("assessed", "end"),
    ("failed", "end"),
    ("refused", "end"),
];
const TWO_ASKED: &[(&str, &str)] = &[
    ("accepted", "ask2"),
    ("assessed", "end"),
    ("failed", "end"),
    ("refused", "end"),
];

impl Fixtures {
    fn invoke(&self, (command, bind): (&str, &str), to: &[(&str, &str)], on_error: &str) -> Step {
        Step {
            op: Operation::Invoke {
                command: ContractRef {
                    name: command.into(),
                    fingerprint: self.0.commands[command].fingerprint().unwrap(),
                },
                input: Expr::Object {
                    fields: BTreeMap::new(),
                },
                bind: bind.into(),
                retry: None,
            },
            edges: edges(to),
            on_error: Some(on_error.into()),
        }
    }
    fn ask(&self, to: &[(&str, &str)], on_error: &str) -> Step {
        self.invoke(("ask.judge", "j"), to, on_error)
    }
    fn look(&self, to: &[(&str, &str)]) -> Step {
        self.invoke(("work.look", "seen"), to, "end")
    }
}
fn join_of(members: &[&str], mode: JoinMode, accepted: &[&str], to: &[(&str, &str)]) -> Step {
    Step {
        op: Operation::Join {
            members: members.iter().map(|m| r(m, &[])).collect(),
            member_keys: ["c", "d"][..members.len()]
                .iter()
                .map(|k| k.to_string())
                .collect(),
            mode,
            accepted_outcomes: strings(accepted),
            deadline_ms: 60_000,
            remaining: RemainingPolicy::Track,
            bind: "answer".into(),
        },
        edges: edges(to),
        on_error: Some("end".into()),
    }
}

impl Fixtures {
    fn base(
        &self,
        expected: Expected,
        (carrier, path): (&str, &'static [&'static str]),
        (start, steps): (&str, Vec<(&str, Step)>),
    ) -> Base {
        Base {
            name: expected.name(),
            carrier: r(carrier, &[]),
            path,
            sound: matches!(expected, Present(_)),
            carrier_missing: matches!(expected, MissingCarrier(_)),
            needed_inside: false,
            two_causes: false,
            loose_item: carrier == "answer" || carrier == "vars",
            depth: 3,
            sites: IN_STEPS,
            content: playbook(&self.0, start, steps),
        }
    }
    // A base composed two deep: the scope sources and sites beyond the first
    // bases, which are composed three deep.
    fn shallow(mut base: Base) -> Base {
        base.depth = 2;
        base
    }
    // A literal bound to a name: the value an author writes as a table.
    fn literal(&self, expected: Expected, value: Value, path: &'static [&'static str]) -> Base {
        Base {
            carrier: let_("lit", lit(value), r("lit", &[])),
            sites: ANYWHERE,
            ..self.base(expected, ("lit", path), (SLOT, vec![]))
        }
    }
    // A read given whole, over literal rows of which the second lacks `k`.
    fn rows(&self, name: &'static str, read: Expr, needed_inside: bool) -> Base {
        Base {
            carrier: let_("rows", lit(json!([{"k": "chore"}, {}])), read),
            needed_inside,
            depth: 2,
            sites: ANYWHERE,
            ..self.base(MissingCarrier(name), ("rows", &[]), (SLOT, vec![]))
        }
    }
    // `look` runs before the loop and again in its body; its `none` outcome
    // leaves the loop by `noted`, whose failure inside the loop is the
    // loop's to handle.
    fn looped(&self, expected: Expected, on_failure: IterationFailure) -> Base {
        let mut noted = self.look(&[("found", "end"), ("none", "end")]);
        let Operation::Invoke { bind, .. } = &mut noted.op else {
            unreachable!()
        };
        *bind = "seen2".into();
        // The check follows a failure of any step in the body, `end` too,
        // which `look`'s own `on_error` reaches without `seen`: where the
        // loop goes on after a failure, `seen` itself may not be bound.
        let two_causes = matches!(expected, MissingCarrier(_));
        let mut base = Self::shallow(self.base(
            expected,
            ("seen", &["kind"]),
            (
                "look",
                vec![
                    ("look", self.look(&[("found", "each"), ("none", "noted")])),
                    (
                        "each",
                        Step {
                            op: Operation::ForEach {
                                collection: lit(json!(["a"])),
                                item: "it0".into(),
                                body: "again".into(),
                                next: SLOT.into(),
                                max_items: 1,
                                concurrency: 1,
                                on_failure,
                            },
                            edges: BTreeMap::new(),
                            on_error: Some("end".into()),
                        },
                    ),
                    ("again", decide(lit(json!(true)), "look")),
                    ("noted", noted),
                ],
            ),
        ));
        base.two_causes = two_causes;
        base
    }
    fn always_asked(&self) -> Vec<(&'static str, Step)> {
        vec![(
            "ask",
            self.ask(
                &[
                    ("accepted", SLOT),
                    ("assessed", "end"),
                    ("failed", "end"),
                    ("refused", "end"),
                ],
                "end",
            ),
        )]
    }
    fn joined(&self, expected: Expected, join: Joined, edge: &str) -> Base {
        let mut to = vec![
            ("completed", "end"),
            ("no_match", "end"),
            ("timeout", "end"),
        ];
        for exit in &mut to {
            if exit.0 == edge {
                exit.1 = SLOT;
            }
        }
        let mut steps = vec![("read", join_of(join.members, join.mode, join.accepted, &to))];
        if join.members.len() == 2 {
            steps.push(("ask", self.ask(TWO_ASKED, "end")));
            steps.push(("ask2", self.invoke(("ask.judge", "j2"), TO_JOIN, "end")));
        } else {
            steps.push(("ask", self.ask(TO_JOIN, "end")));
        }
        Self::shallow(self.base(expected, ("answer", &["c", "participant"]), ("ask", steps)))
    }
    fn literals(&self) -> Vec<Base> {
        vec![
            // Literals: a table's rows keep their own fields, an array its length.
            self.literal(
                MissingField("a field of a literal row that lacks it"),
                json!([{"k": "chore"}, {}]),
                &["1", "k"],
            ),
            self.literal(
                MissingField("a position past the end of a literal array"),
                json!(["chore", "x"]),
                &["5"],
            ),
            self.literal(
                Present("a field of a literal row that has it"),
                json!([{"k": "chore"}, {}]),
                &["0", "k"],
            ),
            // The reviewer's probes L1 and L1b.
            self.rows(
                "a lookup keyed on a field of every literal row, one of which lacks it",
                Expr::Map {
                    collection: b(r("rows", &[])),
                    item: "row".into(),
                    value: b(lookup(r("row", &["k"]), vec![], lit(json!(true)))),
                    max_items: 2,
                },
                true,
            ),
            self.rows(
                "a filter on a field of every literal row, one of which lacks it",
                Expr::Filter {
                    collection: b(r("rows", &[])),
                    item: "row".into(),
                    predicate: b(eq(r("row", &["k"]), lit(json!("chore")))),
                    max_items: 2,
                },
                false,
            ),
        ]
    }
    fn scope_roots(&self) -> Vec<Base> {
        vec![
            // The other roots.
            Self::shallow(self.base(
                MissingField("a binding not bound on some way, read through `vars`"),
                ("vars", &["j", "handle"]),
                ("ask", vec![("ask", self.ask(ALL_END, SLOT))]),
            )),
            Self::shallow(self.base(
                Present("a field every way has, read through `vars`"),
                ("vars", &["j", "handle"]),
                ("ask", self.always_asked()),
            )),
            Self::shallow(self.base(
                Present("a captured definition"),
                ("definitions", &["levels", "0"]),
                (SLOT, vec![]),
            )),
            Base {
                sites: &[Site::Trigger],
                ..Self::shallow(self.base(
                    Present("a field of the trigger's event"),
                    ("event", &["note"]),
                    (SLOT, vec![]),
                ))
            },
        ]
    }
    fn joins(&self) -> Vec<Base> {
        vec![
            // Joins with several members, and `no_match`.
            self.joined(
                MissingField("a member another member can qualify before, in `any_accepted`"),
                Joined {
                    members: &["j", "j2"],
                    mode: JoinMode::AnyAccepted,
                    accepted: &["assessed"],
                },
                "completed",
            ),
            self.joined(
                MissingField("a member another member can complete before, in `any_completion`"),
                Joined {
                    members: &["j", "j2"],
                    mode: JoinMode::AnyCompletion,
                    accepted: &[],
                },
                "completed",
            ),
            self.joined(
                Present("a member on `no_match` of an `all` join"),
                Joined {
                    members: &["j"],
                    mode: JoinMode::All,
                    accepted: &["assessed"],
                },
                "no_match",
            ),
            self.joined(
                MissingField("a member on `no_match` of an `any_accepted` join"),
                Joined {
                    members: &["j"],
                    mode: JoinMode::AnyAccepted,
                    accepted: &["assessed"],
                },
                "no_match",
            ),
        ]
    }
    fn loop_failures(&self) -> Vec<Base> {
        vec![
            // A failure in a loop body.
            self.looped(
                MissingCarrier(
                    "an outcome that reaches the end of a loop only by a failure in its body",
                ),
                IterationFailure::Continue,
            ),
            self.looped(
                Present("an outcome a failure in a loop body takes out of the loop"),
                IterationFailure::Stop,
            ),
        ]
    }
    fn outcome_edges(&self) -> Vec<Base> {
        vec![
            self.base(
                MissingField("an outcome-only field where two outcome edges meet"),
                ("j", &["choice"]),
                (
                    "ask",
                    vec![(
                        "ask",
                        self.ask(
                            &[
                                ("accepted", "end"),
                                ("assessed", SLOT),
                                ("failed", SLOT),
                                ("refused", "end"),
                            ],
                            "end",
                        ),
                    )],
                ),
            ),
            self.base(
                MissingField("a join member's field on the timeout edge"),
                ("answer", &["c", "participant"]),
                (
                    "ask",
                    vec![
                        ("ask", self.ask(TO_JOIN, "end")),
                        (
                            "read",
                            join_of(
                                &["j"],
                                JoinMode::All,
                                &["assessed"],
                                &[("completed", "end"), ("no_match", "end"), ("timeout", SLOT)],
                            ),
                        ),
                    ],
                ),
            ),
            self.base(
                MissingField("a field only one accepted outcome of a join has"),
                ("answer", &["c", "output", "choice"]),
                (
                    "ask",
                    vec![
                        ("ask", self.ask(TO_JOIN, "end")),
                        (
                            "read",
                            join_of(
                                &["j"],
                                JoinMode::All,
                                &[],
                                &[("completed", SLOT), ("no_match", "end"), ("timeout", "end")],
                            ),
                        ),
                    ],
                ),
            ),
        ]
    }
    fn missing_bindings(&self) -> Vec<Base> {
        vec![
            self.base(
                MissingCarrier("a wait's binding on its timeout edge"),
                ("w", &["note"]),
                (
                    "hold",
                    vec![(
                        "hold",
                        Step {
                            op: Operation::Wait {
                                event: Some(ContractRef {
                                    name: "aq.work.seen".into(),
                                    fingerprint: self.0.events["aq.work.seen"]
                                        .fingerprint()
                                        .unwrap(),
                                }),
                                correlation: BTreeMap::new(),
                                minimum_revision: None,
                                deadline_ms: 1000,
                                bind: "w".into(),
                            },
                            edges: edges(&[("received", "end"), ("timeout", SLOT)]),
                            on_error: Some("end".into()),
                        },
                    )],
                ),
            ),
            self.base(
                MissingCarrier("a binding not bound on its step's on_error exit"),
                ("j", &["handle"]),
                ("ask", vec![("ask", self.ask(ALL_END, SLOT))]),
            ),
            self.binding_around_step(),
        ]
    }
    fn binding_around_step(&self) -> Base {
        self.base(
            MissingCarrier("a binding not bound on a way around its step"),
            ("j", &["handle"]),
            (
                "gate",
                vec![
                    (
                        "gate",
                        Step {
                            op: Operation::Decide {
                                predicate: lit(json!(true)),
                            },
                            edges: edges(&[("true", "ask"), ("false", SLOT), ("unknown", "end")]),
                            on_error: None,
                        },
                    ),
                    (
                        "ask",
                        self.ask(
                            &[
                                ("accepted", SLOT),
                                ("assessed", "end"),
                                ("failed", "end"),
                                ("refused", "end"),
                            ],
                            "end",
                        ),
                    ),
                ],
            ),
        )
    }
    fn loop_body(&self) -> Vec<Base> {
        vec![self.base(
            MissingCarrier("a for_each body's binding read after the loop"),
            ("seen", &["id"]),
            (
                "each",
                vec![
                    (
                        "each",
                        Step {
                            op: Operation::ForEach {
                                collection: lit(json!(["a"])),
                                item: "it0".into(),
                                body: "look".into(),
                                next: SLOT.into(),
                                max_items: 1,
                                concurrency: 1,
                                on_failure: IterationFailure::Stop,
                            },
                            edges: BTreeMap::new(),
                            on_error: Some("end".into()),
                        },
                    ),
                    ("look", self.look(&[("found", "each"), ("none", "each")])),
                ],
            ),
        )]
    }
    fn present_fields(&self) -> Vec<Base> {
        vec![
            self.base(
                Present("a field every way has"),
                ("j", &["handle"]),
                (
                    "ask",
                    vec![(
                        "ask",
                        self.ask(
                            &[
                                ("accepted", SLOT),
                                ("assessed", "end"),
                                ("failed", "end"),
                                ("refused", "end"),
                            ],
                            "end",
                        ),
                    )],
                ),
            ),
            self.base(
                Present("a field optional in its one schema"),
                ("seen", &["hint"]),
                (
                    "look",
                    vec![("look", self.look(&[("found", SLOT), ("none", "end")]))],
                ),
            ),
        ]
    }
}

pub(super) fn bases() -> Vec<Base> {
    let fixtures = Fixtures(registry());
    let mut all = vec![];
    all.extend(fixtures.literals());
    all.extend(fixtures.scope_roots());
    all.extend(fixtures.joins());
    all.extend(fixtures.loop_failures());
    all.extend(fixtures.outcome_edges());
    all.extend(fixtures.missing_bindings());
    all.extend(fixtures.loop_body());
    all.extend(fixtures.present_fields());
    all
}
