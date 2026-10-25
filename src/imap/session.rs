//! One authenticated IMAP connection and the few operations v1 needs.
//!
//! [`ImapSession`] wraps `async_imap::Session`. The wrapper exists so the rest of the program
//! never sees the crate's types, only plain values: strings, numbers and our own structs. It
//! also makes the hard rules impossible to break by accident: there are only `PEEK` fetches, and
//! no way to delete, move or expunge.
//!
//! The functions at the top of the file are pure (no IO) and unit-tested. The `impl ImapSession`
//! block below them is the async part that needs a real server; `tests/imap_integration.rs`
//! covers it.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use async_imap::types::{Capabilities, Capability, Flag, Mailbox, NameAttribute};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;

use super::folders::{ListedFolder, ResolvedFolders, resolve_folders};
use super::{ImapError, ImapSettings, TlsMode};
use crate::flags::StoreChange;

/// How long connecting, the TLS handshake and the greeting may take together.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// The fetch queries live in constants so they are easy to audit. Every body fetch must use the
// `PEEK` form: the plain form marks the message as read. `unit test no_plain_body_fetches`
// scans the whole source tree for violations.
const META_QUERY: &str = "(UID RFC822.SIZE FLAGS)";
const BODY_PEEK_QUERY: &str = "(UID BODY.PEEK[])";
const HEADERS_PEEK_QUERY: &str = "(UID BODY.PEEK[HEADER])";
// ENVELOPE is the server's own parsed summary of the header (RFC 3501 §7.4.2). It never
// includes the body, and fetching it does not set `\Seen`.
const ENVELOPE_QUERY: &str = "(UID ENVELOPE)";

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// What the server told us it can do (the `CAPABILITY` response).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    /// `IDLE` (RFC 2177): the server can push "new mail" notifications.
    pub idle: bool,
    /// `UIDPLUS` (RFC 4315): needed for a safe `UID EXPUNGE`; unused in v1.
    pub uidplus: bool,
    /// `MOVE` (RFC 6851); unused in v1.
    pub move_ext: bool,
    /// Every capability atom, uppercased and sorted, for the `check` command to print.
    pub all: Vec<String>,
}

impl ServerCapabilities {
    /// Build from the raw capability atoms, such as `["IMAP4rev1", "IDLE", "UIDPLUS"]`.
    ///
    /// Capability names are case-insensitive in IMAP, so everything is uppercased first.
    pub fn from_atoms<'a>(atoms: impl IntoIterator<Item = &'a str>) -> Self {
        let mut all: Vec<String> = atoms.into_iter().map(str::to_ascii_uppercase).collect();
        all.sort();
        all.dedup();
        let has = |name: &str| all.iter().any(|a| a == name);
        ServerCapabilities {
            idle: has("IDLE"),
            uidplus: has("UIDPLUS"),
            move_ext: has("MOVE"),
            all,
        }
    }

    /// Convert the crate's `Capabilities` into our plain struct.
    fn from_crate(caps: &Capabilities) -> Self {
        let atoms: Vec<String> = caps
            .iter()
            .map(|cap| match cap {
                Capability::Imap4rev1 => "IMAP4rev1".to_string(),
                Capability::Auth(mechanism) => format!("AUTH={mechanism}"),
                Capability::Atom(atom) => atom.clone(),
            })
            .collect();
        Self::from_atoms(atoms.iter().map(String::as_str))
    }
}

/// The state of a selected mailbox that the service cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxInfo {
    /// `UIDVALIDITY`: if this changes, every stored UID is meaningless and state must be reset.
    pub uid_validity: u32,
    /// `UIDNEXT`: the UID the next arriving message will get.
    pub uid_next: u32,
    /// Number of messages in the mailbox.
    pub exists: u32,
    /// The `PERMANENTFLAGS` the server lists, as text (`\Seen`, `\*`, `$Junk`, ...).
    pub permanent_flags: Vec<String>,
    /// True if `PERMANENTFLAGS` contains `\*`, which means clients may create new keywords such
    /// as `$MailFlagBit0`. Without it, flag colours cannot be stored.
    pub keywords_allowed: bool,
}

/// `\*` in `PERMANENTFLAGS` means "the client may create new keywords" (RFC 3501 §2.3.2).
fn keywords_allowed(permanent_flags: &[String]) -> bool {
    permanent_flags.iter().any(|flag| flag == "\\*")
}

