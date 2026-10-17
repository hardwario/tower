//! End-to-end CLI tests: drive the compiled `tower` binary with no hardware attached, so
//! they exercise argument parsing, the `--help`/usage surface, and the exit-code contract
//! (see `main.rs`: 0 ok, 1 tool error, 2 usage). Anything that would open a serial port is
//! avoided: `devices` merely enumerates ports (fine with none), and the bad-argument cases
//! fail in clap before any I/O.

use assert_cmd::Command;
use predicates::prelude::*;

fn tower() -> Command {
    Command::cargo_bin("tower").expect("binary `tower` builds")
}

#[test]
fn help_lists_all_subcommands() {
    tower()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("HARDWARIO TOWER host tool"))
        .stdout(predicate::str::contains("logs"))
        .stdout(predicate::str::contains("events"))
        .stdout(predicate::str::contains("shell"))
        .stdout(predicate::str::contains("exec"))
        .stdout(predicate::str::contains("console"))
        .stdout(predicate::str::contains("chip"))
        // The bare-`tower` UX: the command is optional (no subcommand → TUI).
        .stdout(predicate::str::contains("[COMMAND]"));
}

#[test]
fn version_prints_a_version() {
    tower()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")))
        .stdout(predicate::str::contains(format!(
            "console 0x{:04x}, radio 0x{:04x}",
            tower_protocol::CONSOLE_TAG,
            tower_protocol::radio::RADIO_TAG
        )));
}

