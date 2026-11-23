//! Structural checks on the command and event schemas captured in an artifact.
use super::*;

pub(super) fn validate(registry: &Registry, issues: &mut Vec<ValidationIssue>) {
    for (name, command) in &registry.commands {
        let mut check = ChecksAt::member("commands", name, issues);
        check.command(name, command);
        if let Some(asynchronous) = &command.asynchronous {
            check.async_contract(command, asynchronous, registry);
        }
        check.projections(command);
    }
    for (name, event) in &registry.events {
        ChecksAt::member("events", name, issues).event(name, event);
    }
}

fn registered_name(name: &str, declared: &str) -> bool {
    name == declared && valid_name(name)
}

impl<'a> ChecksAt<'a> {
    pub(super) fn member(group: &str, name: &str, issues: &'a mut Vec<ValidationIssue>) -> Self {
        Self {
            path: format!("{group}.{name}"),
            issues,
        }
    }
}
impl ChecksAt<'_> {
    fn command(&mut self, name: &str, command: &CommandContract) {
        let valid_identity = registered_name(name, &command.name);
        self.require(
            valid_identity && (1..=64).contains(&command.version.len()),
            "registry_identity",
            "invalid command name/version",
        );
        self.schema(&command.input);
        self.require(
            (1..=64).contains(&command.outcomes.len()),
            "outcomes",
            "one to 64 command outcomes required",
        );
        for schema in command.outcomes.values() {
            self.schema(schema);
        }
        self.command_limits(command);
    }

    fn command_limits(&mut self, c: &CommandContract) {
        let bounded = (1..=16_777_216).contains(&c.max_payload_bytes)
            && (1..=31_536_000_000).contains(&c.deadline_ms);
        self.require(
            bounded && !c.scopes.is_empty(),
            "contract_limits",
            "command requires bounded scope, payload, deadline",
        );
        self.require(
            c.effect != Effect::Mutation || c.recovery != Recovery::Observation,
            "recovery",
            "mutation cannot use observation recovery",
        );
    }
    fn async_contract(
        &mut self,
        command: &CommandContract,
        contract: &AsyncContract,
        registry: &Registry,
    ) {
        self.schema(&contract.completion);
        self.require(completion_valid(&contract.completion,command) && admission_valid(command), "async_contract","async contract requires accepted {handle:string} and completion {outcome:declared business enum,output}");
        let registrations_present = registry
            .commands
            .contains_key(&contract.reconciliation_command)
            && !contract.correlation_field.is_empty();
        let event = registry
            .events
            .get(&contract.completion_event)
            .filter(|_| registrations_present);
        let Some(event) = event else {
            self.report(
                "async_contract",
                "async completion/reconciliation registrations required",
            );
            return;
        };
        self.require(
            event
                .correlation_fields
                .contains(&contract.correlation_field)
                && contract.completion.assignable_to(&event.payload),
            "async_contract",
            "async completion schema/correlation incompatible with event",
        );
    }
    fn projections(&mut self, c: &CommandContract) {
        let in_output = |field: &str| {
            c.outcomes
                .values()
                .any(|schema| schema_has_field(schema, field))
        };
        for field in &c.public_result_fields {
            self.require(
                in_output(field),
                "public_projection",
                "public fields must exist in at least one declared outcome object",
            );
        }
        for field in &c.sensitive_output_fields {
            let protected = !c.public_result_fields.contains(field) && in_output(field);
            self.require(protected, "sensitive_field", "protected output fields must exist in a declared outcome and cannot be public projection fields");
        }
        for field in &c.sensitive_input_fields {
            self.require(
                schema_has_field(&c.input, field),
                "sensitive_field",
                "sensitive input field must exist in input object",
            );
        }
    }
    fn event(&mut self, name: &str, e: &EventContract) {
        self.require(
            registered_event(name, e),
            "event_contract",
            "invalid event identity, producer, scope or resource bound",
        );
        self.schema(&e.payload);
        let known_fields = e
            .sensitive_payload_fields
            .iter()
            .all(|field| schema_has_field(&e.payload, field));
        self.require(
            known_fields,
            "sensitive_field",
            "sensitive event payload fields must exist in the declared payload schema",
        );
    }
}

fn completion_valid(schema: &Schema, command: &CommandContract) -> bool {
    let Schema::Object {
        properties,
        required,
    } = schema
    else {
        return false;
    };
    let fields_required = ["outcome", "output"]
        .iter()
        .all(|field| required.contains(*field));
    let fields_present = properties.contains_key("output")
        && valid_completion_outcomes(properties.get("outcome"), command);
    fields_required && fields_present
}

fn valid_completion_outcomes(schema: Option<&Schema>, command: &CommandContract) -> bool {
    let Some(Schema::String { values, .. }) = schema else {
        return false;
    };
    !values.is_empty()
        && values
            .iter()
            .all(|v| v != "accepted" && command.outcomes.contains_key(v))
}

fn admission_valid(command: &CommandContract) -> bool {
    let Some(Schema::Object {
        properties,
        required,
    }) = command.outcomes.get("accepted")
    else {
        return false;
    };
    let requires_handle = required.len() == 1 && required.contains("handle");
    requires_handle && matches!(properties.get("handle"), Some(Schema::String { .. }))
}

fn registered_event(name: &str, event: &EventContract) -> bool {
    let identity = registered_name(name, &event.name);
    let authority = !event.trusted_producers.is_empty() && !event.scopes.is_empty();
    let bounded = (1..=16_777_216).contains(&event.max_payload_bytes) && event.retention_ms > 0;
    let versioned = !event.version.is_empty();
    identity && versioned && authority && bounded
}