/// The text form of a flag as it appears on the wire.
fn flag_name(flag: &Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        // The crate models the `\*` of PERMANENTFLAGS as `MayCreate`.
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(name) => name.to_string(),
    }
}

/// Turn the crate's `Mailbox` (the answer to `SELECT`) into our struct.
fn mailbox_info(mailbox: &Mailbox) -> Result<MailboxInfo, ImapError> {
    let permanent_flags: Vec<String> = mailbox.permanent_flags.iter().map(flag_name).collect();
    Ok(MailboxInfo {
        uid_validity: mailbox
            .uid_validity
            .ok_or(ImapError::MissingData("UIDVALIDITY"))?,
        uid_next: mailbox.uid_next.ok_or(ImapError::MissingData("UIDNEXT"))?,
        exists: mailbox.exists,
        keywords_allowed: keywords_allowed(&permanent_flags),
        permanent_flags,
    })
}

/// A comma-separated UID set such as `4,9,12`, ready for `UID FETCH` or `UID STORE`.
fn uid_set(uids: &[u32]) -> Result<String, ImapError> {
    if uids.is_empty() {
        return Err(ImapError::EmptyUidSet);
    }
    let parts: Vec<String> = uids.iter().map(u32::to_string).collect();
    Ok(parts.join(","))
}

/// The `STORE` arguments for a flag change: at most one `-FLAGS` and one `+FLAGS`.
///
/// Stale bits are removed first. Adding first would briefly show old and new bits together,
/// which can read as a third colour (orange + yellow bits look like green). `.SILENT` asks the
/// server not to echo the new flags; we do not need them.
fn store_queries(change: &StoreChange) -> Vec<String> {
    let mut queries = Vec::new();
    if !change.remove.is_empty() {
        queries.push(format!("-FLAGS.SILENT ({})", change.remove.join(" ")));
    }
    if !change.add.is_empty() {
        queries.push(format!("+FLAGS.SILENT ({})", change.add.join(" ")));
    }
    queries
}

/// Convert one entry of a `LIST` response into our plain struct.
fn listed_folder(name: &async_imap::types::Name) -> ListedFolder {
    ListedFolder {
        name: name.name().to_string(),
        delimiter: name.delimiter().map(str::to_string),
        attributes: name
            .attributes()
            .iter()
            .filter_map(attribute_text)
            .collect(),
    }
}

/// The wire spelling of a `LIST` attribute (`\Sent`, `\Noselect`, ...).
fn attribute_text(attribute: &NameAttribute<'_>) -> Option<String> {
    let text = match attribute {
        NameAttribute::NoInferiors => "\\Noinferiors",
        NameAttribute::NoSelect => "\\Noselect",
        NameAttribute::Marked => "\\Marked",
        NameAttribute::Unmarked => "\\Unmarked",
        NameAttribute::All => "\\All",
        NameAttribute::Archive => "\\Archive",
        NameAttribute::Drafts => "\\Drafts",
        NameAttribute::Flagged => "\\Flagged",
        NameAttribute::Junk => "\\Junk",
        NameAttribute::Sent => "\\Sent",
        NameAttribute::Trash => "\\Trash",
        NameAttribute::Extension(other) => return Some(other.to_string()),
        // The crate marks this enum `#[non_exhaustive]`, so a future version may add variants.
        // We cannot spell them, so we skip them; we only act on `\Sent` and `\Noselect`.
        _ => return None,
    };
    Some(text.to_string())
}

// ---------------------------------------------------------------------------------------------
// Transport: plain TCP or TLS behind one type
// ---------------------------------------------------------------------------------------------

