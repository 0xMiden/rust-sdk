//! Invitation codes the account allowlist tests register accounts with.
//!
//! `scripts/start-test-node.sh` seeds the node's allowlist with a pool of unbound invitation codes
//! when `MIDEN_ACCOUNT_ALLOWLIST=1`, and writes the plaintext codes to the file named by
//! [`INVITATION_CODES_ENV`]. A test claims one code per account it registers.
//!
//! A code binds to the first account that presents it and cannot be reused, so a claim must outlive
//! the test that took it. The claim is therefore a marker file created exclusively, not an advisory
//! lock: an advisory lock releases when the test ends and would hand a consumed code to the next
//! caller. This also keeps a retried test correct, because the retry claims a fresh code instead of
//! the consumed one that its previous attempt took.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

// CONSTANTS
// ================================================================================================

/// Env var naming the file of seeded invitation codes, one code per line.
pub const INVITATION_CODES_ENV: &str = "MIDEN_INVITATION_CODES_FILE";

/// Directory of claim markers, next to the codes file. `start-test-node.sh` creates it when it
/// seeds the pool and clears it when the node restarts.
const CLAIMS_DIR_NAME: &str = "invitation-claims";

// INVITATION POOL
// ================================================================================================

/// The pool of invitation codes seeded for this node.
#[derive(Debug, Clone)]
pub struct InvitationPool {
    codes: Vec<String>,
    claims_dir: PathBuf,
}

impl InvitationPool {
    /// Loads the pool named by [`INVITATION_CODES_ENV`].
    ///
    /// Fails when the env var is unset or the file is missing, because a test that needs a code
    /// cannot run against a node that was started without allowlist enforcement.
    pub fn from_env() -> Result<Self> {
        let path = std::env::var_os(INVITATION_CODES_ENV).map(PathBuf::from).with_context(|| {
            format!(
                "{INVITATION_CODES_ENV} is not set; start the node with MIDEN_ACCOUNT_ALLOWLIST=1"
            )
        })?;

        Self::load(&path)
    }

    /// Loads the pool at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read invitation codes from {}; start the node with \
                 MIDEN_ACCOUNT_ALLOWLIST=1",
                path.display()
            )
        })?;

        let codes: Vec<String> = contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();
        if codes.is_empty() {
            bail!("no invitation codes in {}", path.display());
        }

        let claims_dir = path.parent().unwrap_or_else(|| Path::new(".")).join(CLAIMS_DIR_NAME);

        Ok(Self { codes, claims_dir })
    }

    /// Claims a code that no other test has taken.
    ///
    /// The claim is permanent for the life of the node, so each call yields a code that is still
    /// unbound on the node's allowlist.
    pub fn claim(&self) -> Result<String> {
        std::fs::create_dir_all(&self.claims_dir).with_context(|| {
            format!("failed to create claims directory {}", self.claims_dir.display())
        })?;

        for (index, code) in self.codes.iter().enumerate() {
            let marker = self.claims_dir.join(index.to_string());
            // `create_new` fails when the file is already there, which makes the claim atomic
            // across the test processes nextest runs in parallel.
            match std::fs::File::options().create_new(true).write(true).open(&marker) {
                Ok(_) => return Ok(code.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(anyhow::Error::new(err)
                        .context(format!("failed to claim invitation code {}", marker.display())));
                },
            }
        }

        bail!(
            "every one of the {} seeded invitation codes is claimed; raise INVITATION_POOL_SIZE in \
             scripts/start-test-node.sh",
            self.codes.len()
        )
    }
}
