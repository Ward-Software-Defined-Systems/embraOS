//! Certificate authorities on a running machine: what the `/ca` command
//! reads and writes.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the /ca command of the commit that follows is the reader's caller")
)]
pub(crate) mod cert;
