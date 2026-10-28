//! EMAIL · 15.3 — the sending half of `dom_render_compiler::email`.
//!
//! The compiler crate owns the vocabulary: the `email` block, message
//! validation, the four providers' wire formats and how to read their answers.
//! This module owns what needs a process — a credential, a network client, a
//! directory — behind one seam, [`Mailer`].
//!
//! ## Why a trait, when there are two implementations
//!
//! Because the third is known: SMTP via `lettre`, for the deployment that
//! already runs a relay. The trait is the whole contract a transport has to
//! meet — take a validated message, answer accepted / retry / reject — and the
//! queue, retries, dead-lettering and redaction above it do not change when
//! one is added.
//!
//! ## Where links point, and why never at the request's `Host`
//!
//! A reset link built from the request's `Host` header is the classic
//! password-reset poisoning attack: an attacker requests a reset for the
//! victim's address with `Host: evil.example`, and the victim receives a
//! genuine email whose genuine token is attached to the attacker's domain.
//! [`resolve_link_origin`] therefore reads configuration only —
//! `ALBEDO_PUBLIC_ORIGIN`, the ACME domain, or loopback in development — and
//! boot refuses an app whose auth flows mail links when none of those answer.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use dom_render_compiler::aperture::{ApertureClient, ApertureError, ApertureRequest, CacheScope};
use dom_render_compiler::email::{
    build_request, classify, eml, AttachmentSource, EmailConfig, Mailbox, Message, ProviderKind,
    QueuedEmail, ResolvedAttachment, SendOutcome,
};
use dom_render_compiler::forge::DataSubstrate;

/// A way to hand a validated message to the world.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Attempt one delivery. `attachments` are the message's files, already
    /// read and checked; `idempotency_key` is stable across retries of the
    /// same queue row.
    async fn send(
        &self,
        message: &Message,
        attachments: &[ResolvedAttachment],
        idempotency_key: &str,
    ) -> SendOutcome;
}

/// One of the four HTTP providers, over APERTURE.
///
/// Every byte leaves through the app's one outbound client, so a mail provider
/// is subject to the same egress policy and per-host upstream budget as a
/// declared source — and appears in the same metrics.
pub struct HttpMailer {
    config: EmailConfig,
    key: String,
    /// 🪤 The shared **cell**, read at send time — the client is installed after
    /// the runtime is built, the trap `JobsPlan` documents.
    client: Arc<OnceLock<Arc<ApertureClient>>>,
}

impl HttpMailer {
    /// Build one.
    #[must_use]
    pub fn new(
        config: EmailConfig,
        key: String,
        client: Arc<OnceLock<Arc<ApertureClient>>>,
    ) -> Self {
        Self {
            config,
            key,
            client,
        }
    }
}

#[async_trait]
impl Mailer for HttpMailer {
    async fn send(
        &self,
        message: &Message,
        attachments: &[ResolvedAttachment],
        idempotency_key: &str,
    ) -> SendOutcome {
        let Some(request) =
            build_request(&self.config, &self.key, message, attachments, idempotency_key)
        else {
            return SendOutcome::Reject(format!(
                "provider `{}` cannot describe a request",
                self.config.provider.as_str()
            ));
        };
        let Some(client) = self.client.get() else {
            // Transient by construction: the cell is filled during boot.
            return SendOutcome::Retry("no outbound client is installed yet".to_string());
        };
        let outbound = ApertureRequest {
            method: request.method.to_string(),
            url: request.url,
            // A send is an effect: never cached, never coalesced.
            scope: CacheScope::App,
            ttl: Duration::ZERO,
            headers: request.headers,
            body: request.body,
        };
        match client.send_effect(&outbound).await {
            Ok(response) => classify(
                self.config.provider,
                response.status,
                &response.headers,
                &response.body,
            ),
            // Configuration, not weather: the policy will refuse the next
            // attempt identically.
            Err(err @ (ApertureError::Egress(_) | ApertureError::InvalidUrl(_))) => {
                SendOutcome::Reject(err.to_string())
            }
            Err(err) => SendOutcome::Retry(err.to_string()),
        }
    }
}

/// The `log` provider: each message becomes an `.eml` file.
pub struct OutboxMailer {
    dir: PathBuf,
    from: Mailbox,
    reply_to: Option<Mailbox>,
    /// Print the text body to stderr. On for `albedo dev`, where the reset link
    /// is the thing the developer is looking for; off for `serve`, where a log
    /// line carrying a live credential is the thing to avoid.
    echo: bool,
}

