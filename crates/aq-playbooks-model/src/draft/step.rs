//! What each operation binds on each outgoing edge, including errors and timeouts.
use super::*;

struct StepBindings<'a> {
    artifact: &'a ArtifactContent,
    sources: &'a Sources,
    name: &'a str,
    step: &'a Step,
}

impl ArtifactContent {
    pub(super) fn parts<'a>(
        &'a self,
        sources: &'a Sources,
        name: &'a str,
        step: &'a Step,
    ) -> Parts<'a> {
        StepBindings {
            artifact: self,
            sources,
            name,
            step,
        }
        .parts()
    }
}

impl<'a> StepBindings<'a> {
    fn parts(&self) -> Parts<'a> {
        let (mut exits, bind, own_item) = match &self.step.op {
            Operation::Invoke { command, bind, .. } => {
                (self.invoke(command, bind), Some(bind.as_str()), None)
            }
            Operation::Decide { .. } | Operation::Return { .. } => {
                let exits =
                    self.edges(|edge| (format!("edge `{edge}` of step `{}`", self.name), None));
                (exits, None, None)
            }
            Operation::Wait { event, bind, .. } => {
                (self.wait(event.as_ref(), bind), Some(bind.as_str()), None)
            }
            Operation::ForEach {
                collection,
                item,
                body,
                next,
                ..
            } => {
                let exits = self.loop_edges(collection, item, (body, next));
                (exits, Some(item.as_str()), Some(item.as_str()))
            }
            Operation::Call {
                dependency,
                entry,
                mode,
                bind,
                ..
            } => {
                let exits = self.call_edges((dependency, entry), mode, bind);
                (exits, Some(bind.as_str()), None)
            }
            Operation::Join {
                mode,
                accepted_outcomes,
                member_keys,
                bind,
                ..
            } => {
                let shape = self.join_shape(mode, accepted_outcomes, member_keys);
                let exits = self.edges(|edge| {
                    (
                        format!("edge `{edge}` of join `{}`", self.name),
                        Some((
                            bind,
                            shape.as_ref().map_or(Held::Unfollowed, |s| s.members(edge)),
                        )),
                    )
                });
                (exits, Some(bind.as_str()), None)
            }
        };
        if let Some(to) = self.step.on_error.as_deref() {
            exits.push(Exit {
                to,
                way: format!("the `on_error` exit of step `{}`", self.name),
                holds: bind.map(|b| (b, Held::Unbound)),
            });
        }
        Parts {
            exprs: self.step.op.expressions(),
            exits,
            own_item,
        }
    }

    fn loop_edges(
        &self,
        collection: &'a Expr,
        item: &'a str,
        (body, next): (&'a str, &'a str),
    ) -> Vec<Exit<'a>> {
        let mut exits = vec![
            Exit {
                to: body,
                way: format!("the body of for_each `{}`", self.name),
                holds: Some((item, Held::ItemOf(collection))),
            },
            Exit {
                to: next,
                way: format!("the `next` exit of for_each `{}`", self.name),
                holds: Some((item, Held::Unbound)),
            },
        ];
        exits.extend(self.edges(|edge| {
            (
                format!("edge `{edge}` of for_each `{}`", self.name),
                Some((item, Held::Unbound)),
            )
        }));
        exits
    }

    fn call_edges(
        &self,
        (dependency, entry): (&str, &str),
        mode: &CallMode,
        bind: &'a str,
    ) -> Vec<Exit<'a>> {
        let interface = self
            .artifact
            .dependencies
            .get(dependency)
            .and_then(|d| d.interfaces.get(entry));
        let exits = self.outcomes(bind, |edge| match mode {
            CallMode::Wait => interface
                .and_then(|i| i.outcomes.get(edge))
                .map_or(Held::Unfollowed, Held::Shape),
            CallMode::OwnedAsync | CallMode::Independent => Held::Shape(&self.sources.child),
        });
        exits
    }

    fn edges(
        &self,
        values: impl Fn(&'a str) -> (String, Option<(&'a str, Held<'a>)>),
    ) -> Vec<Exit<'a>> {
        self.step
            .edges
            .iter()
            .map(|(edge, to)| {
                let (way, holds) = values(edge);
                Exit { to, way, holds }
            })
            .collect()
    }

    fn outcomes(&self, bind: &'a str, value: impl Fn(&'a str) -> Held<'a>) -> Vec<Exit<'a>> {
        self.edges(|edge| {
            (
                format!("outcome `{edge}` of step `{}`", self.name),
                Some((bind, value(edge))),
            )
        })
    }

    fn invoke(&self, command: &ContractRef, bind: &'a str) -> Vec<Exit<'a>> {
        let contract = self.artifact.registry.commands.get(&command.name);
        self.outcomes(bind, |edge| {
            contract
                .and_then(|c| c.outcomes.get(edge))
                .map_or(Held::Unfollowed, Held::Shape)
        })
    }

    fn wait(&self, event: Option<&ContractRef>, bind: &'a str) -> Vec<Exit<'a>> {
        let payload = event
            .and_then(|e| self.artifact.registry.events.get(&e.name))
            .map(|e| &e.payload);
        self.edges(|edge| {
            let held = match (event, edge) {
                (Some(_), "received") => payload.map_or(Held::Unfollowed, Held::Shape),
                (Some(_) | None, _) => Held::Unbound,
            };
            (
                format!("edge `{edge}` of wait `{}`", self.name),
                Some((bind, held)),
            )
        })
    }

    fn join_shape(
        &self,
        mode: &'a JoinMode,
        accepted: &'a BTreeSet<String>,
        keys: &'a [String],
    ) -> Option<JoinShape<'a>> {
        Some(JoinShape {
            mode,
            accepted,
            keys,
            loose: self.sources.loose.get(self.name)?,
            outcomes: self.sources.outcomes.get(self.name)?,
        })
    }
}

