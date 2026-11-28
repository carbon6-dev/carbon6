use c6_api::DecisionAnswer;
use c6_api::project_contract::{Capability, CapabilityLocation, ContractContent, ContractItem, ContractSet, ProjectContract, Provenance};
use c6_api::projects::ProjectNew;
use c6_proto::{CreateSession, session_command};
use serde_json::json;

use crate::project_contract as contract;
use crate::realms::{Caller, HOME, ROOT};
use crate::work::tests::routing_fixture;
use crate::{HeadState, UserError};

fn human() -> Caller {
    Caller { browser: true, authenticated_browser: true, ..Caller::in_realm(HOME) }
}

async fn project(st: &HeadState) -> String {
    crate::projects::create(st, &human(), &ProjectNew { title: "Widget".into(), ..Default::default() }, "you").await.unwrap().id
}

fn change(expected_version: u64, rules: &[&str]) -> ContractSet {
    ContractSet {
        expected_version,
        contract: ContractContent {
            architecture_rules: rules
                .iter()
                .map(|rule| ContractItem {
                    value: (*rule).into(),
                    // Client-provided provenance must not become accepted provenance.
                    provenance: Provenance { by: "synthetic caller".into(), at: -1, suggested_by: Some("synthetic request".into()) },
                })
                .collect(),
            ..Default::default()
        },
    }
}

async fn child_row(st: &HeadState, id: &str, parent: &str, realm: &str) {
    // Keep these test sessions offline: the test only builds head snapshots.
    sqlx::query(
        "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm, \
         parent_id, parent_call_id, root_id) SELECT ?, worker_id, ?, cwd, connection_id, model, status, 1, 1, ?, id, ?, \
         coalesce(root_id, id) FROM sessions WHERE id = ?",
    )
    .bind(id)
    .bind(id)
    .bind(realm)
    .bind(format!("call-{id}"))
    .bind(parent)
    .execute(&st.store.pool)
    .await
    .unwrap();
}

async fn snapshot(st: &HeadState, id: &str) -> CreateSession {
    let row = st.store.session(id).await.unwrap().unwrap();
    let command = crate::sessions::refresh_spec(st, &row).await.unwrap().unwrap();
    let Some(session_command::Kind::RefreshSpec(spec)) = command.kind else { panic!("expected a refresh snapshot") };
    spec
}

#[tokio::test]
async fn human_edits_persist_version_and_provenance_but_stale_edits_do_not() {
    let (_dir, head, _mock, _work) = routing_fixture().await;
    let st = &head.state;
    let id = project(st).await;
    let caller = human();

    assert!(contract::history(st, &id, &caller).await.unwrap().is_empty());
    let first = contract::set(st, &id, &change(0, &["Keep the widget interface stable."]), &caller).await.unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(first.provenance.by, "you");
    assert!(first.provenance.at > 0);
    assert_eq!(first.provenance.suggested_by, None);
    assert_eq!(first.content.architecture_rules[0].provenance, first.provenance);

    let second_change = change(1, &["Keep the widget interface stable.", "Keep widget adapters separate."]);
    let second = contract::set(st, &id, &second_change, &caller).await.unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(second.content.architecture_rules[0], first.content.architecture_rules[0]);
    assert_eq!(second.content.architecture_rules[1].provenance, second.provenance);
    assert!(contract::set(st, &id, &second_change, &caller).await.is_err());
    assert_eq!(contract::history(st, &id, &caller).await.unwrap(), vec![first, second]);
}

