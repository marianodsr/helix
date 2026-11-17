//! lsp/servers, lsp/query and `event/lsp`.
//!
//! `lsp/query` asks the language server Helix already runs, through Helix's
//! own client methods. On the main thread the bridge only picks the server
//! (as Helix's own commands do: the first one attached to the document that
//! supports the feature), converts the position and issues the request.
//! Waiting for the answer, reading preview lines and building JSON happen in
//! the bridge's tasks. A query that is cancelled or times out sends
//! `$/cancelRequest` for the request it is waiting on.
//!
//! Positions: the protocol counts 1-based lines and columns in chars; the
//! server counts 0-based in its negotiated encoding. Both directions go
//! through `helix_lsp::util` with the server's offset encoding, against the
//! buffer as it was when the request was issued, or the file on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::{BoxFuture, FutureExt};
use helix_core::line_ending::line_end_char_index;
use helix_core::syntax::config::{LanguageConfiguration, LanguageServerFeature};
use helix_core::{Rope, Uri};
use helix_event::{register_hook, runtime_local};
use helix_lsp::util::{lsp_pos_to_pos, pos_to_lsp_pos};
use helix_lsp::{lsp, Client, LanguageServerId, OffsetEncoding};
use helix_view::editor::Action;
use helix_view::events::{LanguageServerExited, LanguageServerInitialized};
use helix_view::{DocumentId, Editor};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use slotmap::Key;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::bridge::convert::{char_from_pos, pos_from_char, resolve_path};
use crate::bridge::dispatch::on_editor;
use crate::bridge::events::{broadcast, wants_kind, Kind};
use crate::bridge::protocol::{parse_params, ErrorCode, Position, Range, RpcError, RpcResult};
use crate::bridge::server::Ctx;
use crate::events::PostCommand;

pub const CAPABILITY: Option<&str> = Some("lsp");

/// Most list items one response carries.
const MAX_ITEMS: usize = 500;
const DEFAULT_LIMIT: usize = 100;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
/// Longest `preview`, in chars.
const PREVIEW_CHARS: usize = 200;
/// Call sites listed per call.
const MAX_CALL_SITES: usize = 50;
/// Files larger than this are not read for previews and positions.
const MAX_DISK_READ: u64 = 64 * 1024 * 1024;
/// How often to look again while a server for the file is starting.
const STARTING_POLL: Duration = Duration::from_millis(50);

// --- event/lsp ---------------------------------------------------------------

runtime_local! {
    /// Servers reported as initialized. `:lsp-restart` and `:lsp-stop` drop a
    /// server from the registry before it exits, so Helix never dispatches
    /// `LanguageServerExited` for it; those are found missing here instead.
    static KNOWN: Mutex<Vec<(LanguageServerId, String)>> = Mutex::new(Vec::new());
    /// `event/lsp` goes through one task, so events keep their order.
    static EVENTS: OnceLock<UnboundedSender<Value>> = OnceLock::new();
}

pub fn register_hooks() {
    if let Some(shared) = crate::bridge::shared() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        if EVENTS.set(tx).is_ok() {
            let sink = shared.clone();
            let task = tokio::spawn(async move {
                while let Some(params) = rx.recv().await {
                    broadcast(&sink, Kind::Lsp, "event/lsp", params, false);
                }
            });
            shared.track(task.abort_handle());
        }
    }
    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        // Tracked even with nobody subscribed (rare, and a later subscriber
        // must still hear about this server's exit).
        if let Some(client) = event.editor.language_servers.get_by_id(event.server_id) {
            let name = client.name().to_owned();
            if wants_kind(Kind::Lsp) {
                sweep(event.editor);
                lsp_event(event.server_id, name.clone(), "initialized");
            }
            KNOWN.lock().push((event.server_id, name));
        }
        Ok(())
    });
    // Dispatched before the server leaves the registry.
    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        let mut known = KNOWN.lock();
        if let Some(i) = known.iter().position(|(id, _)| *id == event.server_id) {
            let (id, name) = known.swap_remove(i);
            if wants_kind(Kind::Lsp) {
                lsp_event(id, name, "exited");
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        if wants_kind(Kind::Lsp) {
            sweep(event.cx.editor);
        }
        Ok(())
    });
}

/// Report servers that left the registry without an exit event.
fn sweep(editor: &Editor) {
    let mut known = KNOWN.lock();
    known.retain(|(id, name)| {
        let alive = editor.language_servers.get_by_id(*id).is_some();
        if !alive {
            lsp_event(*id, name.clone(), "exited");
        }
        alive
    });
}

fn lsp_event(id: LanguageServerId, name: String, state: &'static str) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(json!({ "id": server_num(id), "name": name, "state": state }));
    }
}

/// The number behind a `LanguageServerId` (a slotmap key, like `viewId`).
pub fn server_num(id: LanguageServerId) -> u64 {
    id.data().as_ffi()
}

// --- lsp/servers -------------------------------------------------------------

/// Protocol names for Helix's language-server features. The `lsp/query`
/// kinds come first, so `features` can be checked against a kind directly.
const FEATURES: &[(&[LanguageServerFeature], &[&str])] = {
    use LanguageServerFeature::*;
    &[
        (&[GotoDefinition], &["definition"]),
        (&[GotoTypeDefinition], &["typeDefinition"]),
        (&[GotoImplementation], &["implementation"]),
        (&[GotoReference], &["references"]),
        (&[Hover], &["hover"]),
        (&[DocumentSymbols], &["documentSymbols"]),
        (&[WorkspaceSymbols], &["workspaceSymbols"]),
        (&[CallHierarchy], &["incomingCalls", "outgoingCalls"]),
        (&[RenameSymbol], &["renamePreview"]),
        (&[GotoDeclaration], &["declaration"]),
        (&[Diagnostics], &["diagnostics"]),
        (&[PullDiagnostics], &["pullDiagnostics"]),
        (&[Completion], &["completion"]),
        (&[SignatureHelp], &["signatureHelp"]),
        (&[CodeAction], &["codeAction"]),
        (&[Format], &["format"]),
        (&[DocumentHighlight], &["documentHighlight"]),
        (&[InlayHints], &["inlayHints"]),
        (&[DocumentLinks], &["documentLinks"]),
        (&[DocumentColors], &["documentColors"]),
        (&[WorkspaceCommand], &["workspaceCommand"]),
    ]
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerInfo {
    id: u64,
    name: String,
    languages: Vec<String>,
    root: Option<String>,
    initialized: bool,
    features: Vec<&'static str>,
    /// The negotiated position encoding (`utf-8`, `utf-16`, `utf-32`).
    position_encoding: Option<&'static str>,
}

pub fn servers(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let (clients, loader) = on_editor(|editor, _| {
            let clients: Vec<Arc<Client>> =
                editor.language_servers.iter_clients().cloned().collect();
            (clients, editor.syn_loader.load_full())
        })
        .await?;
        let mut servers = Vec::with_capacity(clients.len());
        for client in clients {
            let initialized = client.is_initialized();
            let features = if initialized {
                FEATURES
                    .iter()
                    .filter(|(f, _)| f.iter().all(|f| client.supports_feature(*f)))
                    .flat_map(|(_, names)| names.iter().copied())
                    .collect()
            } else {
                Vec::new()
            };
            let root = client
                .workspace_folders()
                .await
                .first()
                .and_then(|folder| folder.uri.to_file_path().ok())
                .map(|path| path.to_string_lossy().into_owned());
            let languages = loader
                .language_configs()
                .filter(|config| {
                    config
                        .language_servers
                        .iter()
                        .any(|ls| ls.name == client.name())
                })
                .map(|config| config.language_id.clone())
                .collect();
            servers.push(ServerInfo {
                id: server_num(client.id()),
                name: client.name().to_owned(),
                languages,
                root,
                initialized,
                features,
                position_encoding: initialized.then(|| match client.offset_encoding() {
                    OffsetEncoding::Utf8 => "utf-8",
                    OffsetEncoding::Utf16 => "utf-16",
                    OffsetEncoding::Utf32 => "utf-32",
                }),
            });
        }
        servers.sort_by(|a, b| (&a.name, a.id).cmp(&(&b.name, b.id)));
        Ok(json!({ "servers": servers }))
    })
}