#[test]
fn delay_without_reset_is_a_usage_error() {
    // `--delay` `requires = "reset"`, so this must be a clap usage error (exit 2), not a run.
    tower()
        .args(["logs", "--delay", "100"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--reset"));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    tower().arg("definitely-not-a-command").assert().code(2);
}

#[test]
fn color_rejects_invalid_value() {
    // `--color` is an enum {auto,always,never}; anything else is a usage error.
    tower().args(["logs", "--color", "rainbow"]).assert().code(2);
}

#[test]
fn devices_runs_with_no_hardware() {
    // Enumerating ports must succeed (exit 0) even when none are present; it's the one
    // command that touches the serial subsystem but never opens a port.
    tower().arg("devices").assert().success();
}

#[test]
fn nonexistent_device_first_open_is_fatal() {
    // A bad `--device` must exit 1 (tool error), not spin forever in the reconnect loop.
    tower()
        .args(["logs", "--device", "/dev/tower-cli-test-nonexistent"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error"));
}

#[test]
fn console_nonexistent_device_first_open_is_fatal() {
    // The console TUI honours the same "first open is fatal" contract: a bad `--device` must
    // fail before ratatui takes the terminal (so the error prints normally and it exits 1),
    // rather than opening an empty four-pane UI that reconnects forever.
    tower()
        .args(["console", "--device", "/dev/tower-cli-test-nonexistent"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error"));
}

#[test]
fn chip_nonexistent_device_is_a_tool_error() {
    // `tower chip` opens the port before any bootloader traffic, so a bad `--device` fails fast
    // with exit 1 (tool error) and never touches hardware.
    tower()
        .args(["chip", "--device", "/dev/tower-cli-test-nonexistent"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error"));
}

// ---- readout protection: protect / unprotect (no hardware) -----------------------
//
// assert_cmd gives the child a non-terminal stdin, which is exactly the scripted case: without
// `--yes` both commands must refuse before touching any port (the bogus `--device` would
// otherwise fail with "opening ..."), and with `--yes` they must get past the confirmation to
// the port open (which then fails on the bogus device, so nothing ever reaches hardware).

const NO_DEVICE: &str = "/dev/tower-cli-test-nonexistent";

#[test]
fn help_lists_protect_and_unprotect() {
    tower()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("protect"))
        .stdout(predicate::str::contains("unprotect"));
}

#[test]
fn protect_help_documents_yes() {
    tower()
        .args(["protect", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("RDP level 1"))
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn unprotect_help_warns_about_the_erase() {
    tower()
        .args(["unprotect", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ERASES"))
        .stdout(predicate::str::contains("EEPROM"))
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn protect_and_unprotect_reject_unknown_flags() {
    tower().args(["protect", "--level", "2"]).assert().code(2);
    tower().args(["unprotect", "--force"]).assert().code(2);
}

#[test]
fn protect_refuses_a_non_terminal_stdin_without_yes() {
    tower()
        .args(["protect", "--device", NO_DEVICE])
        .write_stdin("yes\n") // even a correct answer on a pipe is not a confirmation
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a terminal"))
        .stderr(predicate::str::contains("--yes"))
        .stderr(predicate::str::contains("opening").not());
}

#[test]
fn unprotect_refuses_a_non_terminal_stdin_without_yes() {
    tower()
        .args(["unprotect", "--device", NO_DEVICE])
        .write_stdin("erase\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a terminal"))
        .stderr(predicate::str::contains("--yes"))
        .stderr(predicate::str::contains("opening").not());
}

#[test]
fn protect_and_unprotect_with_yes_skip_the_prompt() {
    for cmd in ["protect", "unprotect"] {
        tower()
            .args([cmd, "--yes", "--device", NO_DEVICE])
            .assert()
            .code(1)
            .stderr(predicate::str::contains(format!("opening {NO_DEVICE}")))
            .stderr(predicate::str::contains("not a terminal").not())
            .stderr(predicate::str::contains("Type \"").not());
    }
}

// ---- gateway / nodes / net surfaces (no hardware, no broker) ---------------------

#[test]
fn gateway_help_lists_modes() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["gateway", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--service"))
        .stdout(predicates::str::contains("--mqtt"))
        .stdout(predicates::str::contains("--prefix"));
}

#[test]
fn nodes_help_lists_subcommands() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["nodes", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("list"))
        .stdout(predicates::str::contains("add"))
        .stdout(predicates::str::contains("shell"))
        .stdout(predicates::str::contains("dequeue"));
}

#[cfg(feature = "embedded-broker")]
#[test]
fn gateway_broker_conflicts_with_mqtt() {
    // Usage errors are clap's: exit 2.
    Command::cargo_bin("tower")
        .unwrap()
        .args(["gateway", "--broker", "127.0.0.1:1883", "--mqtt", "host:1883"])
        .assert()
        .code(2);
}

#[test]
fn nodes_list_fails_fast_without_broker() {
    // Port 1 is never a broker: a clean tool error (1), not a hang.
    Command::cargo_bin("tower")
        .unwrap()
        .args(["nodes", "list", "--mqtt", "127.0.0.1:1", "--timeout", "500"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error"));
}

#[test]
fn net_status_fails_fast_without_broker() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["net", "status", "--mqtt", "127.0.0.1:1", "--timeout", "500"])
        .assert()
        .code(1);
}

#[cfg(feature = "embedded-broker")]
#[test]
fn gateway_refuses_an_unauthenticated_non_loopback_broker() {
    // The access policy: a loopback broker is anonymous, any other bind needs a login. The
    // check runs before the serial port is opened, so this needs no hardware; the port given
    // is never touched. Clear the credential env vars so an operator's shell can't leak in.
    tower()
        .args([
            "gateway",
            "--service",
            "--broker",
            "0.0.0.0:18839",
            "-d",
            "/dev/null-tower-test",
        ])
        .env_remove("TOWER_MQTT_USER")
        .env_remove("TOWER_MQTT_PASSWORD")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("without authentication"));
}

// ---- MQTT over TLS: configuration errors, all caught before any port or socket is touched ----

#[cfg(feature = "embedded-broker")]
#[test]
fn gateway_refuses_a_plaintext_non_loopback_broker_even_with_a_login() {
    // A login alone is not enough off loopback: the password and the node keys would cross the
    // network in clear text. The refusal names both ways out and never echoes the password.
    tower()
        .args([
            "gateway",
            "--service",
            "--broker",
            "0.0.0.0:18839",
            "-d",
            "/dev/null-tower-test",
        ])
        .env("TOWER_MQTT_USER", "ops")
        .env("TOWER_MQTT_PASSWORD", "s3cret-test")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("without TLS"))
        .stderr(predicate::str::contains("--broker-tls-identity"))
        .stderr(predicate::str::contains("--broker-allow-plaintext"))
        .stderr(predicate::str::contains("s3cret-test").not());
}

#[cfg(feature = "embedded-broker")]
#[test]
fn gateway_checks_the_broker_tls_identity_before_the_port() {
    tower()
        .args([
            "gateway",
            "--service",
            "--broker-tls-identity",
            "/nonexistent-tower-test/broker.p12",
            "-d",
            "/dev/null-tower-test",
        ])
        .env("TOWER_BROKER_TLS_PASSWORD", "p12-secret-test")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--broker-tls-identity"))
        .stderr(predicate::str::contains("broker.p12"))
        .stderr(predicate::str::contains("p12-secret-test").not());
}

#[cfg(feature = "embedded-broker")]
#[test]
fn gateway_tls_flag_conflicts_are_usage_errors() {
    for args in [
        // The embedded broker's TLS and an external broker exclude each other.
        &["gateway", "--broker-tls-identity", "x.p12", "--mqtt", "mqtts://h"][..],
        // Waiving TLS while serving it is contradictory.
        &[
            "gateway",
            "--broker-tls-identity",
            "x.p12",
            "--broker-allow-plaintext",
        ][..],
    ] {
        tower().args(args).assert().code(2);
    }
}

#[test]
fn gateway_client_tls_flags_need_an_external_broker() {
    // --mqtt-tls / --mqtt-ca / --mqtt-client-identity configure the link to --mqtt; without it
    // they would silently do nothing.
    for flag in [
        &["--mqtt-tls"][..],
        &["--mqtt-ca", "ca.pem"],
        &["--mqtt-client-identity", "id.p12"],
    ] {
        tower().arg("gateway").args(flag).assert().code(2);
    }
}

#[test]
fn client_tls_files_need_a_tls_link() {
    // A CA on a plain link is a mistake worth saying, not a flag to ignore.
    tower()
        .args([
            "nodes",
            "list",
            "--mqtt",
            "broker.invalid:1883",
            "--mqtt-ca",
            "ca.pem",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("mqtts://"));
}

#[test]
fn client_tls_unreadable_ca_is_named() {
    tower()
        .args([
            "net",
            "status",
            "--mqtt",
            "mqtts://broker.invalid",
            "--mqtt-ca",
            "/nonexistent-tower-test/ca.pem",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--mqtt-ca"))
        .stderr(predicate::str::contains("ca.pem"));
}

#[test]
fn mqtt_address_with_a_login_is_refused_without_echoing_it() {
    tower()
        .args([
            "nodes",
            "list",
            "--mqtt",
            "mqtts://ops:s3cret-test@broker.invalid",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--mqtt-user"))
        .stderr(predicate::str::contains("s3cret-test").not());
}