/// A TCP stream that may or may not be wrapped in TLS.
///
/// `async_imap::Session<T>` is generic over the stream type `T`. Implicit TLS, STARTTLS and
/// plain test connections each produce a different concrete type, so we put them in one enum
/// and the rest of the program sees a single `Session<MaybeTls>`. The trait impls below just
/// forward every call to whichever variant is inside.
#[derive(Debug)]
pub enum MaybeTls {
    /// Unencrypted (loopback tests, or the first phase of STARTTLS).
    Plain(TcpStream),
    /// Encrypted. Boxed because the TLS state is much bigger than a bare socket.
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Both variants are `Unpin`, so `Pin::new` on a plain `&mut` is allowed.
        match self.get_mut() {
            MaybeTls::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            MaybeTls::Tls(stream) => Pin::new(&mut **stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(stream) => Pin::new(stream).poll_write(cx, data),
            MaybeTls::Tls(stream) => Pin::new(&mut **stream).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(stream) => Pin::new(stream).poll_flush(cx),
            MaybeTls::Tls(stream) => Pin::new(&mut **stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            MaybeTls::Tls(stream) => Pin::new(&mut **stream).poll_shutdown(cx),
        }
    }
}

/// Run the TLS handshake, verifying the server certificate against the system trust store.
async fn tls_handshake(tcp: TcpStream, host: &str) -> Result<TlsStream<TcpStream>, ImapError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        // A single unparsable certificate in the system store should not break us.
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(ImapError::NoRootCertificates);
    }

    // The `ring` crypto provider is named explicitly instead of relying on a process-wide
    // default, which would be a hidden global.
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| ImapError::Tls(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| ImapError::InvalidServerName(host.to_string()))?;

    TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .map_err(|e| ImapError::Tls(e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------------------------

/// Size and flags of one message, fetched without its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMeta {
    /// The message's UID in the selected mailbox.
    pub uid: u32,
    /// `RFC822.SIZE` in bytes, if the server reported it.
    pub size: Option<u32>,
    /// Current flags, such as `\Flagged` and `$MailFlagBit0`.
    pub flags: Vec<String>,
}

/// Why an [`ImapSession::idle`] wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEnd {
    /// The server pushed something (usually `EXISTS`: new mail). Look at the mailbox.
    NewData,
    /// The timeout passed with nothing happening. The caller re-issues IDLE.
    Timeout,
    /// The cancellation token fired.
    Cancelled,
}

/// The recipients of one message, as lowercase `local@domain` addresses.
///
/// Built from the `ENVELOPE` of a message in the Sent folder. Addresses that cannot be decoded
/// are left out rather than failing the whole fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvelopeAddresses {
    /// The message's UID in the selected mailbox.
    pub uid: u32,
    /// Everyone in To, Cc and Bcc, in that order, lowercase. Duplicates are not removed.
    pub recipients: Vec<String>,
}

/// Join the two halves of an envelope address into `local@domain`, lowercase.
///
/// IMAP sends an address as `(name adl mailbox host)`. A group such as `Team: a@x.org, b@x.org;`
/// is sent as a start marker (`mailbox` = group name, `host` = NIL), the members, and an end
/// marker (both NIL). Requiring *both* parts makes the markers fall out naturally, while the
/// members are ordinary addresses. Bytes that are not valid UTF-8 are skipped: an old message
/// with a Latin-1 address is not worth an error.
pub(crate) fn envelope_address(mailbox: Option<&[u8]>, host: Option<&[u8]>) -> Option<String> {
    let local = std::str::from_utf8(mailbox?).ok()?.trim();
    let domain = std::str::from_utf8(host?).ok()?.trim();
    let bad =
        |text: &str| text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad(local) || bad(domain) || domain.contains('@') {
        return None;
    }
    Some(format!("{local}@{domain}").to_lowercase())
}

/// An authenticated connection to one IMAP account.
pub struct ImapSession {
    inner: Session<MaybeTls>,
    /// What the last `SELECT` told us; `store_flags` consults it before touching keywords.
    selected: Option<MailboxInfo>,
}

