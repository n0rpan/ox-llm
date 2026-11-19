use oxa_fake::TestEnv;
use oxa_proto::Entry;
use oxa_session::{
    advice::{
        AdviceAdmission, AdviceHistory, AdvicePhase, AdvicePolicy, AdviceTier, AdviceTrigger, AdvisorRoute,
        StuckPoint,
    },
    FsStore, SessionStore,
};
use std::num::{NonZeroU16, NonZeroU32};

#[test]
fn advice_restart_preserves_one_call_per_point_and_escalates_only_advised_failures() {
    use AdviceAdmission::{AlreadyRequested, Disabled, Escalate, LimitReached, Reserve, Unavailable};
    use AdvicePhase::{Delivered, Failed, NotDelivered};
    use AdviceTier::{Bulk, Main, Top};
    use AdviceTrigger::{RepeatGuard, WorkerAsk};

    let env = TestEnv::new();
    let root = env.root().join("sessions");
    let store = FsStore::new(root.clone());
    let session = store.create(&env.work(), None, None).unwrap();
    let policy = AdvicePolicy {
        triggers: vec![WorkerAsk, RepeatGuard],
        max_calls: NonZeroU16::new(2).unwrap(),
        failures_before_escalation: NonZeroU16::new(1).unwrap(),
        max_output: NonZeroU32::new(128).unwrap(),
    };
    let main = AdvisorRoute {
        tier: Main,
        model: "synthetic-main".into(),
        pool: "main-pool".into(),
    };
    let point = StuckPoint::from_evidence(b"synthetic first error").unwrap();
    let history = AdviceHistory::default();
    assert_eq!(
        history.admit(None, point.clone(), WorkerAsk, Bulk, Some(&main)),
        Disabled
    );
    assert_eq!(
        history.admit(Some(&policy), point.clone(), WorkerAsk, Main, Some(&main)),
        Unavailable
    );

    let Reserve(first) = history.admit(Some(&policy), point.clone(), WorkerAsk, Bulk, Some(&main)) else {
        panic!("first point should reserve advice");
    };
    assert_eq!(first.grant().max_output, policy.max_output);
    let append = |store: &FsStore, index: u8, body: oxa_proto::EntryBody| {
        store
            .append(
                &session,
                &Entry {
                    id: format!("{index:08x}"),
                    parent: (index > 1).then(|| format!("{:08x}", index - 1)),
                    ts_ms: 1000,
                    body,
                },
            )
            .unwrap();
        store.sync(&session).unwrap();
    };
    append(&store, 1, first.body());
    drop(store);

    let store = FsStore::new(root);
    let history = || AdviceHistory::load(&store.load(&session).unwrap()).unwrap();
    assert_eq!(
        history().admit(Some(&policy), point, RepeatGuard, Bulk, Some(&main)),
        AlreadyRequested
    );
    assert!(first.advance(Failed).is_err());
    append(&store, 2, first.advance(NotDelivered).unwrap().body());
    let second = StuckPoint::from_evidence(b"synthetic next error").unwrap();
    let Reserve(receipt) = history().admit(Some(&policy), second.clone(), WorkerAsk, Bulk, Some(&main))
    else {
        panic!("an uncertain first dispatch should not count as an advised failure");
    };
    append(&store, 3, receipt.body());
    let delivered = receipt.advance(Delivered).unwrap();
    append(
        &store,
        4,
        receipt.delivery_body("Synthetic advice.".into()).unwrap(),
    );
    append(&store, 5, delivered.advance(Failed).unwrap().body());
    assert_eq!(history().receipt(&second).unwrap().phase(), Failed);

    let next = StuckPoint::from_evidence(b"synthetic later error").unwrap();
    assert_eq!(
        history().admit(Some(&policy), next.clone(), WorkerAsk, Bulk, Some(&main)),
        Escalate
    );
    let top = AdvisorRoute {
        tier: Top,
        model: "synthetic-top".into(),
        pool: "top-pool".into(),
    };
    assert_eq!(
        history().admit(Some(&policy), next, WorkerAsk, Main, Some(&top)),
        LimitReached
    );

    // A torn or forged lifecycle cannot silently reset the attempt accounting.
    let mut entries = store.load(&session).unwrap();
    entries.remove(2);
    assert!(AdviceHistory::load(&entries).is_err());
}