// --- lsp/query: params ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Definition,
    TypeDefinition,
    Implementation,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    IncomingCalls,
    OutgoingCalls,
    RenamePreview,
}

impl QueryKind {
    const ALL: [(&'static str, QueryKind); 10] = [
        ("definition", QueryKind::Definition),
        ("typeDefinition", QueryKind::TypeDefinition),
        ("implementation", QueryKind::Implementation),
        ("references", QueryKind::References),
        ("hover", QueryKind::Hover),
        ("documentSymbols", QueryKind::DocumentSymbols),
        ("workspaceSymbols", QueryKind::WorkspaceSymbols),
        ("incomingCalls", QueryKind::IncomingCalls),
        ("outgoingCalls", QueryKind::OutgoingCalls),
        ("renamePreview", QueryKind::RenamePreview),
    ];

    fn parse(name: &str) -> Option<QueryKind> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
    }

    fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, k)| *k == self)
            .map_or("", |(n, _)| n)
    }

    fn feature(self) -> LanguageServerFeature {
        use LanguageServerFeature as F;
        match self {
            QueryKind::Definition => F::GotoDefinition,
            QueryKind::TypeDefinition => F::GotoTypeDefinition,
            QueryKind::Implementation => F::GotoImplementation,
            QueryKind::References => F::GotoReference,
            QueryKind::Hover => F::Hover,
            QueryKind::DocumentSymbols => F::DocumentSymbols,
            QueryKind::WorkspaceSymbols => F::WorkspaceSymbols,
            QueryKind::IncomingCalls | QueryKind::OutgoingCalls => F::CallHierarchy,
            QueryKind::RenamePreview => F::RenameSymbol,
        }
    }

    fn needs_position(self) -> bool {
        !matches!(
            self,
            QueryKind::DocumentSymbols | QueryKind::WorkspaceSymbols
        )
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryParams {
    kind: String,
    path: Option<String>,
    position: Option<Position>,
    query: Option<String>,
    #[serde(default = "yes")]
    include_declaration: bool,
    new_name: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
    timeout_ms: Option<u64>,
    #[serde(default = "yes")]
    ensure: bool,
    /// Add `timing` (server and bridge time) to the result.
    #[serde(default)]
    timing: bool,
}

/// What to ask, independent of the server.
#[derive(Debug, Clone)]
struct Ask {
    kind: QueryKind,
    position: Option<Position>,
    query: String,
    include_declaration: bool,
    new_name: String,
}

#[derive(Debug, Clone, Copy)]
struct Page {
    limit: usize,
    offset: usize,
}

impl Page {
    /// Cut a list to the page; returns the page, the total and whether
    /// anything after the page was left out.
    fn apply<T>(self, items: Vec<T>) -> (Vec<T>, usize, bool) {
        let total = items.len();
        let page: Vec<T> = items
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        let truncated = self.offset.saturating_add(page.len()) < total;
        (page, total, truncated)
    }
}

// --- lsp/query: issuing ----------------------------------------------------------

/// A server's raw answer.
enum Answer {
    Goto(Option<lsp::GotoDefinitionResponse>),
    References(Option<Vec<lsp::Location>>),
    Hover(Option<lsp::Hover>),
    DocumentSymbols(Option<lsp::DocumentSymbolResponse>),
    WorkspaceSymbols(Option<lsp::WorkspaceSymbolResponse>),
    Prepare(Option<Vec<lsp::CallHierarchyItem>>),
    Incoming(Vec<(lsp::CallHierarchyItem, Vec<lsp::CallHierarchyIncomingCall>)>),
    Outgoing(Vec<(lsp::CallHierarchyItem, Vec<lsp::CallHierarchyOutgoingCall>)>),
    Rename(Option<lsp::WorkspaceEdit>),
}

/// Sends `$/cancelRequest` when dropped before the server answered.
struct CancelOnDrop {
    client: Arc<Client>,
    id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            log::debug!(
                "bridge: cancelling request {} to {}",
                self.id,
                self.client.name()
            );
            self.client
                .notify::<lsp::notification::Cancel>(lsp::CancelParams {
                    id: lsp::NumberOrString::Number(self.id as i32),
                });
        }
    }
}

/// A request on its way to the server.
struct Pending {
    request: BoxFuture<'static, helix_lsp::Result<Answer>>,
    cancel: CancelOnDrop,
    /// When the request was handed to Helix's client.
    sent: Instant,
}

impl Pending {
    /// Must be called right after the request method returned, on the same
    /// thread, so the last request id is this request's.
    fn new(client: &Arc<Client>, request: BoxFuture<'static, helix_lsp::Result<Answer>>) -> Self {
        Pending {
            request,
            cancel: CancelOnDrop {
                client: client.clone(),
                id: Client::last_request_id(),
                armed: true,
            },
            sent: Instant::now(),
        }
    }

    async fn wait(mut self) -> helix_lsp::Result<Answer> {
        let answer = (&mut self.request).await;
        // The server answered (or is gone); there is nothing left to cancel.
        // Helix's own request timeout leaves the request running there.
        if !matches!(answer, Err(helix_lsp::Error::Timeout(_))) {
            self.cancel.armed = false;
        }
        answer
    }
}