impl ImapSession {
    /// Connect, secure the connection as `settings.tls` says, and log in.
    ///
    /// Refuses to send the password over plain text to a non-loopback host.
    pub async fn connect(settings: &ImapSettings) -> Result<ImapSession, ImapError> {
        settings.validate()?;
        // `timeout` wraps a future and gives up when the time is up. Without it, a server that
        // accepts the TCP connection and then says nothing would hang this task forever.
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_and_login(settings))
            .await
            .map_err(|_elapsed| ImapError::Timeout)?
    }

    async fn connect_and_login(settings: &ImapSettings) -> Result<ImapSession, ImapError> {
        let tcp = TcpStream::connect((settings.host.as_str(), settings.port)).await?;

        let stream = match settings.tls {
            TlsMode::Implicit => MaybeTls::Tls(Box::new(tls_handshake(tcp, &settings.host).await?)),
            TlsMode::StartTls | TlsMode::PlainLoopbackOnly => MaybeTls::Plain(tcp),
        };
        let mut client = Client::new(stream);

        // The server speaks first. This also proves the other end is an IMAP server.
        client
            .read_response()
            .await?
            .ok_or(ImapError::MissingData("the server greeting"))?;

        if settings.tls == TlsMode::StartTls {
            client = upgrade_with_starttls(client, &settings.host).await?;
        }

        // `login` hands the unauthenticated client back on failure so the caller could retry;
        // we have no use for it, hence `(error, _client)`.
        let mut inner = client
            .login(&settings.username, &settings.password)
            .await
            .map_err(|(error, _client)| ImapError::Login(error))?;

        // Capabilities can differ before and after login, so ask again now.
        let _ = inner.capabilities().await?;
        Ok(ImapSession {
            inner,
            selected: None,
        })
    }

    /// Ask the server what it supports.
    pub async fn capabilities(&mut self) -> Result<ServerCapabilities, ImapError> {
        let caps = self.inner.capabilities().await?;
        Ok(ServerCapabilities::from_crate(&caps))
    }

    /// Select the inbox (read-write, because we set flags) and report its state.
    pub async fn select_inbox(&mut self) -> Result<MailboxInfo, ImapError> {
        self.select("INBOX").await
    }

    /// Select any folder by its server-side name (as returned by [`resolve_folders`]).
    pub async fn select(&mut self, folder: &str) -> Result<MailboxInfo, ImapError> {
        let mailbox = self.inner.select(folder).await?;
        let info = mailbox_info(&mailbox)?;
        self.selected = Some(info.clone());
        Ok(info)
    }

    /// List all folders and work out the inbox and Sent folder.
    pub async fn resolve_folders(&mut self) -> Result<ResolvedFolders, ImapError> {
        let names: Vec<_> = self
            .inner
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;
        let listed: Vec<ListedFolder> = names.iter().map(listed_folder).collect();
        Ok(resolve_folders(&listed))
    }

    /// Size and flags for the given UIDs in the selected mailbox. Messages that no longer
    /// exist are simply absent from the result.
    pub async fn fetch_meta(&mut self, uids: &[u32]) -> Result<Vec<MessageMeta>, ImapError> {
        let set = uid_set(uids)?;
        let fetches: Vec<_> = self
            .inner
            .uid_fetch(set, META_QUERY)
            .await?
            .try_collect()
            .await?;

        fetches
            .iter()
            .map(|fetch| {
                Ok(MessageMeta {
                    uid: fetch.uid.ok_or(ImapError::MissingData("UID"))?,
                    size: fetch.size,
                    flags: fetch.flags().map(|f| flag_name(&f)).collect(),
                })
            })
            .collect()
    }

    /// Same as [`fetch_meta`](Self::fetch_meta), for every UID in `first..=last`.
    ///
    /// A range keeps the command short. The result is sorted by UID and only holds UIDs inside
    /// the range: for a range such as `5:*`, a server answers with the *last* message even when
    /// nothing is above 5 (RFC 3501 §6.4.8), and callers must not mistake that for new mail.
    pub async fn fetch_meta_range(
        &mut self,
        first: u32,
        last: u32,
    ) -> Result<Vec<MessageMeta>, ImapError> {
        let fetches: Vec<_> = self
            .inner
            .uid_fetch(format!("{first}:{last}"), META_QUERY)
            .await?
            .try_collect()
            .await?;
        let mut metas = fetches
            .iter()
            .map(|fetch| {
                Ok(MessageMeta {
                    uid: fetch.uid.ok_or(ImapError::MissingData("UID"))?,
                    size: fetch.size,
                    flags: fetch.flags().map(|f| flag_name(&f)).collect(),
                })
            })
            .collect::<Result<Vec<_>, ImapError>>()?;
        metas.retain(|meta| (first..=last).contains(&meta.uid));
        metas.sort_by_key(|meta| meta.uid);
        Ok(metas)
    }

    /// To, Cc and Bcc addresses of every message with a UID in `first..=last` in the selected
    /// mailbox, fetched with `ENVELOPE` only (no body, nothing marked as read).
    ///
    /// A range is used instead of a list of UIDs because a long list makes a long command line;
    /// UIDs in the range that do not exist are simply absent from the answer.
    pub async fn fetch_envelopes(
        &mut self,
        first: u32,
        last: u32,
    ) -> Result<Vec<EnvelopeAddresses>, ImapError> {
        if self.selected.is_none() {
            return Err(ImapError::NoMailboxSelected);
        }
        let fetches: Vec<_> = self
            .inner
            .uid_fetch(format!("{first}:{last}"), ENVELOPE_QUERY)
            .await?
            .try_collect()
            .await?;

        let mut result = Vec::new();
        for fetch in &fetches {
            // A response without UID or ENVELOPE (such as an unsolicited flag update) is not
            // an error; there is just nothing to harvest from it.
            let (Some(uid), Some(envelope)) = (fetch.uid, fetch.envelope()) else {
                continue;
            };
            let recipients = [&envelope.to, &envelope.cc, &envelope.bcc]
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|address| {
                    envelope_address(address.mailbox.as_deref(), address.host.as_deref())
                })
                .collect();
            result.push(EnvelopeAddresses { uid, recipients });
        }
        Ok(result)
    }

    /// The whole raw message, fetched with `BODY.PEEK[]` so it is **not** marked as read.
    ///
    /// `None` if the message is gone (for example deleted by the user in the meantime).
    pub async fn fetch_body_peek(&mut self, uid: u32) -> Result<Option<Vec<u8>>, ImapError> {
        let fetches: Vec<_> = self
            .inner
            .uid_fetch(uid.to_string(), BODY_PEEK_QUERY)
            .await?
            .try_collect()
            .await?;
        Ok(fetches
            .iter()
            .find_map(|fetch| fetch.body())
            .map(<[u8]>::to_vec))
    }

    /// Only the raw header block, fetched with `BODY.PEEK[HEADER]`. Used for messages too big
    /// to download in full. `None` if the message is gone.
    pub async fn fetch_headers_peek(&mut self, uid: u32) -> Result<Option<Vec<u8>>, ImapError> {
        let fetches: Vec<_> = self
            .inner
            .uid_fetch(uid.to_string(), HEADERS_PEEK_QUERY)
            .await?
            .try_collect()
            .await?;
        Ok(fetches
            .iter()
            .find_map(|fetch| fetch.header())
            .map(<[u8]>::to_vec))
    }

    /// Apply a flag change to one message: a `UID STORE -FLAGS` for stale bits, then a
    /// `UID STORE +FLAGS` for the new ones.
    ///
    /// Fails with [`ImapError::KeywordsNotAllowed`] if the selected mailbox does not accept
    /// custom keywords, instead of letting the server silently drop the colour bits.
    pub async fn store_flags(&mut self, uid: u32, change: &StoreChange) -> Result<(), ImapError> {
        let selected = self.selected.as_ref().ok_or(ImapError::NoMailboxSelected)?;
        if !selected.keywords_allowed {
            return Err(ImapError::KeywordsNotAllowed);
        }

        for query in store_queries(change) {
            // The returned stream must be drained for the command to finish.
            let _: Vec<_> = self
                .inner
                .uid_store(uid.to_string(), query)
                .await?
                .try_collect()
                .await?;
        }
        Ok(())
    }

    /// Wait for the server to announce changes in the selected mailbox (IMAP `IDLE`, RFC 2177),
    /// for at most `timeout`, or until `cancel` fires.
    ///
    /// Takes `self` and gives the session back with the result. That mirrors `async-imap`:
    /// `Session::idle` consumes the session and returns a `Handle`, and only `Handle::done`
    /// returns it. While IDLE runs, the connection cannot be used for anything else, so the
    /// type system stops us from issuing a command in the middle. If anything fails, the
    /// session is gone with the error and the caller reconnects.
    pub async fn idle(
        self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(ImapSession, IdleEnd), ImapError> {
        // Destructure to move both fields out of `self` (a type with no `Drop` impl allows it).
        let ImapSession { inner, selected } = self;
        let mut handle = inner.idle();
        handle.init().await?;

        let response = {
            // `stop` is a guard: dropping it interrupts the wait. It must stay alive while we
            // wait, so it lives until the end of this block.
            let (wait, stop) = handle.wait_with_timeout(timeout);
            tokio::pin!(wait);
            tokio::select! {
                response = &mut wait => response?,
                _ = cancel.cancelled() => {
                    drop(stop);
                    wait.await?
                }
            }
        };

        // Always send DONE, so the server leaves IDLE cleanly and we get the session back.
        let inner = handle.done().await?;
        let end = match response {
            IdleResponse::NewData(_) => IdleEnd::NewData,
            IdleResponse::Timeout => IdleEnd::Timeout,
            IdleResponse::ManualInterrupt => IdleEnd::Cancelled,
        };
        Ok((ImapSession { inner, selected }, end))
    }

    /// End the session politely.
    pub async fn logout(mut self) -> Result<(), ImapError> {
        self.inner.logout().await?;
        Ok(())
    }

    /// Hand back the underlying `async_imap::Session`, for things this wrapper does not cover
    /// yet. The IDLE worker (task 3.5) needs it, because `Session::idle` takes the session by
    /// value; tests use it to `APPEND` seed messages.
    pub fn into_inner(self) -> Session<MaybeTls> {
        self.inner
    }
}

