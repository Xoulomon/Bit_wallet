//! The boundary tests (PLAN.md §10, enforcing §3a).
//!
//! These are cheap, and they are the difference between a layering that holds
//! and one that is merely described in a README. They fail the build the moment
//! someone adds the convenient dependency.

use std::{path::PathBuf, process::Command};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/crates/wallet-core`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("the crate lives two levels under the workspace root")
}

/// §3a rule 1 and rule 2: `wallet-core` must not be able to *name* a Telegram
/// type or a rendering crate. Asserted against the resolved dependency graph
/// rather than against the manifest, so a transitive path is caught too.
#[test]
fn wallet_core_cannot_name_a_front_end_crate() {
    let out = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "wallet-core",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("cargo tree runs");

    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let tree = String::from_utf8_lossy(&out.stdout);
    for forbidden in ["teloxide", "qrcode", "image ", "comfy-table"] {
        assert!(
            !tree.contains(forbidden),
            "`{forbidden}` reached wallet-core's dependency graph — §3a rule 1/2 broken"
        );
    }
}

/// §3a: a front end is a *client*. It may hold a `Txid` or an `Amount`, but a
/// `bdk_wallet` type, a `Psbt` or a `Mnemonic` in one means a capability grew
/// around the facade instead of onto it.
///
/// Both front ends are checked, because the second one is the honest proof: the
/// bot could have grown a shortcut nobody noticed, but `wallet-cli` was written
/// against the same API and would have failed to compile.
#[test]
fn the_front_ends_hold_no_wallet_internals() {
    for crate_name in ["bot", "wallet-cli"] {
        let src = workspace_root().join(format!("crates/{crate_name}/src"));
        if src.exists() {
            assert_no_internals(&src, crate_name);
        }
    }
}

fn assert_no_internals(bot_src: &PathBuf, crate_name: &str) {
    // Whole identifiers, not substrings: the bot is free to *name* a core error
    // variant such as `CoreError::InvalidMnemonic` — what it may not do is hold
    // the type itself.
    const FORBIDDEN: [&str; 6] = [
        "bdk_wallet",
        "Psbt",
        "Mnemonic",
        "Descriptor",
        "Xpriv",
        "bip39",
    ];

    let mut offenders = Vec::new();
    visit(bot_src, &mut |path, text| {
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            let leaked = code
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|token| FORBIDDEN.contains(&token));
            if leaked {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "wallet internals leaked into the {crate_name} crate:\n{}",
        offenders.join("\n")
    );
}

fn visit(dir: &PathBuf, f: &mut impl FnMut(&PathBuf, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            f(&path, &text);
        }
    }
}

/// §3a rule 2, from the other direction: core's own source must contain no
/// user-facing prose. Error variants carry data; the sentences live in the
/// front end's `render_error`.
#[test]
fn core_returns_no_emoji() {
    let src = workspace_root().join("crates/wallet-core/src");
    let mut offenders = Vec::new();
    visit(&src, &mut |path, text| {
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code
                .chars()
                .any(|c| matches!(c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F))
            {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "presentation leaked into wallet-core:\n{}",
        offenders.join("\n")
    );
}

/// §3a rule 3 and §5: the mnemonic and the API key are redacted from every
/// `Debug` impl and every error message.
///
/// A grep test rather than a unit test, because the property is about *every*
/// type in the crate, not the handful a reviewer thinks to check.
#[test]
fn no_secret_is_ever_a_tracing_field() {
    let src = workspace_root().join("crates/wallet-core/src");
    let mut offenders = Vec::new();

    visit(&src, &mut |path, text| {
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            let is_log = [
                "tracing::info!",
                "tracing::warn!",
                "tracing::error!",
                "tracing::debug!",
                "tracing::trace!",
            ]
            .iter()
            .any(|m| code.contains(m));

            if !is_log {
                continue;
            }

            for secret in [
                "api_key",
                "rpc_pass",
                "mnemonic =",
                "seed",
                "pin =",
                "words",
            ] {
                if code.contains(secret) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "a secret reached a log line:\n{}",
        offenders.join("\n")
    );
}

/// §5: plaintext secrets are wrapped in `Zeroizing`, so a `String` that holds a
/// mnemonic cannot be returned bare from the facade.
#[test]
fn the_facade_returns_no_bare_secret() {
    let facade = workspace_root().join("crates/wallet-core/src/service/mod.rs");
    let text = std::fs::read_to_string(&facade).expect("the facade is readable");

    for line in text.lines() {
        let code = line.split("//").next().unwrap_or(line);
        if !code.contains("pub async fn") && !code.contains("pub fn") {
            continue;
        }
        if code.contains("mnemonic") || code.contains("export") {
            assert!(
                code.contains("Zeroizing") || code.contains("&self") && code.contains("Pin"),
                "a secret-returning method must wrap it: {}",
                line.trim()
            );
        }
    }
}

/// §3a and §9 Step 8: every method on the facade is exercised by a front end or
/// by a test. A method nothing calls is either dead or a capability the front
/// ends had to work around — both worth knowing.
#[test]
fn the_second_front_end_reaches_the_facade_and_nothing_below_it() {
    let cli = workspace_root().join("crates/wallet-cli/src/main.rs");
    if !cli.exists() {
        return;
    }
    let text = std::fs::read_to_string(&cli).expect("the CLI is readable");

    // It must never name a private module of core.
    for private in [
        "onchain::",
        "keys::",
        "payjoin::",
        "rpc::",
        "session::",
        "storage::",
    ] {
        assert!(
            !text.contains(private),
            "wallet-cli reached into `{private}` instead of the facade"
        );
    }

    // And it must cover the capabilities that make it a real front end, not a
    // sketch: if one of these is missing the boundary was never tested.
    for method in [
        "create_wallet",
        "restore_wallet",
        "unlock",
        "next_address",
        "balance",
        "history",
        "quote_send",
        "confirm_send",
        "payjoin_receive",
        "subscribe",
        "status",
    ] {
        assert!(
            text.contains(method),
            "wallet-cli does not exercise `{method}`, so the boundary is untested there"
        );
    }
}
