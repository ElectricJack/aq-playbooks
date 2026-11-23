use super::*;

#[test]
fn unknown_fields_are_still_refused_with_a_draft_position() {
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-unknown", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = workspace();
    let mut text = serde_json::to_string_pretty(&ws.draft).unwrap();
    write_workspace_inputs(&dir, &ws);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let read = Workspace::read(&dir).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(read, ws);
    // "stepz" at top level.
    text = text.replacen("\"steps\"", "\"stepz\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(d.code, "draft_format");
    assert_eq!(d.path, "draft.json:stepz");
    let line = text.lines().position(|l| l.contains("\"stepz\"")).unwrap() as u32 + 1;
    assert_eq!(d.line, Some(line));
    assert!(d.column.is_some());
    assert!(d.message.contains("unknown field `stepz`"), "{}", d.message);
    assert!(
        d.message.starts_with(&format!("draft.json line {line}")),
        "{}",
        d.message
    );
    check_nested_positions(&dir, &ws.draft);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_refined_path_names_only_the_object_serde_complained_about() {
    // A3 re-review I2, probe 1: two faulty expressions in one operation, the
    // one serde reports (`zz`, first in the file) after the other in key order.
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-refined", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = workspace();
    write_workspace_inputs(&dir, &ws);
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"fields\": {",
        "\"fields\": {\"zz\": {\"op\": \"literal\", \"valu\": 1},\n \"aa\": {\"op\": \"ref\", \"rooot\": \"input\"},",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert!(d.message.contains("unknown field `valu`"), "{d:#?}");
    assert_eq!(d.path, "draft.json:steps.read.op.input.fields.zz", "{d:#?}");
    let line = text.lines().position(|l| l.contains("\"zz\"")).unwrap() as u32 + 1;
    assert_eq!(d.line, Some(line), "{d:#?}");
    std::fs::remove_dir_all(&dir).unwrap();
    // Probe 2: literal data that has a `type` key is data, never a schema.
    let mut ws = workspace();
    ws.draft["steps"]["done"]["op"]["output"] =
        json!({"extra": 1, "op": "literal", "value": {"type": "user"}});
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.done.op.output",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `extra`"),
        "{diagnostics:#?}"
    );
    // A lookup case's `when` is data too.
    let mut ws = workspace();
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["cases"][0]["when"] =
        json!({"op": "nonsense"});
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["cases"][0]["thn"] = json!(1);
    let diagnostics = check(&ws);
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.route.op.input.fields.level",
        "{diagnostics:#?}"
    );
}

fn check_nested_positions(dir: &std::path::Path, draft: &Value) {
    // Deeper, the JSON path names the step.
    let text = serde_json::to_string_pretty(draft).unwrap().replacen(
        "\"on_error\": \"failed\"",
        "\"on_eror\": \"failed\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.on_eror",
        "{diagnostics:#?}"
    );
    // Inside an operation, which serde reads whole and reports at the end of
    // the step, the operation is named, at its own line.
    let text = serde_json::to_string_pretty(draft).unwrap().replacen(
        "\"command\": \"work.read\"",
        "\"comand\": \"work.read\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.op",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `comand`"),
        "{diagnostics:#?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let comand = lines.iter().position(|l| l.contains("\"comand\"")).unwrap();
    let op = lines[..comand]
        .iter()
        .rposition(|l| l.trim() == "\"op\": {")
        .unwrap() as u32
        + 1;
    assert_eq!(diagnostics[0].line, Some(op), "{diagnostics:#?}");
    // And inside an expression, the expression.
    let text =
        serde_json::to_string_pretty(draft)
            .unwrap()
            .replacen("\"default\"", "\"defualt\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.route.op.input.fields.level",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("`defualt`"),
        "{diagnostics:#?}"
    );
}