/// Issue the request with Helix's typed client method. `None` when the
/// server does not support it or something it needs is missing.
fn issue(
    client: &Arc<Client>,
    ask: &Ask,
    doc: Option<lsp::TextDocumentIdentifier>,
    pos: Option<lsp::Position>,
) -> Option<Pending> {
    let at = || Some((doc.clone()?, pos?));
    let request = match ask.kind {
        QueryKind::Definition => {
            let (doc, pos) = at()?;
            let request = client.goto_definition(doc, pos, None)?;
            request.map(|r| r.map(Answer::Goto)).boxed()
        }
        QueryKind::TypeDefinition => {
            let (doc, pos) = at()?;
            let request = client.goto_type_definition(doc, pos, None)?;
            request.map(|r| r.map(Answer::Goto)).boxed()
        }
        QueryKind::Implementation => {
            let (doc, pos) = at()?;
            let request = client.goto_implementation(doc, pos, None)?;
            request.map(|r| r.map(Answer::Goto)).boxed()
        }
        QueryKind::References => {
            let (doc, pos) = at()?;
            let request = client.goto_reference(doc, pos, ask.include_declaration, None)?;
            request.map(|r| r.map(Answer::References)).boxed()
        }
        QueryKind::Hover => {
            let (doc, pos) = at()?;
            let request = client.text_document_hover(doc, pos, None)?;
            request.map(|r| r.map(Answer::Hover)).boxed()
        }
        QueryKind::DocumentSymbols => {
            let request = client.document_symbols(doc?)?;
            request.map(|r| r.map(Answer::DocumentSymbols)).boxed()
        }
        QueryKind::WorkspaceSymbols => {
            let request = client.workspace_symbols(ask.query.clone())?;
            request.map(|r| r.map(Answer::WorkspaceSymbols)).boxed()
        }
        QueryKind::IncomingCalls | QueryKind::OutgoingCalls => {
            let (doc, pos) = at()?;
            let request = client.prepare_call_hierarchy(doc, pos)?;
            request.map(|r| r.map(Answer::Prepare)).boxed()
        }
        QueryKind::RenamePreview => {
            let (doc, pos) = at()?;
            let request = client.rename_symbol(doc, pos, ask.new_name.clone())?;
            request.map(|r| r.map(Answer::Rename)).boxed()
        }
    };
    Some(Pending::new(client, request))
}

/// The server answering a query.
#[derive(Clone)]
struct Server {
    client: Arc<Client>,
    encoding: OffsetEncoding,
}

impl Server {
    fn new(client: &Arc<Client>) -> Server {
        Server {
            client: client.clone(),
            encoding: client.offset_encoding(),
        }
    }
}

enum Prepared {
    /// The request is on its way.
    Issued {
        pending: Pending,
        server: Server,
        texts: Texts,
    },
    /// The file is not open: convert the position against the file on disk
    /// and issue off the main thread.
    Disk { server: Server, texts: Texts },
    /// A server for the file is starting; look again shortly.
    Starting,
}

runtime_local! {
    /// Documents the bridge loaded in the background for queries, with the
    /// number of queries using each. Touched on the main thread only.
    static LOADED: Mutex<Vec<(DocumentId, usize)>> = Mutex::new(Vec::new());
}

/// Closes a document the bridge loaded once the last query using it is
/// done, unless the user has it in a pane or it was modified since.
#[derive(Default)]
struct Loaded(Option<DocumentId>);

impl Drop for Loaded {
    fn drop(&mut self) {
        let Some(id) = self.0.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(crate::job::dispatch(move |editor, _| {
            let mut loaded = LOADED.lock();
            let Some(i) = loaded.iter().position(|(doc, _)| *doc == id) else {
                return;
            };
            loaded[i].1 -= 1;
            if loaded[i].1 > 0 {
                return;
            }
            loaded.swap_remove(i);
            drop(loaded);
            let shown = editor.tree.views().any(|(view, _)| view.doc == id);
            let modified = editor.document(id).is_none_or(|doc| doc.is_modified());
            if !shown && !modified {
                let _ = editor.close_document(id, false);
            }
        }));
    }
}

/// Main thread: find the document and server, load the file in the
/// background if asked to, and issue the request.
fn prepare(
    editor: &mut Editor,
    path: Option<&Path>,
    exists: bool,
    ensure: bool,
    loaded: &mut Loaded,
    ask: &Ask,
) -> Result<Prepared, RpcError> {
    let feature = ask.kind.feature();
    let Some(path) = path else {
        // workspaceSymbols without a file
        let server = workspace_server(editor, feature).ok_or_else(|| no_server(None, ask.kind))?;
        let pending =
            issue(&server.client, ask, None, None).ok_or_else(|| no_server(None, ask.kind))?;
        return Ok(Prepared::Issued {
            pending,
            server,
            texts: Texts::snapshot(editor),
        });
    };
    let doc_id = match editor.document_id_by_path(path) {
        Some(id) => {
            if loaded.0 != Some(id) {
                let mut counts = LOADED.lock();
                if let Some((_, n)) = counts.iter_mut().find(|(doc, _)| *doc == id) {
                    // loaded by another query that is still running
                    *n += 1;
                    loaded.0 = Some(id);
                }
            }
            Some(id)
        }
        None if !exists => {
            return Err(RpcError::not_found(format!("{} not found", path.display())))
        }
        None if ensure => {
            let id = editor
                .open(path, Action::Load)
                .map_err(|err| RpcError::not_found(format!("{}: {err}", path.display())))?;
            LOADED.lock().push((id, 1));
            loaded.0 = Some(id);
            Some(id)
        }
        None => None,
    };
    let Some(doc_id) = doc_id else {
        return disk_server(editor, path, ask.kind);
    };
    let doc = editor
        .document(doc_id)
        .ok_or_else(|| RpcError::not_found(format!("{} not found", path.display())))?;
    let client = doc
        .language_servers_with_feature(feature)
        .next()
        .and_then(|ls| editor.language_servers.get_by_id(ls.id()));
    let Some(client) = client else {
        if starting(editor, doc.language_config()) {
            return Ok(Prepared::Starting);
        }
        return Err(no_server(doc.language_name(), ask.kind));
    };
    let server = Server::new(client);
    let text = doc.text();
    let pos = ask
        .position
        .map(|p| pos_to_lsp_pos(text, char_from_pos(text.slice(..), p), server.encoding));
    let pending = issue(&server.client, ask, Some(doc.identifier()), pos)
        .ok_or_else(|| no_server(doc.language_name(), ask.kind))?;
    Ok(Prepared::Issued {
        pending,
        server,
        texts: Texts::snapshot(editor),
    })
}

/// A server for a file that is not open: one attached to the file's
/// language, in the order the language configuration lists them.
fn disk_server(editor: &Editor, path: &Path, kind: QueryKind) -> Result<Prepared, RpcError> {
    let loader = editor.syn_loader.load();
    let Some(config) = loader
        .language_for_filename(path)
        .map(|lang| loader.language(lang).config().clone())
    else {
        return Err(no_server(None, kind));
    };
    let feature = kind.feature();
    let client = config
        .language_servers
        .iter()
        .filter(|ls| ls.has_feature(feature))
        .find_map(|ls| {
            editor
                .language_servers
                .iter_clients()
                .find(|c| c.name() == ls.name && c.is_initialized() && c.supports_feature(feature))
        });
    match client {
        Some(client) => Ok(Prepared::Disk {
            server: Server::new(client),
            texts: Texts::snapshot(editor),
        }),
        None if starting(editor, Some(&config)) => Ok(Prepared::Starting),
        None => Err(no_server(Some(&config.language_id), kind)),
    }
}

