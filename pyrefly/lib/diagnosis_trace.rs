/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use pyrefly_python::module::Module;
use ruff_text_size::TextRange;
use serde::Serialize;

use crate::error::context::TypeCheckKind;
use crate::error::error::Error;

static TRACE: OnceLock<Mutex<Option<JsonlSink>>> = OnceLock::new();
static ASSUMPTION_ID: AtomicUsize = AtomicUsize::new(1);
static OBLIGATION_ID: AtomicUsize = AtomicUsize::new(1);
static DIAGNOSTIC_ID: AtomicUsize = AtomicUsize::new(1);

struct JsonlSink {
    writer: BufWriter<File>,
}

#[derive(Serialize)]
struct TraceRange {
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

#[derive(Serialize)]
struct TraceSource {
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
struct AssumptionEvent {
    event: &'static str,
    id: String,
    module: String,
    file: String,
    range: TraceRange,
    kind: &'static str,
    slot: String,
    #[serde(rename = "type")]
    typ: String,
    source: TraceSource,
    editable: bool,
    weight: u32,
}

#[derive(Serialize)]
struct ObligationSlots {
    actual: String,
    expected: String,
}

#[derive(Serialize)]
struct ObligationEvent {
    event: &'static str,
    id: String,
    module: String,
    file: String,
    range: TraceRange,
    kind: &'static str,
    relation: &'static str,
    got: String,
    want: String,
    slots: ObligationSlots,
    dependencies: Vec<String>,
    result: &'static str,
    diagnostic_id: Option<String>,
}

#[derive(Serialize)]
struct DiagnosticEvent {
    event: &'static str,
    id: String,
    obligation: Option<String>,
    error_kind: String,
    message: String,
    file: String,
    range: TraceRange,
}

pub(crate) fn init(path: &Path) -> anyhow::Result<()> {
    let file = File::create(path)
        .with_context(|| format!("while creating type trace `{}`", path.display()))?;
    let sink = JsonlSink {
        writer: BufWriter::new(file),
    };
    let trace = TRACE.get_or_init(|| Mutex::new(None));
    *trace.lock().expect("diagnosis trace mutex poisoned") = Some(sink);
    Ok(())
}

pub(crate) fn finish() -> anyhow::Result<()> {
    if let Some(trace) = TRACE.get()
        && let Some(sink) = trace.lock().expect("diagnosis trace mutex poisoned").as_mut()
    {
        sink.writer.flush()?;
    }
    Ok(())
}

pub(crate) fn is_enabled() -> bool {
    TRACE
        .get()
        .and_then(|trace| trace.lock().ok().map(|guard| guard.is_some()))
        .unwrap_or(false)
}

#[cfg(test)]
fn reset_for_tests() {
    if let Some(trace) = TRACE.get() {
        *trace.lock().expect("diagnosis trace mutex poisoned") = None;
    }
    ASSUMPTION_ID.store(1, Ordering::Relaxed);
    OBLIGATION_ID.store(1, Ordering::Relaxed);
    DIAGNOSTIC_ID.store(1, Ordering::Relaxed);
}

pub(crate) fn record_type_obligation(
    module: &Module,
    loc: TextRange,
    context: &crate::error::context::TypeCheckContext,
    got: String,
    want: String,
    result: &'static str,
) {
    if !is_enabled() {
        return;
    }
    let obligation_id = next_id("O", &OBLIGATION_ID);
    let actual_id = next_id("A", &ASSUMPTION_ID);
    let expected_id = next_id("A", &ASSUMPTION_ID);
    let diagnostic_id = if result == "error" {
        Some(next_id("E", &DIAGNOSTIC_ID))
    } else {
        None
    };
    let actual_slot = actual_slot(module, loc, &context.kind);
    let expected_slot = context
        .slot
        .clone()
        .unwrap_or_else(|| expected_slot(module, loc, &context.kind));
    let file = module.path().as_path().to_string_lossy().into_owned();
    let module_name = module.name().as_str().to_owned();

    write_event(&AssumptionEvent {
        event: "assumption",
        id: actual_id.clone(),
        module: module_name.clone(),
        file: file.clone(),
        range: trace_range(module, loc),
        kind: "local_inferred_type",
        slot: actual_slot.clone(),
        typ: got.clone(),
        source: TraceSource {
            kind: "pyrefly_check",
            text: got.clone(),
        },
        editable: false,
        weight: 100,
    });
    write_event(&AssumptionEvent {
        event: "assumption",
        id: expected_id.clone(),
        module: module_name.clone(),
        file: file.clone(),
        range: trace_range(module, loc),
        kind: "explicit_annotation",
        slot: expected_slot.clone(),
        typ: want.clone(),
        source: TraceSource {
            kind: "pyrefly_check",
            text: want.clone(),
        },
        editable: true,
        weight: 5,
    });
    write_event(&ObligationEvent {
        event: "obligation",
        id: obligation_id.clone(),
        module: module_name,
        file,
        range: trace_range(module, loc),
        kind: obligation_kind(&context.kind),
        relation: "assignable",
        got: got.clone(),
        want: want.clone(),
        slots: ObligationSlots {
            actual: actual_slot,
            expected: expected_slot,
        },
        dependencies: vec![actual_id, expected_id],
        result,
        diagnostic_id: diagnostic_id.clone(),
    });
    if let Some(diagnostic_id) = diagnostic_id {
        write_event(&DiagnosticEvent {
            event: "diagnostic",
            id: diagnostic_id,
            obligation: Some(obligation_id),
            error_kind: context.kind.as_error_kind().to_name().to_owned(),
            message: format!("Type `{got}` is not assignable to `{want}`"),
            file: module.path().as_path().to_string_lossy().into_owned(),
            range: trace_range(module, loc),
        });
    }
}

pub(crate) fn record_diagnostic(error: &Error) {
    if !is_enabled() {
        return;
    }
    write_event(&DiagnosticEvent {
        event: "diagnostic",
        id: next_id("E", &DIAGNOSTIC_ID),
        obligation: None,
        error_kind: error.error_kind().to_name().to_owned(),
        message: error.msg(),
        file: error.path().as_path().to_string_lossy().into_owned(),
        range: TraceRange {
            start_line: error.display_range().start.line_within_file().get(),
            start_col: error.display_range().start.column().get(),
            end_line: error.display_range().end.line_within_file().get(),
            end_col: error.display_range().end.column().get(),
        },
    });
}

fn write_event<T: Serialize>(event: &T) {
    if let Some(trace) = TRACE.get()
        && let Some(sink) = trace.lock().expect("diagnosis trace mutex poisoned").as_mut()
    {
        let _ = serde_json::to_writer(&mut sink.writer, event);
        let _ = writeln!(sink.writer);
    }
}

fn next_id(prefix: &str, counter: &AtomicUsize) -> String {
    format!("{prefix}{}", counter.fetch_add(1, Ordering::Relaxed))
}

fn trace_range(module: &Module, loc: TextRange) -> TraceRange {
    let display = module.display_range(loc);
    TraceRange {
        start_line: display.start.line_within_file().get(),
        start_col: display.start.column().get(),
        end_line: display.end.line_within_file().get(),
        end_col: display.end.column().get(),
    }
}

fn range_slot(module: &Module, loc: TextRange) -> String {
    let range = trace_range(module, loc);
    format!(
        "{}:{}:{}",
        module.path().as_path().display(),
        range.start_line,
        range.start_col
    )
}

fn actual_slot(module: &Module, loc: TextRange, kind: &TypeCheckKind) -> String {
    match kind {
        TypeCheckKind::CallArgument(name, func)
        | TypeCheckKind::CallVarArgs(_, name, func) => format!(
            "call:{}:arg:{}",
            function_slot_name(module, func.as_ref()),
            name.as_ref().map(|x| x.as_str()).unwrap_or("_")
        ),
        TypeCheckKind::CallKwArgs(arg, _, func) => format!(
            "call:{}:kwarg:{}",
            function_slot_name(module, func.as_ref()),
            arg.as_ref().map(|x| x.as_str()).unwrap_or("_")
        ),
        TypeCheckKind::CallUnpackKwArg(name, func) => {
            format!("call:{}:unpack_kwarg:{}", function_slot_name(module, func.as_ref()), name)
        }
        TypeCheckKind::AnnotatedName(name) => format!("local:{}:value", name),
        TypeCheckKind::Attribute(name) => format!("attribute:{}:value", name),
        TypeCheckKind::AnnAssign => format!("assignment:{}:value", range_slot(module, loc)),
        TypeCheckKind::ExplicitFunctionReturn
        | TypeCheckKind::ImplicitFunctionReturn(_)
        | TypeCheckKind::MagicMethodReturn(..)
        | TypeCheckKind::TypeGuardReturn => format!("return:{}:expr", range_slot(module, loc)),
        _ => format!("expr:{}:actual", range_slot(module, loc)),
    }
}

fn expected_slot(module: &Module, loc: TextRange, kind: &TypeCheckKind) -> String {
    match kind {
        TypeCheckKind::CallArgument(name, func)
        | TypeCheckKind::CallVarArgs(_, name, func) => format!(
            "function:{}:param:{}",
            function_slot_name(module, func.as_ref()),
            name.as_ref().map(|x| x.as_str()).unwrap_or("_")
        ),
        TypeCheckKind::CallKwArgs(_, param, func) => format!(
            "function:{}:param:{}",
            function_slot_name(module, func.as_ref()),
            param.as_ref().map(|x| x.as_str()).unwrap_or("_")
        ),
        TypeCheckKind::CallUnpackKwArg(name, func) => {
            format!("function:{}:param:{}", function_slot_name(module, func.as_ref()), name)
        }
        TypeCheckKind::AnnotatedName(name) => format!("local:{}:annotation", name),
        TypeCheckKind::Attribute(name) => format!("attribute:{}:annotation", name),
        TypeCheckKind::AnnAssign => format!("assignment:{}:annotation", range_slot(module, loc)),
        TypeCheckKind::ExplicitFunctionReturn
        | TypeCheckKind::ImplicitFunctionReturn(_)
        | TypeCheckKind::MagicMethodReturn(..)
        | TypeCheckKind::TypeGuardReturn => {
            format!("function:{}:return:{}", module.name().as_str(), range_slot(module, loc))
        }
        _ => format!("expr:{}:expected", range_slot(module, loc)),
    }
}

fn function_slot_name(
    module: &Module,
    func: Option<&pyrefly_types::callable::FunctionKind>,
) -> String {
    match func {
        Some(func) => sanitize_slot_part(&func.format(module.name())),
        None => sanitize_slot_part(module.name().as_str()),
    }
}

fn sanitize_slot_part(x: &str) -> String {
    x.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

fn obligation_kind(kind: &TypeCheckKind) -> &'static str {
    match kind {
        TypeCheckKind::ExplicitFunctionReturn
        | TypeCheckKind::ImplicitFunctionReturn(_)
        | TypeCheckKind::MagicMethodReturn(..)
        | TypeCheckKind::TypeGuardReturn => "return",
        TypeCheckKind::CallArgument(..)
        | TypeCheckKind::CallVarArgs(..)
        | TypeCheckKind::CallKwArgs(..)
        | TypeCheckKind::CallUnpackKwArg(..) => "call_argument",
        TypeCheckKind::AnnAssign
        | TypeCheckKind::AnnotatedName(_)
        | TypeCheckKind::Attribute(_)
        | TypeCheckKind::UnpackedAssign => "assignment",
        TypeCheckKind::Container => "container_element",
        _ => "assignability",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    use pyrefly_python::module::Module;
    use pyrefly_python::module_name::ModuleName;
    use pyrefly_python::module_path::ModulePath;
    use pyrefly_types::callable::FunctionKind;
    use ruff_python_ast::name::Name;
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;
    use serde_json::Value;

    use crate::error::context::TypeCheckContext;

    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_module() -> Module {
        Module::new(
            ModuleName::from_str("sample"),
            ModulePath::memory(PathBuf::from("sample.py")),
            Arc::new("def f(x: str | None) -> str:\n    return x\n".to_owned()),
        )
    }

    fn trace_values(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn disabled_by_default() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        assert!(!is_enabled());
    }

    #[test]
    fn invalid_trace_path_reports_context() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let path = PathBuf::from("/definitely/not/a/real/pyrefly/trace.jsonl");
        let err = init(&path).unwrap_err().to_string();
        assert!(err.contains("while creating type trace"));
        assert!(err.contains("trace.jsonl"));
        reset_for_tests();
    }

    #[test]
    fn writes_failed_return_obligation_jsonl() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let tempdir = tempfile::tempdir().unwrap();
        let trace_path = tempdir.path().join("trace.jsonl");
        init(&trace_path).unwrap();

        let module = test_module();
        let context = TypeCheckContext::of_kind(TypeCheckKind::ExplicitFunctionReturn)
            .with_slot("function:f:return".to_owned());
        record_type_obligation(
            &module,
            TextRange::new(TextSize::from(37), TextSize::from(38)),
            &context,
            "str | None".to_owned(),
            "str".to_owned(),
            "error",
        );
        finish().unwrap();

        let values = trace_values(&trace_path);
        assert_eq!(values.len(), 4);
        assert!(values.iter().any(|event| {
            event["event"] == "assumption"
                && event["id"] == "A1"
                && event["kind"] == "local_inferred_type"
                && event["type"] == "str | None"
                && event["editable"] == false
        }));
        assert!(values.iter().any(|event| {
            event["event"] == "assumption"
                && event["id"] == "A2"
                && event["kind"] == "explicit_annotation"
                && event["slot"] == "function:f:return"
                && event["type"] == "str"
                && event["editable"] == true
        }));
        assert!(values.iter().any(|event| {
            event["event"] == "obligation"
                && event["id"] == "O1"
                && event["kind"] == "return"
                && event["relation"] == "assignable"
                && event["got"] == "str | None"
                && event["want"] == "str"
                && event["slots"]["expected"] == "function:f:return"
                && event["dependencies"] == serde_json::json!(["A1", "A2"])
                && event["result"] == "error"
                && event["diagnostic_id"] == "E1"
        }));
        assert!(values.iter().any(|event| {
            event["event"] == "diagnostic"
                && event["id"] == "E1"
                && event["obligation"] == "O1"
                && event["error_kind"] == "bad-return"
                && event["message"] == "Type `str | None` is not assignable to `str`"
        }));
        reset_for_tests();
    }

