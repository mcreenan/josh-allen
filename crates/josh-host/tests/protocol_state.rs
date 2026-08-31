mod common;

use josh_host::Session;
use josh_protocol::{CatalogSetParams, ExecutionResult, WireErrorCode};

#[test]
fn cancellation_cleans_the_active_execution_slot() {
    let mut session = common::initialized_session();
    let loaded = common::load_unit_program(&mut session);
    let prepared = session
        .prepare_execution(
            "h-run".to_owned(),
            common::execution_params(loaded, "exec-1"),
        )
        .unwrap();
    assert!(session.cancel("h-run"));
    assert_eq!(
        prepared.run(None),
        ExecutionResult::Cancelled { reason: None }
    );
    session.finish_execution("h-run");

    let second = common::load_unit_program(&mut session);
    session
        .prepare_execution(
            "h-run-2".to_owned(),
            common::execution_params(second, "exec-2"),
        )
        .unwrap();
}

#[test]
fn execution_ids_are_never_reused_after_cleanup() {
    let mut session = common::initialized_session();
    let loaded = common::load_unit_program(&mut session);
    let params = common::execution_params(loaded, "exec-1");
    session
        .prepare_execution("first".to_owned(), params.clone())
        .unwrap();
    session.finish_execution("first");
    assert_eq!(
        session
            .prepare_execution("second".to_owned(), params)
            .unwrap_err()
            .code,
        WireErrorCode::ExecutionDuplicate
    );
}

#[test]
fn execution_cannot_gain_an_unnegotiated_standard_capability() {
    let mut session = common::initialized_session();
    let loaded = common::load_unit_program(&mut session);
    let mut params = common::execution_params(loaded, "exec-capability");
    params.granted_capabilities.push("fs.read".to_owned());
    assert_eq!(
        session
            .prepare_execution("capability".to_owned(), params)
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalid
    );
}

#[test]
fn execution_cannot_gain_negotiated_but_undeclared_authority() {
    let mut session = Session::new();
    let mut initialize = common::initialize_params();
    initialize.standard_capabilities = vec!["fs.read".to_owned()];
    session.initialize(&initialize).unwrap();
    let catalog = CatalogSetParams {
        schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
        metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
        tools: Vec::new(),
    };
    session
        .set_projection(
            &josh_protocol::HostProjectionSetParams::complete_for_catalog(
                "test-projection",
                initialize.host,
                josh_protocol::SessionBindingLevel::None,
                &catalog,
            ),
        )
        .unwrap();
    session.set_catalog(&catalog).unwrap();
    let loaded = common::load_unit_program(&mut session);
    let mut params = common::execution_params(loaded, "exec-authority");
    params.granted_capabilities.push("fs.read".to_owned());
    assert_eq!(
        session
            .prepare_execution("authority".to_owned(), params)
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalid
    );
}

#[test]
fn pure_execution_rejects_a_working_directory_before_acceptance() {
    let mut session = common::initialized_session();
    let loaded = common::load_unit_program(&mut session);
    let mut params = common::execution_params(loaded, "exec-pure-workdir");
    params.working_directory = Some(".".to_owned());
    assert_eq!(
        session
            .prepare_execution("pure-workdir".to_owned(), params)
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalid
    );
}