#[tokio::test]
async fn agents_and_tokens_cannot_set_contract_and_other_realms_cannot_see_it() {
    let (_dir, head, _mock, _work) = routing_fixture().await;
    let st = &head.state;
    let id = project(st).await;
    let first = change(0, &["Keep the widget interface stable."]);

    let agent = Caller::in_realm(HOME);
    let token = Caller { api_token_name: Some("widget token".into()), ..human() };
    for caller in [&agent, &token] {
        let denied = contract::set(st, &id, &first, caller).await.unwrap_err();
        assert!(matches!(denied.downcast_ref::<UserError>(), Some(UserError::Forbidden(_))));
    }
    let accepted = contract::set(st, &id, &first, &human()).await.unwrap();

    let outside = Caller::in_realm(ROOT);
    let hidden = contract::history(st, &id, &outside).await.unwrap_err();
    assert!(matches!(hidden.downcast_ref::<UserError>(), Some(UserError::NotFound(_))));
    assert!(contract::propose(st, &id, &change(1, &[]), &outside, "session:widget-other").await.is_err());
    assert_eq!(contract::history(st, &id, &agent).await.unwrap(), vec![accepted]);
}

#[tokio::test]
async fn proposal_changes_policy_only_after_card_acceptance() {
    let (_dir, head, _mock, _work) = routing_fixture().await;
    let st = &head.state;
    let id = project(st).await;
    let caller = Caller::in_realm(HOME);
    let source = "session:session";
    let draft = change(0, &["Keep widget adapters separate."]);

    for (option, expected_revisions) in [("Keep current", 0), ("Accept", 1)] {
        let card = contract::propose(st, &id, &draft, &caller, source).await.unwrap();
        assert_eq!(card.state, "open");
        assert!(contract::history(st, &id, &caller).await.unwrap().is_empty());
        let answer = DecisionAnswer { option: Some(option.into()), ..Default::default() };
        let card = crate::decisions::answer(st, card.id, &answer, "test").await.unwrap();
        assert_eq!(card.state, "answered");
        assert_eq!(contract::history(st, &id, &caller).await.unwrap().len(), expected_revisions);
    }

    let revisions = contract::history(st, &id, &caller).await.unwrap();
    let accepted = &revisions[0];
    assert_eq!(accepted.version, 1);
    assert_eq!(accepted.provenance.by, "you");
    assert_eq!(accepted.provenance.suggested_by.as_deref(), Some(source));
    assert_eq!(accepted.content.architecture_rules[0].provenance, accepted.provenance);
    assert_eq!(accepted.content.architecture_rules[0].value, "Keep widget adapters separate.");
}

