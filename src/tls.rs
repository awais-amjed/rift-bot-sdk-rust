/// Make ring this process's TLS provider, once.
///
/// rustls refuses to choose when more than one provider is compiled in, and
/// panics on the first connection instead. LiveKit's crates bring ring; this
/// crate asks for nothing else, but a bot's own dependencies may bring
/// aws-lc-rs, so the choice is made here rather than left to chance. A
/// provider the bot installed itself first is kept.
pub(crate) fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
