// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `config.rs` — parsing, defaults, and startup validation.
//!
//! Defaults get their own tests because each one is a promise about what an
//! existing deployment keeps doing after an upgrade, and `validate` gets
//! them because it is the last chance to catch a deployment mistake before
//! the server starts answering.

use crate::config::{Config, HookEvent};

/// Parses config text, panicking with the text on a parse failure.
#[track_caller]
fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text)
        .unwrap_or_else(|err| panic!("config does not parse: {}\n{}", err, toml_text))
}

/// A minimal valid `[server]` section pointing at a fresh temporary store,
/// returned with the directory that must outlive it.
fn server_section() -> (tempfile::TempDir, String) {
    let store = tempfile::tempdir().expect("cannot create temporary store");

    let section = format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 5355\napi_key = \"k\"\nrepos_path = \"{}\"\n",
        store.path().display()
    );

    (store, section)
}

#[test]
fn optional_sections_collapse_to_their_defaults() {
    let (_store, server) = server_section();

    let config = parse(&server);

    // Every default here is "what the deployment already did".
    assert!(
        config.server.checkout_files,
        "files stay on disk by default"
    );
    assert!(
        !config.server.checkout_files_autoheal,
        "a whole-store repair pass that removes files must be opt-in"
    );

    assert!(config.limits.allowed_extensions.is_none());
    assert_eq!(config.limits.batch_read_maximum_files, 100);

    assert!(config.maintenance.enabled);
    assert_eq!(config.maintenance.delay_secs, 86_400);
    assert!(
        !config.maintenance.destructive_prune,
        "maintenance must not be able to destroy data by default"
    );
    assert!(config.maintenance.maximum_packs.is_none());

    assert!(config.hooks.is_none());
    assert!(
        config.replication.is_none(),
        "a node with no [replication] section is standalone"
    );
}

#[test]
fn hook_events_parse_from_their_dotted_wire_names() {
    let (_store, server) = server_section();

    let config = parse(&format!(
        "{}\n[hooks]\nurl = \"https://example.com/hook\"\n\
         events = [\"file.created\", \"file.updated\", \"file.deleted\", \
         \"file.moved\", \"order.updated\", \"order.deleted\"]\n\
         retry_attempts = 5\nretry_backoff_ms = 2000\n",
        server
    ));

    let hooks = config.hooks.expect("hooks section missing");

    assert_eq!(
        hooks.events,
        vec![
            HookEvent::FileCreated,
            HookEvent::FileUpdated,
            HookEvent::FileDeleted,
            HookEvent::FileMoved,
            HookEvent::OrderUpdated,
            HookEvent::OrderDeleted,
        ]
    );
}

#[test]
fn validate_accepts_a_minimal_standalone_config() {
    let (_store, server) = server_section();

    assert!(parse(&server).validate().is_ok());
}

#[test]
fn validate_reports_every_problem_at_once() {
    // Collected rather than short-circuited so an operator fixes every
    // mistake in one edit instead of playing whack-a-mole.
    let store = tempfile::tempdir().expect("cannot create temporary store");

    let config = parse(&format!(
        "[server]\nhost = \"\"\nport = 5355\napi_key = \"\"\nlog_level = \"chatty\"\n\
         repos_path = \"{}\"\n",
        store.path().display()
    ));

    let errors = config.validate().expect_err("invalid config accepted");

    assert!(
        errors.len() >= 3,
        "expected several errors, got {:?}",
        errors
    );
    assert!(errors.iter().any(|error| error.contains("server.host")));
    assert!(errors.iter().any(|error| error.contains("server.api_key")));
    assert!(errors
        .iter()
        .any(|error| error.contains("server.log_level")));
}

#[test]
fn validate_creates_a_missing_repository_store() {
    // Validating repos_path doubles as provisioning, so a fresh deployment
    // needs no manual mkdir.
    let store = tempfile::tempdir().expect("cannot create temporary store");
    let nested = store.path().join("deep").join("store");

    assert!(!nested.exists());

    let config = parse(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 5355\napi_key = \"k\"\nrepos_path = \"{}\"\n",
        nested.display()
    ));

    assert!(config.validate().is_ok());
    assert!(nested.is_dir(), "repos_path should have been created");
}

#[test]
fn validate_rejects_a_hook_url_that_is_not_http() {
    let (_store, server) = server_section();

    let config = parse(&format!(
        "{}\n[hooks]\nurl = \"ftp://example.com/hook\"\nevents = [\"file.created\"]\n\
         retry_attempts = 1\nretry_backoff_ms = 1\n",
        server
    ));

    let errors = config.validate().expect_err("non-http hook url accepted");

    assert!(
        errors.iter().any(|error| error.contains("hooks.url")),
        "{:?}",
        errors
    );
}