#[tokio::test]
async fn covered_failure_proposes_a_ledger_note_without_changing_policy_until_accepted() {
    let (_dir, head, _mock, _work) = routing_fixture().await;
    let st = &head.state;
    let id = project(st).await;
    let caller = human();
    let session = st.store.session("session").await.unwrap().unwrap();
    crate::projects::add_session(st, &id, &session.id, "you").await.unwrap();

    let covered = json!({"capabilities": ["inspect-widget"]});
    assert_eq!(contract::failure(st, &session, &covered).await.unwrap(), json!({"proposed": false}));
    assert!(crate::decisions::list(st, "open", &caller).await.unwrap().is_empty());

    let mut draft = change(0, &[]);
    draft.contract.decisions.push(ContractItem { value: "Keep the widget interface stable.".into(), ..Default::default() });
    let schema = json!({"type": "object", "properties": {}});
    draft.contract.capabilities.push(ContractItem {
        value: Capability {
            name: "inspect-widget".into(),
            description: "Inspect a widget".into(),
            location: CapabilityLocation::Template { name: "widget-procedure".into() },
            inputs: schema.clone(),
            result_schema: schema,
            trigger_phrases: vec![],
            command_patterns: vec![],
        },
        provenance: Provenance::default(),
    });
    let first = contract::set(st, &id, &draft, &caller).await.unwrap();

    for uncovered in [json!({}), json!({"capabilities": ["inspect-other"]})] {
        assert_eq!(contract::failure(st, &session, &uncovered).await.unwrap(), json!({"proposed": false}));
    }
    assert!(crate::decisions::list(st, "open", &caller).await.unwrap().is_empty());

    let mixed = json!({"capabilities": ["inspect-other", "inspect-widget"]});
    let proposed = contract::failure(st, &session, &mixed).await.unwrap();
    assert_eq!(proposed["proposed"], true);
    let card_id = proposed["decision"].as_i64().expect("proposal decision id");
    let card = crate::decisions::get(st, card_id).await.unwrap().unwrap();
    assert_eq!(card.state, "open");
    assert_eq!(card.context["project"], id);
    let proposed_change: ContractSet = serde_json::from_value(card.context["contract_change"].clone()).unwrap();
    assert_eq!(proposed_change.expected_version, first.version);
    let note = &proposed_change.contract.decisions[1].value;
    assert!(note.contains("inspect-widget") && !note.contains("inspect-other"));
    assert_eq!(contract::history(st, &id, &caller).await.unwrap(), vec![first.clone()]);

    let answer = DecisionAnswer { option: Some("Accept".into()), ..Default::default() };
    let card = crate::decisions::answer(st, card_id, &answer, "test").await.unwrap();
    assert_eq!(card.state, "answered");
    let revisions = contract::history(st, &id, &caller).await.unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0], first);
    let second = &revisions[1];
    assert_eq!(second.version, 2);
    assert_eq!(second.content.capabilities, first.content.capabilities);
    assert_eq!(second.content.decisions.len(), 2);
    assert_eq!(second.content.decisions[0], first.content.decisions[0]);
    assert_eq!(&second.content.decisions[1].value, note);
    assert_eq!(second.content.decisions[1].provenance, second.provenance);
    assert_eq!(second.provenance.suggested_by.as_deref(), Some("session:session"));

    assert_eq!(contract::failure(st, &session, &covered).await.unwrap(), json!({"proposed": false}));
    assert!(crate::decisions::list(st, "open", &caller).await.unwrap().is_empty());
}

#[tokio::test]
async fn descendant_refresh_snapshots_pick_up_only_same_realm_contract_changes() {
    let (_dir, head, _mock, _work) = routing_fixture().await;
    let st = &head.state;
    let id = project(st).await;
    child_row(st, "widget-child", "session", HOME).await;
    child_row(st, "widget-leaf", "widget-child", HOME).await;
    child_row(st, "widget-other", "session", ROOT).await;
    crate::projects::add_session(st, &id, "session", "you").await.unwrap();

    let no_contract = snapshot(st, "widget-leaf").await;
    assert!(contract::for_session(st, "widget-leaf").await.unwrap().is_none());
    assert!(no_contract.project_contract_json.is_none());
    assert!(no_contract.project_contract_prompt.is_none());
    assert!(!no_contract.base_prompt.is_empty());

    let mut previous_prompt = None;
    for (version, rule) in [(1, "Keep the widget interface stable."), (2, "Keep widget adapters separate.")] {
        let accepted = contract::set(st, &id, &change(version - 1, &[rule]), &human()).await.unwrap();
        assert_eq!(contract::for_session(st, "widget-leaf").await.unwrap(), Some((id.clone(), accepted.clone())));
        let next = snapshot(st, "widget-leaf").await;
        assert_eq!(next.base_prompt, no_contract.base_prompt);
        let resolved: ProjectContract = serde_json::from_str(next.project_contract_json.as_deref().unwrap()).unwrap();
        assert_eq!(resolved, accepted);
        let prompt = next.project_contract_prompt.unwrap();
        assert_eq!(prompt, contract::render(st, HOME, &accepted).await);
        assert!(prompt.contains(&format!("v{version}")) && prompt.contains(rule));
        assert_ne!(previous_prompt.as_ref(), Some(&prompt));
        previous_prompt = Some(prompt);
    }

    assert!(contract::for_session(st, "widget-other").await.unwrap().is_none());
    let other = snapshot(st, "widget-other").await;
    assert!(other.project_contract_json.is_none());
    assert!(other.project_contract_prompt.is_none());
}
