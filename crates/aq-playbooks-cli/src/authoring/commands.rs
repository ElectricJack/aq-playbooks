use super::*;

pub(super) fn seal(root: &Path, output: Option<String>) -> Result<(bool, Value)> {
    let (_, artifact) = match sealed(root) {
        Ok(sealed) => sealed,
        Err(refusal) => return Ok(refusal),
    };
    if let Some(output) = &output {
        let target = inside(root, output)?;
        let given = target.strip_prefix(root).unwrap_or(&target);
        anyhow::ensure!(
            !HOST_INPUTS.iter().any(|input| given.starts_with(input)),
            "{output}: seal does not overwrite what the host gave the workspace or what save, ask and unable write ({}); write the artifact to a new name, such as sealed.artifact.json.",
            HOST_INPUTS.join(", ")
        );
        write_inside(root, output, &canonical_bytes(&artifact)?)?;
    }
    let registry = &artifact.content.registry;
    Ok((
        true,
        json!({
            "artifact": artifact.digest,
            "executable_fingerprint": artifact.executable_fingerprint,
            "required_capabilities": artifact.content.playbook.required_capabilities,
            "commands": registry.commands.keys().collect::<Vec<_>>(),
            "events": registry.events.keys().collect::<Vec<_>>(),
            "written": output,
        }),
    ))
}

pub(super) fn try_case(root: &Path, case: String) -> Result<(bool, Value)> {
    let parsed = read_case(root, &case)?;
    let (_, artifact) = match sealed(root) {
        Ok(sealed) => sealed,
        Err(refusal) => return Ok(refusal),
    };
    let report = run_case(&artifact, &parsed)?;
    Ok((report.passed, serde_json::to_value(report)?))
}

pub(super) fn compare(root: &Path) -> Result<(bool, Value)> {
    let (_, next) = match sealed(root) {
        Ok(sealed) => sealed,
        Err(refusal) => return Ok(refusal),
    };
    if !exists(root, PREVIOUS_FILE) {
        return Ok((true, json!({"previous": null})));
    }
    let previous: Artifact = parse_json(
        PREVIOUS_FILE,
        &read_inside(root, PREVIOUS_FILE, MAX_ARTIFACT_BYTES)?,
    )?;
    previous
        .verify()
        .map_err(|e| anyhow!("{PREVIOUS_FILE}: is not a sound artifact: {e}"))?;
    Ok((
        true,
        serde_json::to_value(draft::compare(&previous, &next)?)?,
    ))
}

pub(super) fn facts(root: &Path) -> Result<(bool, Value)> {
    let record: SourceRecord = parse_json(
        RECORD_FILE,
        &read_inside(root, RECORD_FILE, MAX_RECORD_BYTES)?,
    )?;
    let facts: BTreeMap<String, Value> =
        parse_json(FACTS_FILE, &read_inside(root, FACTS_FILE, MAX_FACTS_BYTES)?)?;
    Ok((
        true,
        json!({
            "facts_version": record.facts_version,
            "digest": facts_digest(&facts)?,
            "facts": facts,
            "use": "Read a fact in the draft as {\"op\": \"ref\", \"root\": \"definitions\", \"path\": [\"<name>\", …]}.",
        }),
    ))
}