struct JoinShape<'a> {
    mode: &'a JoinMode,
    accepted: &'a BTreeSet<String>,
    keys: &'a [String],
    loose: &'a BTreeMap<String, Schema>,
    outcomes: &'a [BTreeMap<String, Schema>],
}
impl<'a> JoinShape<'a> {
    fn members(&self, edge: &str) -> Held<'a> {
        let several = self.keys.len() > 1;
        let mut held = BTreeMap::new();
        for (key, outcomes) in self.keys.iter().zip(self.outcomes) {
            let Some(any) = self.loose.get(key) else {
                return Held::Unfollowed;
            };
            let qualified = || {
                outcomes
                    .iter()
                    .filter(|(o, _)| self.accepted.is_empty() || self.accepted.contains(*o))
                    .map(|(o, schema)| Member::Answer(o, schema))
                    .collect::<Vec<_>>()
            };
            let unanswered = Member::Absent("when that member has not answered");
            let none = vec![Member::Absent("where no member is kept")];
            let members = match (self.mode, edge) {
                (JoinMode::All, "completed") => qualified(),
                (JoinMode::All, "no_match") => vec![Member::Loose(any)],
                (JoinMode::AnyAccepted, "completed") => {
                    let mut members = qualified();
                    // With one member, that member is the one that qualified.
                    if several {
                        members.push(Member::Absent("when another member qualified first"));
                    }
                    members
                }
                (JoinMode::AnyCompletion, "completed") => {
                    let mut members = vec![Member::Loose(any)];
                    // With one member, that member is the one that completed.
                    if several {
                        members.push(Member::Absent("when another member completed first"));
                    }
                    members
                }
                (JoinMode::All | JoinMode::AnyAccepted | JoinMode::AnyCompletion, "timeout") => {
                    vec![Member::Loose(any), unanswered]
                }
                (JoinMode::All | JoinMode::AnyAccepted | JoinMode::AnyCompletion, _) => none,
            };
            held.insert(key.as_str(), members);
        }
        Held::Members(held)
    }
}
