use super::*;
pub(super) fn save(root: &Path) -> Result<(bool, Value)> {
    not_ended(root)?;
    let (ws, artifact) = match sealed(root) {
        Ok(sealed) => sealed,
        Err((_, check)) => {
            return refused(
                vec!["the draft does not check; the faults are under `diagnostics`.".into()],
                check,
            );
        }
    };
    let (cases, reports, reasons) = verify_cases(root, &ws, &artifact);
    if !reasons.is_empty() {
        return refused(reasons, json!({"cases": reports}));
    }
    let outcome = Outcome::Saved {
        draft: ws.draft.clone(),
        cases,
        artifact: artifact.digest.clone(),
        facts_digest: facts_digest(&ws.facts)?,
    };
    let bytes = match outcome.to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => return refused(vec![format!("{e:#}")], json!({})),
    };
    write_inside(root, OUTCOME_FILE, &bytes)?;
    let table: Vec<Value> = reports
        .iter()
        .map(|r| json!({"name": r.name, "situation": r.situation, "outcome": r.outcome}))
        .collect();
    Ok((
        true,
        json!({
            "saved": true,
            "written": OUTCOME_FILE,
            "artifact": artifact.digest,
            "facts_digest": facts_digest(&ws.facts)?,
            "cases": table,
            "next": FINISHED,
        }),
    ))
}

fn refused(reasons: Vec<String>, extra: Value) -> Result<(bool, Value)> {
    let mut value = json!({
        "saved": false,
        "reasons": reasons,
        "next": "Nothing was written. Fix each reason, check and try again, then run `aq-playbooks save` again.",
    });
    if let (Some(value), Value::Object(extra)) = (value.as_object_mut(), extra) {
        value.extend(extra);
    }
    Ok((false, value))
}
fn verify_cases(
    root: &Path,
    ws: &Workspace,
    artifact: &Artifact,
) -> (Vec<Case>, Vec<CaseReport>, Vec<String>) {
    let mut reasons = vec![];
    let needed = &artifact.content.playbook.required_capabilities;
    let missing: Vec<&String> = needed.difference(&ws.record.capabilities).collect();
    if !missing.is_empty() {
        reasons.push(format!(
            "the playbook needs the capabilities {missing:?}, which record.json does not give this source ({:?}). Use only commands whose capabilities it gives.",
            ws.record.capabilities
        ));
    }
    let (files, cases) = match read_cases(root) {
        Ok(read) => read,
        Err(faults) => {
            reasons.extend(faults);
            (vec![], vec![])
        }
    };
    let mut reports: Vec<CaseReport> = vec![];
    if reasons.is_empty() {
        match run_cases(artifact, &cases, CASES_TRANSITION_BUDGET) {
            Ok(done) => reports = done,
            Err(e) => reasons.push(format!("{e:#}")),
        }
    }
    reasons.extend(
        files
            .iter()
            .zip(&reports)
            .filter(|(_, report)| !report.passed)
            .map(|(file, report)| {
                format!(
                    "case {:?} ({file}) does not run as it says: {}",
                    report.name,
                    report.problems.join(" ")
                )
            }),
    );
    (cases, reports, reasons)
}

/// Every `cases/*.json`, by file name, or every fault found reading them.
fn read_cases(root: &Path) -> std::result::Result<(Vec<String>, Vec<Case>), Vec<String>> {
    let dir = inside(root, CASES_DIR).map_err(|e| vec![format!("{e:#}")])?;
    let mut names = vec![];
    match fs::symlink_metadata(&dir) {
        Err(_) => {}
        Ok(meta) if !meta.is_dir() => {
            return Err(vec![format!(
                "{CASES_DIR} is not a directory; put each case in its own file, {CASES_DIR}/<name>.json."
            )]);
        }
        Ok(_) => {
            names = directory_names(&dir)?;
        }
    }
    names.sort();
    if !(MIN_CASES..=MAX_CASES).contains(&names.len()) {
        return Err(vec![format!(
            "{CASES_DIR}/ holds {} case files (*.json); a playbook is saved with at least {MIN_CASES} cases and at most {MAX_CASES}. Write a case for each situation the document distinguishes.",
            names.len()
        )]);
    }
    let mut cases = vec![];
    let mut faults = vec![];
    for name in &names {
        match read_case(root, name) {
            Ok(case) => cases.push(case),
            Err(e) => faults.push(format!("{e:#}")),
        }
    }
    if faults.is_empty() {
        Ok((names, cases))
    } else {
        Err(faults)
    }
}

fn directory_names(dir: &Path) -> std::result::Result<Vec<String>, Vec<String>> {
    let mut names = vec![];
    let entries =
        fs::read_dir(dir).map_err(|e| vec![format!("{CASES_DIR}/: cannot be read ({e}).")])?;
    for entry in entries {
        let entry = entry.map_err(|e| vec![format!("{CASES_DIR}/: cannot be read ({e}).")])?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") {
            names.push(format!("{CASES_DIR}/{name}"));
        }
    }
    Ok(names)
}