#[test]
fn validate_rejects_an_empty_hook_subscription_list() {
    let (_store, server) = server_section();

    let config = parse(&format!(
        "{}\n[hooks]\nurl = \"https://example.com/hook\"\nevents = []\n\
         retry_attempts = 1\nretry_backoff_ms = 1\n",
        server
    ));

    let errors = config.validate().expect_err("empty events list accepted");

    assert!(
        errors.iter().any(|error| error.contains("hooks.events")),
        "{:?}",
        errors
    );
}

#[test]
fn validate_requires_a_master_url_on_a_replica_and_refuses_one_on_a_master() {
    let (_store, server) = server_section();

    let replica_errors = parse(&format!(
        "{}\n[replication]\nrole = \"replica\"\nsecret = \"s\"\nnode_id = \"n\"\n",
        server
    ))
    .validate()
    .expect_err("a replica with no master_url was accepted");

    assert!(
        replica_errors
            .iter()
            .any(|error| error.contains("master_url is required")),
        "{:?}",
        replica_errors
    );

    let master_errors = parse(&format!(
        "{}\n[replication]\nrole = \"master\"\nsecret = \"s\"\nnode_id = \"n\"\n\
         master_url = \"http://elsewhere:5356\"\n",
        server
    ))
    .validate()
    .expect_err("a master with a master_url was accepted");

    assert!(
        master_errors
            .iter()
            .any(|error| error.contains("must not be set")),
        "{:?}",
        master_errors
    );
}

#[test]
fn validate_refuses_to_share_one_port_between_the_two_listeners() {
    // Caught here rather than left to a confusing "address already in use":
    // the whole point of the split is that these are two different doors.
    let (_store, server) = server_section();

    let errors = parse(&format!(
        "{}\n[replication]\nrole = \"master\"\nsecret = \"s\"\nnode_id = \"n\"\nport = 5355\n",
        server
    ))
    .validate()
    .expect_err("a shared port was accepted");

    assert!(
        errors
            .iter()
            .any(|error| error.contains("replication.port")),
        "{:?}",
        errors
    );
}

#[test]
fn validate_holds_a_node_id_to_the_same_rule_peers_apply_on_receipt() {
    let (_store, server) = server_section();

    let errors = parse(&format!(
        "{}\n[replication]\nrole = \"master\"\nsecret = \"s\"\nnode_id = \"bad id\"\n",
        server
    ))
    .validate()
    .expect_err("an invalid node id was accepted");

    assert!(
        errors.iter().any(|error| error.contains("node_id")),
        "{:?}",
        errors
    );
}

#[test]
fn validate_rejects_a_maximum_pack_threshold_below_two() {
    // One pack and no loose objects is already consolidated, so a threshold
    // of 1 would arm a no-op pass after every write.
    let (_store, server) = server_section();

    let errors = parse(&format!("{}\n[maintenance]\nmaximum_packs = 1\n", server))
        .validate()
        .expect_err("maximum_packs = 1 was accepted");

    assert!(
        errors.iter().any(|error| error.contains("maximum_packs")),
        "{:?}",
        errors
    );

    let (_store, server) = server_section();

    assert!(
        parse(&format!("{}\n[maintenance]\nmaximum_packs = 2\n", server))
            .validate()
            .is_ok()
    );
}

#[test]
fn validate_rejects_a_zero_batch_read_cap() {
    let (_store, server) = server_section();

    let errors = parse(&format!(
        "{}\n[limits]\nbatch_read_maximum_files = 0\n",
        server
    ))
    .validate()
    .expect_err("a zero batch cap was accepted");

    assert!(
        errors
            .iter()
            .any(|error| error.contains("batch_read_maximum_files")),
        "{:?}",
        errors
    );
}

#[test]
fn the_shipped_config_files_parse_and_validate() {
    // The three configs in the repository are what the Docker image, the
    // Debian package, and the dev master/replica pair install. A change that
    // breaks one of them breaks a deployment, not a test.
    for path in ["config.toml", "config.master.toml", "config.replica.toml"] {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("cannot read {}: {}", path, err));

        let config: Config =
            toml::from_str(&raw).unwrap_or_else(|err| panic!("{} does not parse: {}", path, err));

        config
            .validate()
            .unwrap_or_else(|errors| panic!("{} is invalid: {:?}", path, errors));
    }
}

#[test]
fn the_shipped_standalone_config_opens_no_replication_surface() {
    // Documented promise: the shipped default holds no replication secret
    // and opens no peer listener, so becoming a master is always a choice.
    let raw = std::fs::read_to_string("config.toml").expect("cannot read config.toml");
    let config: Config = toml::from_str(&raw).expect("config.toml does not parse");

    assert!(config.replication.is_none());
}
