//! Resolve asynchronous join handles and their completion schemas.
use super::*;

impl ArtifactContent {
    /// A join names an operation handle by the binding of the invoke that
    /// admitted it, as the runner resolves members (`{"handle": h}`). Returns
    /// the fix when `member` reads a path inside such a binding instead.
    pub(super) fn handle_member_path(&self, member: &Expr) -> Option<String> {
        let Expr::Ref { root, path } = member else {
            return None;
        };
        let (binding, rest) = match (root.as_str(), path.split_first()) {
            ("vars", Some((binding, rest))) => (binding, rest),
            ("vars", None) => return None,
            _ => (root, path.as_slice()),
        };
        if rest.is_empty() {
            return None;
        }
        let (step, command) = self
            .playbook
            .steps
            .iter()
            .find_map(|(name, s)| match &s.op {
                Operation::Invoke { command, bind, .. } if bind == binding => {
                    Some((name, &command.name))
                }
                _ => None,
            })?;
        let written = dotted(root, path);
        if self
            .registry
            .commands
            .get(command)
            .is_some_and(|c| c.asynchronous.is_none())
        {
            return Some(format!(
                "`{written}` is a result of step `{step}`, whose command {command} is \
                 synchronous; only the handle of an asynchronous command can be joined. Use \
                 the result directly on the step's outcome edges instead of joining it"
            ));
        }
        Some(format!(
            "`{written}` reads inside the operation handle bound by step `{step}` ({command}); \
             a join names the handle by its binding alone: write \
             {{\"op\":\"ref\",\"root\":\"{binding}\",\"path\":[]}}"
        ))
    }
    pub(super) fn member_outcomes(
        &self,
        member: &Expr,
    ) -> Result<BTreeMap<String, Schema>, String> {
        let (binding, path) = member_binding(member)?;
        let matches =
            |bind: &str, field: &str| bind == binding && (path.is_empty() || path == [field]);
        for step in self.playbook.steps.values() {
            match &step.op {
                Operation::Call {
                    dependency,
                    entry,
                    mode: CallMode::OwnedAsync,
                    bind,
                    ..
                } if matches(bind, "child") => {
                    return self
                        .dependencies
                        .get(dependency)
                        .and_then(|d| d.interfaces.get(entry))
                        .map(|entry| entry.outcomes.clone())
                        .ok_or_else(|| "join references unknown child interface".into());
                }
                // Stored artifacts may name the handle field. Drafts and the runner
                // require the handle binding itself.
                Operation::Invoke { command, bind, .. } if matches(bind, "handle") => {
                    return self.completion_outcomes(command);
                }
                _ => {}
            }
        }
        Err(format!(
            "join member {binding} is not a statically declared owned asynchronous handle"
        ))
    }

    fn completion_outcomes(
        &self,
        reference: &ContractRef,
    ) -> Result<BTreeMap<String, Schema>, String> {
        let command = self
            .registry
            .commands
            .get(&reference.name)
            .ok_or("join references unknown command")?;
        let asynchronous = command
            .asynchronous
            .as_ref()
            .ok_or("join operation must have asynchronous contract")?;
        let Schema::Object { properties, .. } = &asynchronous.completion else {
            return Err("async completion must be object".into());
        };
        let Some(Schema::String { values, .. }) = properties.get("outcome") else {
            return Err("async completion requires outcome enum".into());
        };
        Ok(command
            .outcomes
            .iter()
            .filter(|(outcome, _)| values.contains(*outcome))
            .map(|(name, schema)| (name.clone(), schema.clone()))
            .collect())
    }
    pub(super) fn join_schema(
        &self,
        step: &Step,
        completed: bool,
    ) -> Result<Option<Schema>, String> {
        let Operation::Join {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            ..
        } = &step.op
        else {
            return Ok(None);
        };
        if member_keys.is_empty() {
            return Ok(None);
        }
        if member_keys.len() != members.len() {
            return Err("join key/member cardinality mismatch".into());
        }
        let guaranteed_business = completed && *mode != JoinMode::AnyCompletion;
        let mut properties = BTreeMap::new();
        for (i, (key, member)) in member_keys.iter().zip(members).enumerate() {
            let outcomes = self
                .member_outcomes(member)
                .map_err(|e| format!("members[{i}] (key {key}): {e}"))?;
            if let Some(member) = member_result(outcomes, accepted_outcomes, guaranteed_business)? {
                properties.insert(key.clone(), member);
            }
        }
        let required = if guaranteed_business && *mode == JoinMode::All {
            properties.keys().cloned().collect()
        } else {
            BTreeSet::new()
        };
        Ok(Some(Schema::Object {
            properties,
            required,
        }))
    }
}

fn member_binding(member: &Expr) -> Result<(&str, &[String]), String> {
    let Expr::Ref { root, path } = member else {
        return Err("named join members must reference statically declared owned child or operation handles".into());
    };
    let (binding, path) = if root == "vars" {
        let Some((binding, path)) = path.split_first() else {
            return Err("join member vars reference needs a handle name".into());
        };
        (binding.as_str(), path)
    } else {
        (root.as_str(), path.as_slice())
    };
    Ok((binding, path))
}

fn member_result(
    mut outcomes: BTreeMap<String, Schema>,
    accepted: &BTreeSet<String>,
    guaranteed_business: bool,
) -> Result<Option<Schema>, String> {
    if guaranteed_business && !accepted.is_empty() {
        outcomes.retain(|outcome, _| accepted.contains(outcome));
    }
    // In AnyAccepted a member with no qualifying outcome has no result key.
    if guaranteed_business && outcomes.is_empty() {
        return Ok(None);
    }
    let output =
        union_schemas(outcomes.values()).ok_or("join member result exceeds bounded union limit")?;
    let outcome = Schema::String {
        max_length: outcomes.keys().map(String::len).max().unwrap_or(1).max(1),
        values: outcomes.into_keys().collect(),
    };
    let value = |schema| {
        if guaranteed_business {
            schema
        } else {
            Schema::Nullable {
                inner: Box::new(schema),
            }
        }
    };
    Ok(Some(Schema::object(BTreeMap::from([
        ("participant".into(), Schema::string(128)),
        ("outcome".into(), value(outcome)),
        ("output".into(), value(output)),
    ]))))
}
