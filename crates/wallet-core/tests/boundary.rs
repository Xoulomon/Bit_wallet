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

/// §3a: the bot is a *client*. It may hold a `Txid` or an `Amount`, but a
/// `bdk_wallet` type, a `Psbt` or a `Mnemonic` in that crate means a capability
/// grew around the facade instead of onto it.
#[test]
fn the_bot_crate_holds_no_wallet_internals() {
    let bot_src = workspace_root().join("crates/bot/src");
    if !bot_src.exists() {
        // The "delete the front end and the wallet still builds" scenario.
        return;
    }

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
    visit(&bot_src, &mut |path, text| {
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
        "wallet internals leaked into the bot crate:\n{}",
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