/// For `workspaceSymbols` without a path: the focused document's server,
/// else any running server that supports it.
fn workspace_server(editor: &Editor, feature: LanguageServerFeature) -> Option<Server> {
    let focused = editor
        .tree
        .try_get(editor.tree.focus)
        .and_then(|view| editor.document(view.doc))
        .and_then(|doc| doc.language_servers_with_feature(feature).next())
        .and_then(|ls| editor.language_servers.get_by_id(ls.id()));
    focused
        .or_else(|| {
            editor
                .language_servers
                .iter_clients()
                .find(|c| c.is_initialized() && c.supports_feature(feature))
        })
        .map(Server::new)
}

/// Is a server configured for this language still initializing?
fn starting(editor: &Editor, config: Option<&LanguageConfiguration>) -> bool {
    let Some(config) = config else {
        return false;
    };
    editor.language_servers.iter_clients().any(|c| {
        !c.is_initialized() && config.language_servers.iter().any(|ls| ls.name == c.name())
    })
}

fn no_server(language: Option<&str>, kind: QueryKind) -> RpcError {
    let message = match language {
        Some(lang) => format!(
            "no running language server for {lang} supports {}",
            kind.name()
        ),
        None => format!("no running language server supports {}", kind.name()),
    };
    RpcError {
        code: ErrorCode::NoLanguageServer,
        message,
        data: Some(json!({ "language": language, "kind": kind.name() })),
    }
}

fn server_error(client: &Client, err: helix_lsp::Error) -> RpcError {
    let name = client.name();
    match err {
        helix_lsp::Error::Timeout(_) => RpcError::new(
            ErrorCode::Timeout,
            format!("{name} did not answer within Helix's request timeout"),
        ),
        helix_lsp::Error::StreamClosed => RpcError {
            code: ErrorCode::NoLanguageServer,
            message: format!("{name} exited"),
            data: Some(json!({ "server": name })),
        },
        helix_lsp::Error::Rpc(err) => RpcError {
            code: ErrorCode::Unsupported,
            message: format!("{name}: {}", err.message),
            data: Some(json!({ "server": name, "serverCode": err.code.code() })),
        },
        err => RpcError {
            code: ErrorCode::Unsupported,
            message: format!("{name}: {err}"),
            data: Some(json!({ "server": name })),
        },
    }
}

// --- lsp/query: the method -------------------------------------------------------

pub fn query(cx: Ctx, params: Value) -> BoxFuture<'static, RpcResult> {
    Box::pin(async move {
        let begin = Instant::now();
        let params: QueryParams = parse_params(params)?;
        let kind = QueryKind::parse(&params.kind).ok_or_else(|| {
            RpcError::invalid_params("kind", format!("unknown kind: {}", params.kind))
        })?;
        if kind != QueryKind::WorkspaceSymbols && params.path.is_none() {
            return Err(RpcError::invalid_params(
                "path",
                format!("{} needs a path", kind.name()),
            ));
        }
        if kind.needs_position() && params.position.is_none() {
            return Err(RpcError::invalid_params(
                "position",
                format!("{} needs a position", kind.name()),
            ));
        }
        if kind == QueryKind::WorkspaceSymbols && params.query.is_none() {
            return Err(RpcError::invalid_params(
                "query",
                "workspaceSymbols needs a query",
            ));
        }
        if kind == QueryKind::RenamePreview && params.new_name.as_deref().unwrap_or("").is_empty() {
            return Err(RpcError::invalid_params(
                "newName",
                "renamePreview needs a newName",
            ));
        }
        let timeout_ms = params
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let timing = params.timing;
        let run = run_query(cx, kind, params);
        let (mut result, times) =
            match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(RpcError::new(
                        ErrorCode::Timeout,
                        format!("the language server did not answer within {timeout_ms} ms"),
                    ))
                }
            };
        if timing {
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            result["timing"] = json!({
                "issueMs": ms(times.sent.duration_since(begin)),
                "serverMs": ms(times.answered.duration_since(times.sent)),
                "shapeMs": ms(times.shaped.duration_since(times.answered)),
                "totalMs": ms(begin.elapsed()),
            });
        }
        Ok(result)
    })
}

/// Everything after validation. Dropping this future (timeout, `$/cancel`,
/// client gone) cancels the server request and releases a background load.
async fn run_query(
    cx: Ctx,
    kind: QueryKind,
    params: QueryParams,
) -> Result<(Value, Times), RpcError> {
    let path = params
        .path
        .as_deref()
        .map(|p| resolve_path(p, &cx.shared.info.workspace_root));
    let exists = match &path {
        Some(path) => match tokio::fs::metadata(path).await {
            Ok(meta) if meta.is_dir() => {
                return Err(RpcError::not_found(format!(
                    "{} is a directory",
                    path.display()
                )))
            }
            Ok(_) => true,
            Err(_) => false,
        },
        None => false,
    };
    let ask = Ask {
        kind,
        position: params.position,
        query: params.query.unwrap_or_default(),
        include_declaration: params.include_declaration,
        new_name: params.new_name.unwrap_or_default(),
    };
    let page = Page {
        limit: params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_ITEMS),
        offset: params.offset,
    };
    let ensure = params.ensure;

    let mut loaded = Loaded::default();
    let (pending, server, texts) = loop {
        let (target, a) = (path.clone(), ask.clone());
        let (prepared, l) = on_editor(move |editor, _| {
            let mut loaded = loaded;
            let prepared = prepare(editor, target.as_deref(), exists, ensure, &mut loaded, &a);
            (prepared, loaded)
        })
        .await?;
        loaded = l;
        match prepared? {
            Prepared::Issued {
                pending,
                server,
                texts,
            } => break (pending, server, texts),
            Prepared::Disk { server, mut texts } => {
                let path = path.clone().unwrap_or_default();
                let (p, s, ask) = (path.clone(), server.clone(), ask.clone());
                let (pending, texts) = tokio::task::spawn_blocking(move || {
                    let (path, server) = (p, s);
                    let text = texts.get(&path).cloned();
                    let doc = lsp::Url::from_file_path(&path)
                        .ok()
                        .map(lsp::TextDocumentIdentifier::new);
                    let pos = match (text, ask.position) {
                        (Some(text), Some(p)) => Some(pos_to_lsp_pos(
                            &text,
                            char_from_pos(text.slice(..), p),
                            server.encoding,
                        )),
                        (None, Some(_)) => None,
                        (_, None) => Some(lsp::Position::default()),
                    };
                    (issue(&server.client, &ask, doc, pos), texts)
                })
                .await
                .map_err(|err| RpcError::new(ErrorCode::Unsupported, err.to_string()))?;
                let Some(pending) = pending else {
                    return Err(RpcError::not_found(format!(
                        "cannot read {}",
                        path.display()
                    )));
                };
                break (pending, server, texts);
            }
            Prepared::Starting => tokio::time::sleep(STARTING_POLL).await,
        }
    };

    let sent = pending.sent;
    let client = server.client.clone();
    let mut answer = pending
        .wait()
        .await
        .map_err(|err| server_error(&client, err))?;
    if let Answer::Prepare(items) = answer {
        answer = calls(&client, kind, items.unwrap_or_default()).await?;
    }
    let answered = Instant::now();
    let doc_path = path;
    let result = tokio::task::spawn_blocking(move || {
        shape(answer, texts, server.encoding, doc_path.as_deref(), page)
    })
    .await
    .map_err(|err| RpcError::new(ErrorCode::Unsupported, err.to_string()))?;
    drop(loaded);
    let times = Times {
        sent,
        answered,
        shaped: Instant::now(),
    };
    Ok((result, times))
}