impl OutboxMailer {
    /// Write into `dir`, created on first send.
    #[must_use]
    pub fn new(dir: PathBuf, config: &EmailConfig, echo: bool) -> Self {
        Self {
            dir,
            from: config.from.clone(),
            reply_to: config.reply_to.clone(),
            echo,
        }
    }
}

#[async_trait]
impl Mailer for OutboxMailer {
    async fn send(
        &self,
        message: &Message,
        attachments: &[ResolvedAttachment],
        idempotency_key: &str,
    ) -> SendOutcome {
        let now = crate::auth::now_ms();
        // The row id is the idempotency key, so a retried row overwrites its own
        // file rather than writing a second one — the outbox mirrors what a
        // provider honouring the key would have delivered.
        let stem: String = idempotency_key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .take(96)
            .collect();
        let rendered = eml::render(
            message,
            attachments,
            &self.from,
            self.reply_to.as_ref(),
            &stem,
            &eml::rfc2822(now),
        );
        let path = self.dir.join(format!("{stem}.eml"));
        let write = async {
            tokio::fs::create_dir_all(&self.dir).await?;
            tokio::fs::write(&path, rendered.as_bytes()).await
        };
        if let Err(err) = write.await {
            return SendOutcome::Retry(format!("could not write {}: {err}", path.display()));
        }

        // stderr, not `tracing`: without a subscriber a `tracing` line reaches
        // nobody, and the whole point of this provider is to be read.
        if self.echo {
            let to: Vec<String> = message.to.iter().map(Mailbox::formatted).collect();
            eprintln!(
                "\n  ✉  email (log provider) → {}\n     subject: {}\n     saved:   {}\n{}\n",
                to.join(", "),
                message.subject,
                path.display(),
                message
                    .text
                    .as_deref()
                    .unwrap_or("(html only — open the .eml file)")
                    .lines()
                    .map(|line| format!("     │ {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else {
            eprintln!("  ✉  email written to {} (log provider)", path.display());
        }
        SendOutcome::Accepted(Some(stem))
    }
}

/// What the rest of the server holds: the lowered block, a transport, where
/// links point, and the link-mailing work still in flight.
pub struct EmailRuntime {
    config: EmailConfig,
    mailer: Arc<dyn Mailer>,
    /// Absolute origin for links in mail. `None` when it could not be resolved
    /// from configuration — fine for an app that only calls `sendEmail()`,
    /// fatal at boot for one whose auth flows mail links.
    link_origin: Option<String>,
    /// Project root; uploaded attachments are read from under it.
    project_dir: PathBuf,
    inflight: Arc<Inflight>,
}

/// A count of spawned tasks and a way to wait for it to reach zero.
///
/// The "mail me a link" endpoints answer before their work runs (so the
/// response cannot be timed), which makes that work a detached task — and a
/// detached task is killed with the process. Shutdown therefore waits here, up
/// to the drain deadline, for every token write and queue write already begun.
#[derive(Default)]
struct Inflight {
    count: std::sync::atomic::AtomicUsize,
    idle: tokio::sync::Notify,
}

/// Decrements on drop, so a task that panics still counts itself out.
struct InflightGuard(Arc<Inflight>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl EmailRuntime {
    /// Assemble.
    #[must_use]
    pub fn new(
        config: EmailConfig,
        mailer: Arc<dyn Mailer>,
        link_origin: Option<String>,
        project_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            mailer,
            link_origin,
            project_dir,
            inflight: Arc::new(Inflight::default()),
        }
    }

    /// Run `work` detached from the response, but not from shutdown: see
    /// [`Self::drain`].
    pub fn spawn<F>(&self, work: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        use std::sync::atomic::Ordering;
        self.inflight.count.fetch_add(1, Ordering::AcqRel);
        let guard = InflightGuard(Arc::clone(&self.inflight));
        tokio::spawn(async move {
            let _guard = guard;
            work.await;
        });
    }

    /// Wait for every [`Self::spawn`]ed task to finish, or `deadline` to pass.
    /// Returns how many were still running when it gave up.
    pub async fn drain(&self, deadline: Duration) -> usize {
        use std::sync::atomic::Ordering;
        let wait = async {
            loop {
                // Registered before the check, so a task finishing between the
                // two cannot be missed.
                let notified = self.inflight.idle.notified();
                if self.inflight.count.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(deadline, wait).await;
        self.inflight.count.load(Ordering::Acquire)
    }

    /// The lowered block.
    #[must_use]
    pub fn config(&self) -> &EmailConfig {
        &self.config
    }

    /// The name recipients know this app by: the sender's display name, or the
    /// link origin's host, or the sender's domain.
    #[must_use]
    pub fn app_name(&self) -> String {
        if let Some(name) = &self.config.from.name {
            return name.clone();
        }
        self.link_origin
            .as_deref()
            .and_then(|origin| url::Url::parse(origin).ok())
            .and_then(|url| url.host_str().map(str::to_string))
            .or_else(|| {
                self.config
                    .from
                    .address
                    .split_once('@')
                    .map(|(_, domain)| domain.to_string())
            })
            .unwrap_or_else(|| "this app".to_string())
    }

    /// An absolute URL for `path_and_query` (which starts with `/`), or `None`
    /// when no origin is configured.
    #[must_use]
    pub fn link(&self, path_and_query: &str) -> Option<String> {
        self.link_origin
            .as_deref()
            .map(|origin| format!("{}{path_and_query}", origin.trim_end_matches('/')))
    }

    /// Deliver one queued email for `principal`. Re-validates the row first: it
    /// is a database value, and a database can be edited. Attachments are read
    /// and ownership-checked here, at send time, for the same reason.
    pub async fn deliver(
        &self,
        queued: &QueuedEmail,
        idempotency_key: &str,
        db: &dyn DataSubstrate,
        principal: Option<&str>,
    ) -> SendOutcome {
        if let Err(err) = queued.message.validate() {
            return SendOutcome::Reject(format!("the queued message is invalid: {err}"));
        }
        let files = match resolve_attachments(db, &self.project_dir, principal, &queued.message).await
        {
            Ok(files) => files,
            Err(outcome) => return outcome,
        };
        self.mailer.send(&queued.message, &files, idempotency_key).await
    }
}

/// Check the upload attachments of a message about to be queued for
/// `principal`, without reading any bytes: each exists, each is theirs, and the
/// total fits. The same checks run again at send time.
///
/// # Errors
/// A message for the author, naming the attachment.
pub(crate) async fn check_attachments(
    db: &dyn DataSubstrate,
    principal: Option<&str>,
    message: &Message,
) -> Result<(), String> {
    let mut total: u64 = 0;
    for attachment in &message.attachments {
        match &attachment.source {
            AttachmentSource::Inline(content) => total += (content.len() / 4 * 3) as u64,
            AttachmentSource::Upload(id) => {
                total += owned_upload(db, principal, id).await?.byte_size;
            }
        }
    }
    if total > dom_render_compiler::email::message::MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "sendEmail(): attachments total {total} bytes; the limit is {}",
            dom_render_compiler::email::message::MAX_ATTACHMENT_BYTES
        ));
    }
    Ok(())
}

/// The upload behind `id`, if it exists and belongs to `principal`.
///
/// 🔒 **Exact ownership, both ways.** A content-addressed id is a name, not a
/// permission: anyone who has seen one can type it. Without this, an action that
/// attaches `form.fileId` would mail any user's upload to whoever asked. An
/// anonymous upload is attachable only by an anonymous send, and a signed-in
/// user's only by that user.
async fn owned_upload(
    db: &dyn DataSubstrate,
    principal: Option<&str>,
    id: &str,
) -> Result<dom_render_compiler::upload::store::StoredUpload, String> {
    let stored = dom_render_compiler::upload::store::lookup(db, id)
        .await
        .map_err(|err| format!("attachment `{id}`: {err}"))?
        .ok_or_else(|| format!("attachment `{id}` is not a stored upload"))?;
    if stored.principal.as_deref() != principal {
        return Err(format!(
            "attachment `{id}` belongs to a different user than the one this email is sent for"
        ));
    }
    Ok(stored)
}

/// Read every attachment's bytes. A missing or foreign upload is permanent; a
/// read error is retried.
async fn resolve_attachments(
    db: &dyn DataSubstrate,
    project_dir: &std::path::Path,
    principal: Option<&str>,
    message: &Message,
) -> Result<Vec<ResolvedAttachment>, SendOutcome> {
    use base64::Engine as _;
    let mut out = Vec::with_capacity(message.attachments.len());
    let mut total: u64 = 0;
    for attachment in &message.attachments {
        let resolved = match &attachment.source {
            AttachmentSource::Inline(content) => ResolvedAttachment {
                filename: attachment.filename.clone(),
                content_type: attachment
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                bytes: base64::engine::general_purpose::STANDARD
                    .decode(content.as_bytes())
                    .map_err(|_| SendOutcome::Reject("an inline attachment is not base64".into()))?,
            },
            AttachmentSource::Upload(id) => {
                let stored = owned_upload(db, principal, id)
                    .await
                    .map_err(SendOutcome::Reject)?;
                let path = dom_render_compiler::upload::store::path_for(project_dir, id)
                    .map_err(|err| SendOutcome::Reject(err.to_string()))?;
                let bytes = match tokio::fs::read(&path).await {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(SendOutcome::Reject(format!(
                            "attachment `{id}` is recorded but its file is gone"
                        )))
                    }
                    Err(err) => {
                        return Err(SendOutcome::Retry(format!("attachment `{id}`: {err}")))
                    }
                };
                ResolvedAttachment {
                    filename: if attachment.filename.is_empty() {
                        stored.original_name
                    } else {
                        attachment.filename.clone()
                    },
                    content_type: attachment.content_type.clone().unwrap_or(stored.content_type),
                    bytes,
                }
            }
        };
        total += resolved.bytes.len() as u64;
        out.push(resolved);
    }
    if total > dom_render_compiler::email::message::MAX_ATTACHMENT_BYTES {
        return Err(SendOutcome::Reject(format!(
            "attachments total {total} bytes; the limit is {}",
            dom_render_compiler::email::message::MAX_ATTACHMENT_BYTES
        )));
    }
    Ok(out)
}

/// `albedo email test` — send one message through the configured provider, now,
/// and report exactly what the provider said.
///
/// Not queued: an operator checking a configuration wants the provider's answer
/// in the terminal, not a row to go and read. Uses the same client, egress
/// policy and provider code as a running server, so a pass here is a pass there.
///
/// # Errors
/// The block is invalid, the key is missing, or the address is not one.
pub async fn send_test(
    config: EmailConfig,
    to: &str,
    outbox: PathBuf,
) -> Result<SendOutcome, String> {
    use dom_render_compiler::aperture::{EgressMode, EgressPolicy, ReqwestTransport, ResponseCache};

    let to = Mailbox::parse(to).map_err(|err| err.to_string())?;
    let policy = Arc::new(EgressPolicy::with_declared_hosts(
        EgressMode::Serve,
        config.egress_host(),
    ));
    let transport = ReqwestTransport::new(Arc::clone(&policy)).map_err(|err| err.to_string())?;
    let client = Arc::new(ApertureClient::new(
        Arc::new(transport),
        Arc::new(ResponseCache::default()),
        policy,
    ));
    let cell = Arc::new(OnceLock::new());
    let _ = cell.set(client);
    let mailer = mailer_for(&config, cell, outbox, true)?;
    let message = Message {
        from: None,
        to: vec![to],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: None,
        subject: "ALBEDO email test".to_string(),
        text: Some(format!(
            "This message was sent by `albedo email test` through the `{}` provider.\n\n\
             If you are reading it, this app's email configuration delivers.\n",
            config.provider.as_str()
        )),
        html: None,
        attachments: Vec::new(),
    };
    message.validate().map_err(|err| err.to_string())?;
    let key = format!("albedo-email-test-{:016x}", rand::random::<u64>());
    Ok(mailer.send(&message, &[], &key).await)
}

/// Build the transport the block names.
///
/// # Errors
/// The `apiKey` names an unset environment variable.
pub fn mailer_for(
    config: &EmailConfig,
    client: Arc<OnceLock<Arc<ApertureClient>>>,
    outbox: PathBuf,
    dev_mode: bool,
) -> Result<Arc<dyn Mailer>, String> {
    if config.provider == ProviderKind::Log {
        return Ok(Arc::new(OutboxMailer::new(outbox, config, dev_mode)));
    }
    let key = config
        .resolve_key()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("`email.provider` \"{}\" needs an apiKey", config.provider.as_str()))?;
    Ok(Arc::new(HttpMailer::new(config.clone(), key, client)))
}

