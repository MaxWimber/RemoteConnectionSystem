//! Wire formats shared by the agent, the web server and the tests.

pub mod agent;

/// Protocol version from `docs/PROTOCOL.md`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Build identifier set by `scripts/package.ps1`.
pub const BUILD_ID: &str = match option_env!("RCS_BUILD_ID") {
    Some(id) => id,
    None => "dev",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_builds_have_a_build_id() {
        assert!(!BUILD_ID.is_empty());
    }

    #[test]
    fn protocol_version_matches_the_document() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