/// For `timing`: request handed to Helix's client, answer received (both
/// steps for call hierarchy), result built.
struct Times {
    sent: Instant,
    answered: Instant,
    shaped: Instant,
}

/// The second step of a call-hierarchy query, for every prepared item.
async fn calls(
    client: &Arc<Client>,
    kind: QueryKind,
    items: Vec<lsp::CallHierarchyItem>,
) -> Result<Answer, RpcError> {
    let mut incoming = Vec::new();
    let mut outgoing = Vec::new();
    for item in items {
        let pending = if kind == QueryKind::IncomingCalls {
            client.call_hierarchy_incoming(item.clone()).map(|request| {
                let request = request.map(move |r| {
                    r.map(|calls| Answer::Incoming(vec![(item, calls.unwrap_or_default())]))
                });
                Pending::new(client, request.boxed())
            })
        } else {
            client.call_hierarchy_outgoing(item.clone()).map(|request| {
                let request = request.map(move |r| {
                    r.map(|calls| Answer::Outgoing(vec![(item, calls.unwrap_or_default())]))
                });
                Pending::new(client, request.boxed())
            })
        };
        let Some(pending) = pending else {
            return Err(no_server(None, kind));
        };
        match pending
            .wait()
            .await
            .map_err(|err| server_error(client, err))?
        {
            Answer::Incoming(calls) => incoming.extend(calls),
            Answer::Outgoing(calls) => outgoing.extend(calls),
            _ => {}
        }
    }
    Ok(if kind == QueryKind::IncomingCalls {
        Answer::Incoming(incoming)
    } else {
        Answer::Outgoing(outgoing)
    })
}

// --- lsp/query: shaping the answer -------------------------------------------------

/// Text of the files an answer refers to: open buffers as they were when the
/// request was issued (what the server saw), otherwise the file on disk.
pub(crate) struct Texts {
    open: HashMap<PathBuf, Rope>,
    disk: HashMap<PathBuf, Option<Rope>>,
}

impl Texts {
    /// Main thread: an O(1) clone of every open buffer's text.
    pub fn snapshot(editor: &Editor) -> Texts {
        Texts {
            open: editor
                .documents()
                .filter_map(|doc| Some((doc.path()?.to_path_buf(), doc.text().clone())))
                .collect(),
            disk: HashMap::new(),
        }
    }

    pub fn new(open: HashMap<PathBuf, Rope>) -> Texts {
        Texts {
            open,
            disk: HashMap::new(),
        }
    }

    /// May read from disk: call off the main thread, in a blocking task.
    pub fn get(&mut self, path: &Path) -> Option<&Rope> {
        if self.open.contains_key(path) {
            return self.open.get(path);
        }
        self.disk
            .entry(path.to_path_buf())
            .or_insert_with(|| read_disk(path))
            .as_ref()
    }
}

fn read_disk(path: &Path) -> Option<Rope> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_DISK_READ {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(Rope::from(String::from_utf8_lossy(&bytes).as_ref()))
}

/// Server position -> protocol position. Without the text (the file cannot
/// be read) one code unit is taken as one char.
pub(crate) fn pos_from_lsp(
    text: Option<&Rope>,
    pos: lsp::Position,
    enc: OffsetEncoding,
) -> Position {
    match text.and_then(|t| Some((t, lsp_pos_to_pos(t, pos, enc)?))) {
        Some((text, ch)) => pos_from_char(text.slice(..), ch),
        None => Position {
            line: pos.line as usize + 1,
            col: pos.character as usize + 1,
        },
    }
}

pub(crate) fn range_from_lsp(text: Option<&Rope>, range: lsp::Range, enc: OffsetEncoding) -> Range {
    // as Helix does: a range that ends before it starts is empty at its end
    let start = if range.start > range.end {
        range.end
    } else {
        range.start
    };
    Range {
        start: pos_from_lsp(text, start, enc),
        end: pos_from_lsp(text, range.end, enc),
    }
}

/// The line, without its line ending, cut to `PREVIEW_CHARS`.
fn preview(text: Option<&Rope>, line: u32) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let line = line as usize;
    if line >= text.len_lines() {
        return String::new();
    }
    let start = text.line_to_char(line);
    let end = line_end_char_index(&text.slice(..), line).min(start + PREVIEW_CHARS);
    text.slice(start..end).to_string()
}

#[derive(Debug, Serialize)]
struct Location {
    path: String,
    range: Range,
    preview: String,
}

/// A location before conversion: the server's path and range.
#[derive(Debug, Clone)]
struct RawLoc {
    path: PathBuf,
    range: lsp::Range,
}

impl RawLoc {
    fn new(uri: &lsp::Url, range: lsp::Range) -> Option<RawLoc> {
        match Uri::try_from(uri) {
            Ok(uri) => Some(RawLoc {
                path: uri.as_path()?.to_path_buf(),
                range,
            }),
            Err(err) => {
                log::warn!("bridge: discarding location with unsupported URI: {err}");
                None
            }
        }
    }

    fn sort_key(&self) -> (&Path, u32, u32) {
        (
            &self.path,
            self.range.start.line,
            self.range.start.character,
        )
    }
}

struct Shaper {
    texts: Texts,
    enc: OffsetEncoding,
}

impl Shaper {
    fn range(&mut self, path: &Path, range: lsp::Range) -> Range {
        let enc = self.enc;
        range_from_lsp(self.texts.get(path), range, enc)
    }

    fn location(&mut self, loc: &RawLoc) -> Location {
        let enc = self.enc;
        let text = self.texts.get(&loc.path);
        Location {
            path: loc.path.to_string_lossy().into_owned(),
            range: range_from_lsp(text, loc.range, enc),
            preview: preview(text, loc.range.start.line),
        }
    }
}

#[derive(Debug)]
struct RawSymbol {
    name: String,
    kind: lsp::SymbolKind,
    loc: RawLoc,
    container: Option<String>,
    /// The whole symbol (document symbols in nested form).
    extent: Option<lsp::Range>,
}

#[derive(Debug)]
struct RawCall {
    name: String,
    kind: lsp::SymbolKind,
    loc: RawLoc,
    sites: Vec<RawLoc>,
}