/// Where links in mail point. Configuration only — never a request header.
///
/// In order: `ALBEDO_PUBLIC_ORIGIN`; the first ACME domain; `http(s)://localhost:{port}`
/// when the server binds loopback or runs as `albedo dev`. `None` otherwise.
///
/// # Errors
/// `ALBEDO_PUBLIC_ORIGIN` is set but is not a bare `http(s)` origin — a path or
/// query in it would be pasted in front of every link.
pub fn resolve_link_origin(
    public_origin: Option<&str>,
    acme_domain: Option<&str>,
    bind_host: &str,
    port: u16,
    tls: bool,
    dev_mode: bool,
) -> Result<Option<String>, String> {
    if let Some(raw) = public_origin.map(str::trim).filter(|raw| !raw.is_empty()) {
        let url = url::Url::parse(raw)
            .map_err(|err| format!("{} = `{raw}` is not a URL: {err}", crate::tls::PUBLIC_ORIGIN_ENV))?;
        let bare = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty();
        if !bare {
            return Err(format!(
                "{} = `{raw}` must be a bare origin like `https://app.example.com` — no path, \
                 query or credentials, because every emailed link is built on it",
                crate::tls::PUBLIC_ORIGIN_ENV
            ));
        }
        return Ok(Some(raw.trim_end_matches('/').to_string()));
    }
    if let Some(domain) = acme_domain {
        return Ok(Some(format!("https://{domain}")));
    }
    let loopback = matches!(bind_host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    if loopback || dev_mode {
        let scheme = if tls { "https" } else { "http" };
        return Ok(Some(format!("{scheme}://localhost:{port}")));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::email::EmailDecl;
    use dom_render_compiler::forge::LibSqlSubstrate;
    use dom_render_compiler::upload::store::{self as uploads, StoredUpload};

    const ALICE: &str = "u_alice";

    async fn db_with_upload(owner: Option<&str>, project: &std::path::Path) -> (LibSqlSubstrate, String) {
        let db = LibSqlSubstrate::open_ephemeral().await.expect("opens");
        for statement in uploads::collection().migrations.iter() {
            db.migrate(statement).await.expect("upload table");
        }
        let id = "ab".repeat(32);
        uploads::record(
            &db,
            &StoredUpload {
                id: id.clone(),
                bucket: "files".into(),
                content_type: "application/pdf".into(),
                byte_size: 4,
                original_name: "invoice.pdf".into(),
                principal: owner.map(str::to_string),
            },
            0,
        )
        .await
        .expect("records");
        let path = uploads::path_for(project, &id).expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"%PDF").unwrap();
        (db, id)
    }

    fn with_upload(id: &str) -> Message {
        Message::from_authored(&serde_json::json!({
            "to": "a@x.dev", "subject": "s", "text": "t", "attachments": [{ "upload": id }]
        }))
        .expect("valid")
    }

    /// 🔒 A content-addressed id is a name, not a permission. Alice's upload is
    /// attachable by alice's sends only — not by bob's, and not by an anonymous one.
    #[tokio::test]
    async fn an_upload_is_attachable_only_by_the_principal_that_owns_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (db, id) = db_with_upload(Some(ALICE), dir.path()).await;
        let message = with_upload(&id);

        assert!(check_attachments(&db, Some(ALICE), &message).await.is_ok());
        for stranger in [Some("u_bob"), None] {
            let err = check_attachments(&db, stranger, &message).await.unwrap_err();
            assert!(err.contains("different user"), "{err}");
        }

        // And at send time, where the row could have been edited in between.
        let resolved = resolve_attachments(&db, dir.path(), Some(ALICE), &message)
            .await
            .expect("alice's own file");
        assert_eq!(resolved[0].filename, "invoice.pdf", "falls back to the uploaded name");
        assert_eq!(resolved[0].content_type, "application/pdf");
        assert_eq!(resolved[0].bytes, b"%PDF");
        assert!(matches!(
            resolve_attachments(&db, dir.path(), Some("u_bob"), &message).await,
            Err(SendOutcome::Reject(_))
        ));
    }

    #[tokio::test]
    async fn a_missing_upload_is_refused_not_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (db, _) = db_with_upload(None, dir.path()).await;
        let message = with_upload(&"cd".repeat(32));
        assert!(check_attachments(&db, None, &message).await.is_err());
        assert!(matches!(
            resolve_attachments(&db, dir.path(), None, &message).await,
            Err(SendOutcome::Reject(_))
        ));
    }

    /// Item 5: shutdown waits for link work already begun, up to its deadline.
    #[tokio::test]
    async fn drain_waits_for_spawned_work_and_reports_what_it_abandoned() {
        let runtime = EmailRuntime::new(
            log_config(),
            Arc::new(OutboxMailer::new(PathBuf::from("."), &log_config(), false)),
            None,
            PathBuf::from("."),
        );
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&done);
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(runtime.drain(Duration::from_secs(5)).await, 0);
        assert!(done.load(std::sync::atomic::Ordering::SeqCst), "drain returned before the work finished");

        runtime.spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        assert_eq!(
            runtime.drain(Duration::from_millis(50)).await,
            1,
            "a deadline bounds the wait and says what it left behind"
        );
    }

    /// A task that panics still counts itself out, or shutdown would always
    /// wait out the full deadline after one bad send.
    #[tokio::test]
    async fn a_panicking_task_does_not_hold_up_shutdown() {
        let runtime = EmailRuntime::new(
            log_config(),
            Arc::new(OutboxMailer::new(PathBuf::from("."), &log_config(), false)),
            None,
            PathBuf::from("."),
        );
        runtime.spawn(async { panic!("a bad send") });
        assert_eq!(runtime.drain(Duration::from_secs(5)).await, 0);
    }

    fn log_config() -> EmailConfig {
        serde_json::from_value::<EmailDecl>(serde_json::json!({
            "from": "Acme <no-reply@acme.dev>", "provider": "log"
        }))
        .expect("parses")
        .lower()
        .expect("lowers")
    }

    fn message() -> Message {
        Message::from_authored(&serde_json::json!({
            "to": "ada@x.dev", "subject": "Hi", "text": "Link: https://acme.dev/x?t=1"
        }))
        .expect("valid")
    }

    #[tokio::test]
    async fn the_outbox_writes_one_file_per_row_and_a_retry_overwrites_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mailer = OutboxMailer::new(dir.path().join("outbox"), &log_config(), false);
        for _ in 0..2 {
            assert!(matches!(
                mailer.send(&message(), &[], "albedo:send_email:00ff").await,
                SendOutcome::Accepted(_)
            ));
        }
        let files: Vec<_> = std::fs::read_dir(dir.path().join("outbox"))
            .expect("created")
            .collect();
        assert_eq!(files.len(), 1, "same row, same file");
        let eml = std::fs::read_to_string(files[0].as_ref().expect("entry").path()).expect("reads");
        assert!(eml.contains("To: ada@x.dev\r\n"), "{eml}");
        assert!(eml.contains("From: \"Acme\" <no-reply@acme.dev>"), "{eml}");
    }

    /// The row id reaches a filename; it must not reach outside the directory.
    #[tokio::test]
    async fn a_hostile_row_id_cannot_escape_the_outbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outbox = dir.path().join("outbox");
        let mailer = OutboxMailer::new(outbox.clone(), &log_config(), false);
        mailer.send(&message(), &[], "../../etc/passwd").await;
        let names: Vec<String> = std::fs::read_dir(&outbox)
            .expect("created")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["______etc_passwd.eml"]);
    }

    #[test]
    fn links_come_from_configuration_in_a_fixed_order() {
        let resolve = |origin, acme, host, dev| resolve_link_origin(origin, acme, host, 3000, false, dev);
        assert_eq!(
            resolve(Some("https://app.acme.dev/"), Some("x.dev"), "0.0.0.0", false),
            Ok(Some("https://app.acme.dev".into()))
        );
        assert_eq!(
            resolve(None, Some("app.acme.dev"), "0.0.0.0", false),
            Ok(Some("https://app.acme.dev".into()))
        );
        assert_eq!(resolve(None, None, "127.0.0.1", false), Ok(Some("http://localhost:3000".into())));
        assert_eq!(resolve(None, None, "0.0.0.0", true), Ok(Some("http://localhost:3000".into())));
        assert_eq!(resolve(None, None, "0.0.0.0", false), Ok(None), "production with nothing declared");
    }

    #[test]
    fn a_public_origin_with_a_path_is_refused() {
        for bad in ["https://app.dev/base", "https://app.dev/?x=1", "ftp://app.dev", "https://u:p@app.dev"] {
            assert!(
                resolve_link_origin(Some(bad), None, "0.0.0.0", 3000, false, false).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_app_name_prefers_the_senders_display_name() {
        let runtime = EmailRuntime::new(
            log_config(),
            Arc::new(OutboxMailer::new(PathBuf::from("."), &log_config(), false)),
            Some("https://app.acme.dev".into()),
            PathBuf::from("."),
        );
        assert_eq!(runtime.app_name(), "Acme");
        assert_eq!(
            runtime.link("/_albedo/auth/password/reset?token=t").as_deref(),
            Some("https://app.acme.dev/_albedo/auth/password/reset?token=t")
        );
    }
}
