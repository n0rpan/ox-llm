use oxa_cli::{advisor_scrubber, ScrubCtx};
use oxa_config::{Dirs, Settings};
use oxa_core::{
    advice::{AdviceContext, AdviceDispatcher, BoundAdvisor},
    repeat::{LIMIT, NOTICES_BEFORE_STOP},
    Deps, Open, Session, ToolSelection,
};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{CancelToken, HeadlessUi, OxaError, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{
    advice::{AdviceGrant, AdvicePolicy, AdviceTier, AdviceTrigger, AdvisorRoute},
    MemStore, SessionRef,
};
use std::{
    num::{NonZeroU16, NonZeroU32},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const TOOL_PATH: &str = "private-fixture-argument.txt";
const IDENTITY: &str = "fixtureperson";

struct Advisor(AtomicUsize);

impl AdviceDispatcher for Advisor {
    fn authorized(&self, _: &SessionRef, _: u64) -> bool {
        true
    }

    fn call(&self, _: &AdviceGrant, context: &AdviceContext, _: &CancelToken) -> Result<String, OxaError> {
        assert_eq!(context.objective, "Review the change for <user>.");
        assert!(context.delta.contains("tool: read"));
        assert!(context
            .delta
            .contains("Synthetic compiler diagnostic for <user>."));
        assert!(!context.delta.contains(IDENTITY));
        assert!(!context.delta.contains(TOOL_PATH));
        assert!(context.failing_evidence.contains("same tool call"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("Inspect the reported diagnostic.".into())
    }
}

#[test]
fn advisor_dispatch_uses_scrubbed_diagnostics_while_the_worker_keeps_the_original_context() {
    let env = TestEnv::new();
    env.write(
        &format!("work/{TOOL_PATH}"),
        "Synthetic compiler diagnostic for fixtureperson.",
    );
    let args = serde_json::json!({"path": TOOL_PATH}).to_string();
    let mut replies: Vec<_> = (0..LIMIT + NOTICES_BEFORE_STOP + 1)
        .map(|index| ScriptTurn::tool(&format!("read-{index}"), "read", &args))
        .collect();
    replies.push(ScriptTurn::text("Inspected the reported diagnostic."));
    let engine = Arc::new(ScriptedEngine::new(replies));
    let mut session = Session::open(
        Deps {
            engine: engine.clone(),
            store: Arc::new(MemStore::new()),
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .unwrap();

    let advisor = Arc::new(Advisor(AtomicUsize::new(0)));
    let original = session.inner().sref.clone();
    session.inner().shared.bind_advisor(BoundAdvisor {
        session: original.clone(),
        lease_identity: [3; 32],
        objective: "Review the change for fixtureperson.".into(),
        worker: AdviceTier::Bulk,
        policy: AdvicePolicy {
            triggers: vec![AdviceTrigger::RepeatGuard],
            max_calls: NonZeroU16::MIN,
            failures_before_escalation: NonZeroU16::MIN,
            max_output: NonZeroU32::new(64).unwrap(),
        },
        route: AdvisorRoute {
            tier: AdviceTier::Main,
            model: "fixture-advisor".into(),
            pool: "fixture-pool".into(),
        },
        dispatcher: advisor.clone(),
        scrub: advisor_scrubber(ScrubCtx {
            user: Some(IDENTITY.into()),
            ..ScrubCtx::default()
        }),
    });
    session
        .prompt(
            "Inspect the file.".into(),
            vec![],
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();

    assert_eq!(advisor.0.load(Ordering::SeqCst), 1);
    assert_eq!(session.inner().sref, original);
    let requests = engine.requests();
    let messages = &requests.last().unwrap().messages;
    assert!(messages.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains(IDENTITY))));
    assert!(messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .any(|call| call.args_raw.contains(TOOL_PATH)));
    assert!(messages.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("Inspect the reported diagnostic."))));
}