#[derive(Debug)]
struct RawEdit {
    path: PathBuf,
    range: lsp::Range,
    new_text: String,
}

fn shape(
    answer: Answer,
    texts: Texts,
    enc: OffsetEncoding,
    doc: Option<&Path>,
    page: Page,
) -> Value {
    let mut s = Shaper { texts, enc };
    match answer {
        Answer::Goto(response) => {
            let locs = match response {
                None => Vec::new(),
                Some(lsp::GotoDefinitionResponse::Scalar(loc)) => {
                    RawLoc::new(&loc.uri, loc.range).into_iter().collect()
                }
                Some(lsp::GotoDefinitionResponse::Array(locs)) => locs
                    .iter()
                    .filter_map(|l| RawLoc::new(&l.uri, l.range))
                    .collect(),
                Some(lsp::GotoDefinitionResponse::Link(links)) => links
                    .iter()
                    .filter_map(|l| RawLoc::new(&l.target_uri, l.target_selection_range))
                    .collect(),
            };
            locations(&mut s, locs, page)
        }
        Answer::References(locs) => {
            let mut locs: Vec<RawLoc> = locs
                .unwrap_or_default()
                .iter()
                .filter_map(|l| RawLoc::new(&l.uri, l.range))
                .collect();
            locs.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
            locs.dedup_by(|a, b| a.path == b.path && a.range == b.range);
            locations(&mut s, locs, page)
        }
        Answer::Hover(hover) => json!({ "markdown": hover_markdown(hover) }),
        Answer::DocumentSymbols(response) => {
            let mut symbols = Vec::new();
            match response {
                None => {}
                Some(lsp::DocumentSymbolResponse::Flat(flat)) => {
                    symbols.extend(flat.into_iter().filter_map(symbol_information));
                }
                Some(lsp::DocumentSymbolResponse::Nested(nested)) => {
                    if let Some(doc) = doc {
                        for symbol in nested {
                            flatten(symbol, doc, None, &mut symbols);
                        }
                    }
                }
            }
            symbols_json(&mut s, symbols, page)
        }
        Answer::WorkspaceSymbols(response) => {
            let symbols = match response {
                None => Vec::new(),
                Some(lsp::WorkspaceSymbolResponse::Flat(flat)) => {
                    flat.into_iter().filter_map(symbol_information).collect()
                }
                Some(lsp::WorkspaceSymbolResponse::Nested(nested)) => nested
                    .into_iter()
                    .filter_map(|symbol| {
                        let loc = match &symbol.location {
                            lsp::OneOf::Left(loc) => RawLoc::new(&loc.uri, loc.range)?,
                            lsp::OneOf::Right(loc) => RawLoc::new(&loc.uri, lsp::Range::default())?,
                        };
                        Some(RawSymbol {
                            name: symbol.name,
                            kind: symbol.kind,
                            loc,
                            container: symbol.container_name,
                            extent: None,
                        })
                    })
                    .collect(),
            };
            symbols_json(&mut s, symbols, page)
        }
        Answer::Incoming(items) => {
            let calls = items
                .into_iter()
                .flat_map(|(_, calls)| calls)
                .filter_map(|call| {
                    let from = call.from;
                    let sites = call
                        .from_ranges
                        .iter()
                        .filter_map(|r| RawLoc::new(&from.uri, *r))
                        .collect();
                    Some(RawCall {
                        loc: RawLoc::new(&from.uri, from.selection_range)?,
                        name: from.name,
                        kind: from.kind,
                        sites,
                    })
                })
                .collect();
            calls_json(&mut s, calls, page)
        }
        Answer::Outgoing(items) => {
            let calls = items
                .into_iter()
                .flat_map(|(item, calls)| calls.into_iter().map(move |c| (item.uri.clone(), c)))
                .filter_map(|(caller, call)| {
                    let to = call.to;
                    let sites = call
                        .from_ranges
                        .iter()
                        .filter_map(|r| RawLoc::new(&caller, *r))
                        .collect();
                    Some(RawCall {
                        loc: RawLoc::new(&to.uri, to.selection_range)?,
                        name: to.name,
                        kind: to.kind,
                        sites,
                    })
                })
                .collect();
            calls_json(&mut s, calls, page)
        }
        Answer::Rename(edit) => rename_json(&mut s, edit.unwrap_or_default(), page),
        Answer::Prepare(_) => json!({}),
    }
}

fn locations(s: &mut Shaper, locs: Vec<RawLoc>, page: Page) -> Value {
    let (locs, total, truncated) = page.apply(locs);
    let locations: Vec<Location> = locs.iter().map(|l| s.location(l)).collect();
    json!({ "locations": locations, "total": total, "truncated": truncated })
}

#[allow(deprecated)]
fn symbol_information(symbol: lsp::SymbolInformation) -> Option<RawSymbol> {
    Some(RawSymbol {
        loc: RawLoc::new(&symbol.location.uri, symbol.location.range)?,
        name: symbol.name,
        kind: symbol.kind,
        container: symbol.container_name.filter(|c| !c.is_empty()),
        extent: None,
    })
}

fn flatten(
    symbol: lsp::DocumentSymbol,
    doc: &Path,
    container: Option<&str>,
    out: &mut Vec<RawSymbol>,
) {
    out.push(RawSymbol {
        name: symbol.name.clone(),
        kind: symbol.kind,
        loc: RawLoc {
            path: doc.to_path_buf(),
            range: symbol.selection_range,
        },
        container: container.map(str::to_owned),
        extent: Some(symbol.range),
    });
    for child in symbol.children.into_iter().flatten() {
        flatten(child, doc, Some(&symbol.name), out);
    }
}

fn symbols_json(s: &mut Shaper, symbols: Vec<RawSymbol>, page: Page) -> Value {
    let (symbols, total, truncated) = page.apply(symbols);
    let symbols: Vec<Value> = symbols
        .into_iter()
        .map(|sym| {
            let mut v = json!({
                "name": sym.name,
                "kind": symbol_kind(sym.kind),
                "location": s.location(&sym.loc),
            });
            if let Some(container) = sym.container {
                v["container"] = container.into();
            }
            if let Some(extent) = sym.extent {
                v["extent"] = json!(s.range(&sym.loc.path, extent));
            }
            v
        })
        .collect();
    json!({ "symbols": symbols, "total": total, "truncated": truncated })
}

fn calls_json(s: &mut Shaper, calls: Vec<RawCall>, page: Page) -> Value {
    let (calls, total, truncated) = page.apply(calls);
    let calls: Vec<Value> = calls
        .into_iter()
        .map(|call| {
            let sites: Vec<Location> = call
                .sites
                .iter()
                .take(MAX_CALL_SITES)
                .map(|l| s.location(l))
                .collect();
            json!({
                "name": call.name,
                "kind": symbol_kind(call.kind),
                "location": s.location(&call.loc),
                "callSites": sites,
            })
        })
        .collect();
    json!({ "calls": calls, "total": total, "truncated": truncated })
}

