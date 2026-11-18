//! Presentation of the existing typed native chat proposal; no action or policy writer.

#[cfg(test)]
mod tests {
    use crate::lead_chat::conversation::{ActionDraft, BoardChange, ConversationReply, ProposedAction};
    use crate::lead_page::{document_for, OwnerChatView, Page, Prompt, View};
    use oxa_work::standing::{
        Condition, GateEvidence, Kind, ReservedClass, ReviewTier, Scope, Source, Value,
    };

    fn page(draft: Option<ActionDraft>, confirmed: bool) -> String {
        let reply = ConversationReply {
            text: "Fixture reply".into(),
            facts: vec![],
            action: draft.map(|draft| ProposedAction {
                draft,
                source: Source::SessionEntry {
                    session_id: "private-session-source".into(),
                    entry_id: "private-entry-source".into(),
                },
            }),
        };
        let proposal = reply.action.as_ref().map(|action| {
            (
                "proposal-example".to_owned(),
                serde_json::to_value(action).unwrap(),
            )
        });
        let owner = OwnerChatView {
            csrf: "csrf-example",
            reply: Some(&reply),
            proposal: proposal.as_ref(),
            confirmed,
        };
        document_for(
            Page::Chat(Prompt::Status),
            &View {
                owner_chat: Some(owner),
                ..Default::default()
            },
            0,
        )
    }

    fn rule(kind: Kind, scope: Scope, value: Value) -> String {
        page(
            Some(ActionDraft::Rule {
                rule: kind,
                scope,
                value,
            }),
            false,
        )
    }

    fn protected(html: &str) {
        assert!(!html.contains("<pre>") && !html.contains("&quot;kind&quot;"));
        assert!(!html.contains("private-session-source") && !html.contains("private-entry-source"));
        assert!(html.contains("name=csrf value=\"csrf-example\""));
        assert!(html.contains("name=proposal_id value=\"proposal-example\""));
        assert_eq!(html.matches("<button>Confirm proposal</button>").count(), 1);
        assert!(!html.contains(">Undo<") && !html.contains(">Steer now<"));
    }

    #[test]
    fn typed_chat_proposals_show_human_actions_and_keep_existing_confirmation() {
        let follow = page(
            Some(ActionDraft::FollowUp {
                task: "task-<example>".into(),
                text: "Check <script>fixture</script> & report".into(),
            }),
            false,
        );
        assert!(follow.contains("Proposed follow-up") && follow.contains("task-&lt;example&gt;"));
        assert!(follow.contains("Check &lt;script&gt;fixture&lt;/script&gt; &amp; report"));
        protected(&follow);
        for (change, label) in [
            (BoardChange::ReleaseLease, "Release lease"),
            (BoardChange::Reprioritize, "Change priority"),
            (BoardChange::Park, "Park item"),
        ] {
            let html = page(Some(ActionDraft::Board { item: 42, change }), false);
            assert!(html.contains("Proposed board change") && html.contains(label));
            assert!(html.contains("href=/items/42>#42</a>"));
            protected(&html);
        }
        let note = rule(
            Kind::Note,
            Scope::Branch("feature/<example>".into()),
            Value::Text("Keep <b>evidence</b>".into()),
        );
        assert!(note.contains("Proposed standing rule") && note.contains("Note"));
        assert!(
            note.contains("Branch feature/&lt;example&gt;")
                && note.contains("Keep &lt;b&gt;evidence&lt;/b&gt;")
        );
        protected(&note);
    }

    #[test]
    fn rule_cards_keep_scope_conditions_and_exact_owner_policy_values() {
        let budget = rule(
            Kind::Budget,
            Scope::PullRequest(42),
            Value::Budget {
                daily_usd_micros: 1_000_001,
            },
        );
        assert!(budget.contains("USD 1.000001 per day") && budget.contains("href=/items/42>#42</a>"));
        let hold = rule(
            Kind::Hold,
            Scope::Repository,
            Value::Constraint {
                active: true,
                until: Some(Condition::PrMerged(43)),
            },
        );
        assert!(hold.contains("Active until") && hold.contains("href=/items/43>#43</a>"));
        assert!(hold.contains("This repository"));
        let release = rule(
            Kind::Freeze,
            Scope::Task("task-example".into()),
            Value::Constraint {
                active: false,
                until: None,
            },
        );
        assert!(release.contains("Release constraint") && release.contains("Task task-example"));
        let retired = rule(Kind::Note, Scope::Path("src/example.rs".into()), Value::Retired);
        assert!(retired.contains("Retire rule") && retired.contains("Path src/example.rs"));
        let gate = rule(
            Kind::Gate,
            Scope::Repository,
            Value::Gate {
                tier: ReviewTier::T3,
                evidence: vec![GateEvidence::PrBuild, GateEvidence::Review, GateEvidence::Queue],
            },
        );
        for label in ["T3", "PR checks", "Review", "Merge queue"] {
            assert!(gate.contains(label));
        }
        let reserve = rule(
            Kind::Reserve,
            Scope::Host("worker-example".into()),
            Value::Reserve {
                classes: vec![ReservedClass::Paid, ReservedClass::Credentials],
            },
        );
        assert!(
            reserve.contains("Paid calls")
                && reserve.contains("Credentials")
                && reserve.contains("Host worker-example")
        );
        for html in [&budget, &hold, &release, &retired, &gate, &reserve] {
            protected(html);
        }
    }

    #[test]
    fn confirmation_record_is_not_shown_as_application_and_questions_have_no_action() {
        let confirmed = page(
            Some(ActionDraft::Board {
                item: 42,
                change: BoardChange::Park,
            }),
            true,
        );
        assert!(confirmed.contains("Proposed board change"));
        assert!(confirmed.contains("Confirmation recorded. Application is pending."));
        assert!(!confirmed.contains("Confirm proposal") && !confirmed.contains(">Undo<"));
        let question = page(None, false);
        assert!(question.contains("Fixture reply"));
        assert!(!question.contains("Proposed action") && !question.contains("Confirm proposal"));
    }
}
