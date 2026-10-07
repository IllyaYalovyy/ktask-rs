//! Which world a build belongs to.

/// The world a binary was built for, fixed when it was built: `dev` for every build from
/// the repository, `user` only for the installed tool. Each has roots of its own, so neither
/// can reach the other's queues.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Channel {
    /// A build from the repository: `cargo build`, `cargo test`, `cargo run`, `cargo install`.
    #[default]
    Dev,
    /// The installed tool, the one that runs the real queue.
    User,
}

impl Channel {
    /// The word the channel goes by wherever it is shown or written: `dev` or `user`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::User => "user",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_goes_by_its_name_and_a_plain_build_is_dev() {
        assert_eq!(Channel::Dev.name(), "dev");
        assert_eq!(Channel::User.name(), "user");
        assert_eq!(Channel::default(), Channel::Dev);
    }
}