/// Send `STARTTLS` and, if the server agrees, wrap the socket in TLS.
async fn upgrade_with_starttls(
    mut client: Client<MaybeTls>,
    host: &str,
) -> Result<Client<MaybeTls>, ImapError> {
    client
        .run_command_and_check_ok("STARTTLS", None)
        .await
        .map_err(|error| match error {
            // A tagged NO or BAD means the server refuses; anything else is a real failure.
            async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_) => {
                ImapError::StartTlsUnsupported
            }
            other => ImapError::Imap(other),
        })?;

    match client.into_inner() {
        MaybeTls::Plain(tcp) => {
            let tls = tls_handshake(tcp, host).await?;
            // No new greeting after STARTTLS, so we can use the client right away.
            Ok(Client::new(MaybeTls::Tls(Box::new(tls))))
        }
        MaybeTls::Tls(_) => Err(ImapError::Tls(
            "STARTTLS requested on a connection that is already encrypted".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn envelope_addresses_are_joined_and_lowercased() {
        let joined = envelope_address(Some(b"Ann.Lee"), Some(b"Example.ORG"));
        assert_eq!(joined.as_deref(), Some("ann.lee@example.org"));
    }

    #[test]
    fn envelope_addresses_skip_group_markers_and_junk() {
        // Group start: host is NIL. Group end: both are NIL.
        assert_eq!(envelope_address(Some(b"Team"), None), None);
        assert_eq!(envelope_address(None, None), None);
        assert_eq!(envelope_address(None, Some(b"example.org")), None);
        // Latin-1 bytes, empty parts, spaces and a second `@` are rejected, not a panic.
        assert_eq!(
            envelope_address(Some(b"caf\xe9"), Some(b"example.org")),
            None
        );
        assert_eq!(envelope_address(Some(b""), Some(b"example.org")), None);
        assert_eq!(envelope_address(Some(b"a b"), Some(b"example.org")), None);
        assert_eq!(envelope_address(Some(b"a"), Some(b"x@y")), None);
    }

    #[test]
    fn capabilities_are_parsed_case_insensitively() {
        let caps = ServerCapabilities::from_atoms(["IMAP4rev1", "idle", "UIDPLUS", "move"]);
        assert!(caps.idle);
        assert!(caps.uidplus);
        assert!(caps.move_ext);
    }

    #[test]
    fn missing_capabilities_are_false() {
        let caps = ServerCapabilities::from_atoms(["IMAP4rev1", "AUTH=PLAIN"]);
        assert!(!caps.idle);
        assert!(!caps.uidplus);
        assert!(!caps.move_ext);
        assert_eq!(caps.all, ["AUTH=PLAIN", "IMAP4REV1"]);
    }

    #[test]
    fn keywords_need_the_backslash_star() {
        let with_star: Vec<String> = ["\\Seen", "\\Flagged", "\\*"].map(String::from).to_vec();
        let without: Vec<String> = ["\\Seen", "\\Flagged", "$Junk"].map(String::from).to_vec();
        assert!(keywords_allowed(&with_star));
        assert!(!keywords_allowed(&without));
        assert!(!keywords_allowed(&[]));
    }

    #[test]
    fn mailbox_info_reads_select_data() {
        let mailbox = Mailbox {
            exists: 7,
            uid_validity: Some(1234),
            uid_next: Some(88),
            permanent_flags: vec![Flag::Seen, Flag::Flagged, Flag::MayCreate],
            ..Mailbox::default()
        };
        let info = mailbox_info(&mailbox).unwrap();
        assert_eq!(info.uid_validity, 1234);
        assert_eq!(info.uid_next, 88);
        assert_eq!(info.exists, 7);
        assert!(info.keywords_allowed);
        assert_eq!(info.permanent_flags, ["\\Seen", "\\Flagged", "\\*"]);
    }

    #[test]
    fn mailbox_without_permanent_star_forbids_keywords() {
        let mailbox = Mailbox {
            uid_validity: Some(1),
            uid_next: Some(2),
            permanent_flags: vec![Flag::Seen, Flag::Flagged],
            ..Mailbox::default()
        };
        assert!(!mailbox_info(&mailbox).unwrap().keywords_allowed);
    }

    #[test]
    fn mailbox_without_uidvalidity_is_an_error() {
        let result = mailbox_info(&Mailbox::default());
        assert!(matches!(result, Err(ImapError::MissingData("UIDVALIDITY"))));
    }

    #[test]
    fn custom_flags_keep_their_name() {
        assert_eq!(
            flag_name(&Flag::Custom("$MailFlagBit0".into())),
            "$MailFlagBit0"
        );
    }

    #[test]
    fn uid_sets_are_comma_separated() {
        assert_eq!(uid_set(&[4, 9, 12]).unwrap(), "4,9,12");
        assert_eq!(uid_set(&[7]).unwrap(), "7");
        assert!(matches!(uid_set(&[]), Err(ImapError::EmptyUidSet)));
    }

    #[test]
    fn store_queries_remove_first_then_add() {
        use crate::flags::FlagColour;
        // Orange: bit 0 only.
        let change = FlagColour::Orange.store_change().unwrap();
        assert_eq!(
            store_queries(&change),
            [
                "-FLAGS.SILENT ($MailFlagBit1 $MailFlagBit2)",
                "+FLAGS.SILENT (\\Flagged $MailFlagBit0)",
            ]
        );
    }

    #[test]
    fn store_queries_skip_empty_lists() {
        let only_add = StoreChange {
            add: vec!["\\Flagged"],
            remove: vec![],
        };
        assert_eq!(store_queries(&only_add), ["+FLAGS.SILENT (\\Flagged)"]);
    }

    #[test]
    fn attributes_use_wire_spelling() {
        assert_eq!(
            attribute_text(&NameAttribute::Sent).as_deref(),
            Some("\\Sent")
        );
        assert_eq!(
            attribute_text(&NameAttribute::NoSelect).as_deref(),
            Some("\\Noselect")
        );
        assert_eq!(
            attribute_text(&NameAttribute::Extension("\\HasChildren".into())).as_deref(),
            Some("\\HasChildren")
        );
    }

    /// Hard rule: fetch with `BODY.PEEK[...]` only, because a plain body fetch marks the mail
    /// as read. Also covers the bare RFC 822 data item, which behaves like the plain body fetch
    /// (the SIZE and HEADER variants are fine).
    ///
    /// The forbidden strings are assembled at runtime so that this test's own source does not
    /// contain them.
    #[test]
    fn no_plain_body_fetches() {
        let plain_body = ["BODY", "["].concat();
        let rfc822 = ["RFC", "822"].concat();

        let mut violations = Vec::new();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        scan_dir(&src, &plain_body, &rfc822, &mut violations);

        assert!(
            violations.is_empty(),
            "plain body fetches mark mail as read; use BODY.PEEK instead:\n{}",
            violations.join("\n")
        );
    }

    fn scan_dir(dir: &Path, plain_body: &str, rfc822: &str, violations: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan_dir(&path, plain_body, rfc822, violations);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                for (number, line) in text.lines().enumerate() {
                    // The bare data item is only dangerous when no `.SIZE` or `.HEADER` suffix follows.
                    let bare_rfc822 = line
                        .match_indices(rfc822)
                        .any(|(at, _)| !line[at + rfc822.len()..].starts_with('.'));
                    if line.contains(plain_body) || bare_rfc822 {
                        violations.push(format!("{}:{}: {}", path.display(), number + 1, line));
                    }
                }
            }
        }
    }

    /// Hard rule: v1 never deletes, moves or expunges mail.
    #[test]
    fn no_destructive_commands() {
        let production_code = include_str!("session.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ".expunge(",
            ".uid_expunge(",
            ".uid_mv(",
            ".mv(",
            ".copy(",
            ".uid_copy(",
            ".delete(",
        ] {
            assert!(
                !production_code.contains(forbidden),
                "session code must not call {forbidden}"
            );
        }
    }
}