fn rename_json(s: &mut Shaper, edit: lsp::WorkspaceEdit, page: Page) -> Value {
    let mut edits = Vec::new();
    let mut ops = Vec::new();
    let mut push = |uri: &lsp::Url, range: lsp::Range, new_text: String| {
        if let Some(loc) = RawLoc::new(uri, range) {
            edits.push(RawEdit {
                path: loc.path,
                range,
                new_text,
            });
        }
    };
    let text_edits = |e: lsp::TextDocumentEdit,
                      push: &mut dyn FnMut(&lsp::Url, lsp::Range, String)| {
        for edit in e.edits {
            let edit = match edit {
                lsp::OneOf::Left(edit) => edit,
                lsp::OneOf::Right(annotated) => annotated.text_edit,
            };
            push(&e.text_document.uri, edit.range, edit.new_text);
        }
    };
    match edit.document_changes {
        Some(lsp::DocumentChanges::Edits(changes)) => {
            for change in changes {
                text_edits(change, &mut push);
            }
        }
        Some(lsp::DocumentChanges::Operations(changes)) => {
            for change in changes {
                match change {
                    lsp::DocumentChangeOperation::Edit(change) => text_edits(change, &mut push),
                    lsp::DocumentChangeOperation::Op(op) => ops.push(file_operation(op)),
                }
            }
        }
        None => {
            for (uri, changes) in edit.changes.iter().flatten() {
                for change in changes {
                    push(uri, change.range, change.new_text.clone());
                }
            }
        }
    }
    edits.sort_by(|a, b| {
        (&a.path, a.range.start.line, a.range.start.character).cmp(&(
            &b.path,
            b.range.start.line,
            b.range.start.character,
        ))
    });
    let mut files: Vec<String> = edits
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    files.dedup();
    let (edits, total, truncated) = page.apply(edits);
    let edits: Vec<Value> = edits
        .into_iter()
        .map(|e| {
            json!({
                "path": e.path.to_string_lossy(),
                "range": s.range(&e.path, e.range),
                "newText": e.new_text,
            })
        })
        .collect();
    let mut result =
        json!({ "edits": edits, "files": files, "total": total, "truncated": truncated });
    if !ops.is_empty() {
        result["fileOperations"] = ops.into();
    }
    result
}

fn file_operation(op: lsp::ResourceOp) -> Value {
    let path = |uri: &lsp::Url| {
        uri.to_file_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| uri.to_string())
    };
    match op {
        lsp::ResourceOp::Create(op) => json!({ "kind": "create", "path": path(&op.uri) }),
        lsp::ResourceOp::Rename(op) => {
            json!({ "kind": "rename", "path": path(&op.old_uri), "newPath": path(&op.new_uri) })
        }
        lsp::ResourceOp::Delete(op) => json!({ "kind": "delete", "path": path(&op.uri) }),
    }
}

fn hover_markdown(hover: Option<lsp::Hover>) -> String {
    fn marked(m: lsp::MarkedString) -> String {
        match m {
            lsp::MarkedString::String(s) => s,
            lsp::MarkedString::LanguageString(s) => format!("```{}\n{}\n```", s.language, s.value),
        }
    }
    match hover.map(|h| h.contents) {
        None => String::new(),
        Some(lsp::HoverContents::Scalar(m)) => marked(m),
        Some(lsp::HoverContents::Array(ms)) => {
            ms.into_iter().map(marked).collect::<Vec<_>>().join("\n\n")
        }
        Some(lsp::HoverContents::Markup(m)) => m.value,
    }
}