    #[test]
    fn writes_call_argument_slots_jsonl() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let tempdir = tempfile::tempdir().unwrap();
        let trace_path = tempdir.path().join("trace.jsonl");
        init(&trace_path).unwrap();

        let module = test_module();
        let func = FunctionKind::from_name(module.clone(), None, &Name::new("label"), None, None);
        let context = TypeCheckContext::of_kind(TypeCheckKind::CallArgument(
            Some(Name::new("value")),
            Some(func),
        ));
        record_type_obligation(
            &module,
            TextRange::new(TextSize::from(37), TextSize::from(38)),
            &context,
            "int".to_owned(),
            "str".to_owned(),
            "error",
        );
        finish().unwrap();

        let values = trace_values(&trace_path);
        assert!(values.iter().any(|event| {
            event["event"] == "assumption"
                && event["id"] == "A1"
                && event["slot"] == "call:label:arg:value"
                && event["type"] == "int"
                && event["editable"] == false
        }));
        assert!(values.iter().any(|event| {
            event["event"] == "assumption"
                && event["id"] == "A2"
                && event["slot"] == "function:label:param:value"
                && event["type"] == "str"
                && event["editable"] == true
        }));
        assert!(values.iter().any(|event| {
            event["event"] == "obligation"
                && event["id"] == "O1"
                && event["kind"] == "call_argument"
                && event["slots"]["actual"] == "call:label:arg:value"
                && event["slots"]["expected"] == "function:label:param:value"
                && event["diagnostic_id"] == "E1"
        }));
        reset_for_tests();
    }
}