/// `function`, `method`, `enumMember`, … (LSP's names in camel case).
fn symbol_kind(kind: lsp::SymbolKind) -> String {
    let name = format!("{kind:?}");
    if name.starts_with("SymbolKind(") {
        return "unknown".into();
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(line: u32, character: u32) -> lsp::Position {
        lsp::Position { line, character }
    }

    fn p(line: usize, col: usize) -> Position {
        Position { line, col }
    }

    /// Protocol -> server -> protocol for every encoding, with multi-byte
    /// chars before and after the position.
    #[test]
    fn positions_round_trip_in_every_encoding() {
        // 'é' 2 bytes / 1 UTF-16 unit, '🦀' 4 bytes / 2 UTF-16 units
        let text = Rope::from("aé🦀x🦀é\nzzz\r\nend");
        let cases = [
            // (protocol position, utf-8, utf-16, utf-32)
            (p(1, 1), lp(0, 0), lp(0, 0), lp(0, 0)),
            (p(1, 3), lp(0, 3), lp(0, 2), lp(0, 2)), // on the crab
            (p(1, 4), lp(0, 7), lp(0, 4), lp(0, 3)), // 'x' after the crab
            (p(1, 6), lp(0, 12), lp(0, 7), lp(0, 5)), // 'é' after the second crab
            (p(2, 2), lp(1, 1), lp(1, 1), lp(1, 1)),
            (p(3, 3), lp(2, 2), lp(2, 2), lp(2, 2)),
        ];
        for (pos, u8, u16, u32) in cases {
            for (enc, lsp_pos) in [
                (OffsetEncoding::Utf8, u8),
                (OffsetEncoding::Utf16, u16),
                (OffsetEncoding::Utf32, u32),
            ] {
                let ch = char_from_pos(text.slice(..), pos);
                assert_eq!(pos_to_lsp_pos(&text, ch, enc), lsp_pos, "{pos:?} {enc:?}");
                assert_eq!(
                    pos_from_lsp(Some(&text), lsp_pos, enc),
                    pos,
                    "{lsp_pos:?} {enc:?}"
                );
            }
        }
    }

    #[test]
    fn positions_past_the_end_clamp() {
        let text = Rope::from("ab\ncd\n");
        let enc = OffsetEncoding::Utf16;
        // past the end of a line: end of that line
        assert_eq!(pos_from_lsp(Some(&text), lp(0, 99), enc), p(1, 3));
        // past the last line: end of the text
        assert_eq!(pos_from_lsp(Some(&text), lp(9, 0), enc), p(3, 1));
        // no text: taken as is
        assert_eq!(pos_from_lsp(None, lp(4, 7), enc), p(5, 8));
    }

    #[test]
    fn inverted_ranges_are_empty_at_the_end() {
        let text = Rope::from("abcdef");
        let r = range_from_lsp(
            Some(&text),
            lsp::Range::new(lp(0, 4), lp(0, 2)),
            OffsetEncoding::Utf16,
        );
        assert_eq!((r.start, r.end), (p(1, 3), p(1, 3)));
    }

    #[test]
    fn previews() {
        let long = "x".repeat(300);
        let text = Rope::from(format!("first é\r\n{long}\nlast"));
        assert_eq!(preview(Some(&text), 0), "first é");
        assert_eq!(preview(Some(&text), 1).chars().count(), PREVIEW_CHARS);
        assert_eq!(preview(Some(&text), 2), "last");
        assert_eq!(preview(Some(&text), 9), "");
        assert_eq!(preview(None, 0), "");
    }

    #[test]
    fn paging() {
        let page = Page {
            limit: 2,
            offset: 1,
        };
        assert_eq!(page.apply(vec![1, 2, 3, 4]), (vec![2, 3], 4, true));
        assert_eq!(page.apply(vec![1, 2, 3]), (vec![2, 3], 3, false));
        let none = Page {
            limit: 0,
            offset: 0,
        };
        assert_eq!(none.apply(vec![1]), (vec![], 1, true));
        let past = Page {
            limit: 5,
            offset: 9,
        };
        assert_eq!(past.apply(vec![1]), (vec![], 1, false));
    }

    #[test]
    fn kinds() {
        for (name, kind) in QueryKind::ALL {
            assert_eq!(QueryKind::parse(name), Some(kind));
            assert_eq!(kind.name(), name);
        }
        assert_eq!(symbol_kind(lsp::SymbolKind::FUNCTION), "function");
        assert_eq!(symbol_kind(lsp::SymbolKind::ENUM_MEMBER), "enumMember");
        assert_eq!(
            symbol_kind(lsp::SymbolKind::TYPE_PARAMETER),
            "typeParameter"
        );
        // every query kind has a feature name
        let names: Vec<&str> = FEATURES
            .iter()
            .flat_map(|(_, n)| n.iter().copied())
            .collect();
        for (name, _) in QueryKind::ALL {
            assert!(names.contains(&name), "{name}");
        }
    }

    fn texts(path: &str, text: &str) -> Texts {
        Texts::new([(PathBuf::from(path), Rope::from(text))].into())
    }

    #[test]
    fn locations_from_open_buffer_text() {
        let uri = lsp::Url::from_file_path("/r/a.go").unwrap();
        let answer = Answer::References(Some(vec![
            lsp::Location::new(uri.clone(), lsp::Range::new(lp(1, 6), lp(1, 9))),
            lsp::Location::new(uri.clone(), lsp::Range::new(lp(0, 3), lp(0, 4))),
            lsp::Location::new(uri, lsp::Range::new(lp(0, 3), lp(0, 4))),
        ]));
        // line 2: "🦀é" is 3 UTF-16 units... "x := 🦀é" -> col of 'é' is 6 units in
        let v = shape(
            answer,
            texts("/r/a.go", "a🦀bc\nx := 🦀é\n"),
            OffsetEncoding::Utf16,
            None,
            Page {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(v["total"], 2, "sorted and deduplicated: {v}");
        assert_eq!(v["truncated"], false);
        let first = &v["locations"][0];
        assert_eq!(first["path"], "/r/a.go");
        assert_eq!(first["range"]["start"], json!({ "line": 1, "col": 3 }));
        assert_eq!(first["preview"], "a🦀bc");
        let second = &v["locations"][1];
        assert_eq!(second["range"]["start"], json!({ "line": 2, "col": 6 }));
        assert_eq!(second["range"]["end"], json!({ "line": 2, "col": 8 }));
        assert_eq!(second["preview"], "x := 🦀é");
    }

    #[test]
    fn nested_document_symbols_flatten_with_container() {
        let child = lsp::DocumentSymbol {
            name: "Area".into(),
            detail: None,
            kind: lsp::SymbolKind::METHOD,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: lsp::Range::new(lp(1, 1), lp(1, 15)),
            selection_range: lsp::Range::new(lp(1, 1), lp(1, 5)),
            children: None,
        };
        let parent = lsp::DocumentSymbol {
            name: "Shape".into(),
            detail: None,
            kind: lsp::SymbolKind::INTERFACE,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: lsp::Range::new(lp(0, 0), lp(2, 1)),
            selection_range: lsp::Range::new(lp(0, 5), lp(0, 10)),
            children: Some(vec![child]),
        };
        let v = shape(
            Answer::DocumentSymbols(Some(lsp::DocumentSymbolResponse::Nested(vec![parent]))),
            texts("/r/s.go", "type Shape interface {\n\tArea() float64\n}\n"),
            OffsetEncoding::Utf16,
            Some(Path::new("/r/s.go")),
            Page {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(v["total"], 2);
        assert_eq!(v["symbols"][0]["name"], "Shape");
        assert_eq!(v["symbols"][0]["kind"], "interface");
        assert!(v["symbols"][0].get("container").is_none());
        assert_eq!(v["symbols"][1]["container"], "Shape");
        assert_eq!(
            v["symbols"][1]["location"]["range"]["start"],
            json!({ "line": 2, "col": 2 })
        );
        assert_eq!(v["symbols"][1]["location"]["preview"], "\tArea() float64");
        assert_eq!(
            v["symbols"][0]["extent"]["end"],
            json!({ "line": 3, "col": 2 })
        );
    }

    #[test]
    fn rename_edits_from_changes_and_document_changes() {
        let a = lsp::Url::from_file_path("/r/a.go").unwrap();
        let b = lsp::Url::from_file_path("/r/b.go").unwrap();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Edits(vec![
                lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: b.clone(),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(lsp::TextEdit::new(
                        lsp::Range::new(lp(0, 0), lp(0, 1)),
                        "Y".into(),
                    ))],
                },
                lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: a.clone(),
                        version: Some(3),
                    },
                    edits: vec![lsp::OneOf::Left(lsp::TextEdit::new(
                        lsp::Range::new(lp(0, 2), lp(0, 3)),
                        "Z".into(),
                    ))],
                },
            ])),
            change_annotations: None,
        };
        let mut t = texts("/r/a.go", "🦀x\n");
        t.open.insert(PathBuf::from("/r/b.go"), Rope::from("q\n"));
        let v = shape(
            Answer::Rename(Some(edit)),
            t,
            OffsetEncoding::Utf16,
            None,
            Page {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(v["files"], json!(["/r/a.go", "/r/b.go"]));
        assert_eq!(v["edits"][0]["path"], "/r/a.go");
        assert_eq!(
            v["edits"][0]["range"]["start"],
            json!({ "line": 1, "col": 2 })
        );
        assert_eq!(v["edits"][0]["newText"], "Z");
        assert_eq!(v["total"], 2);
    }

    #[test]
    fn hover_contents() {
        let h = |contents| {
            Some(lsp::Hover {
                contents,
                range: None,
            })
        };
        assert_eq!(hover_markdown(None), "");
        assert_eq!(
            hover_markdown(h(lsp::HoverContents::Markup(lsp::MarkupContent {
                kind: lsp::MarkupKind::Markdown,
                value: "**x**".into()
            }))),
            "**x**"
        );
        assert_eq!(
            hover_markdown(h(lsp::HoverContents::Array(vec![
                lsp::MarkedString::String("a".into()),
                lsp::MarkedString::LanguageString(lsp::LanguageString {
                    language: "go".into(),
                    value: "func f()".into()
                }),
            ]))),
            "a\n\n```go\nfunc f()\n```"
        );
    }
}
