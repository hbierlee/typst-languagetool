use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::*;
use lsp_types::request::*;
use lsp_types::*;
use lt_world::LtWorld;
use serde_json::Value;
use typst::syntax::Source;
use typst_languagetool::{LanguageTool, LanguageToolBackend, Suggestion};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(default)]
struct InitOptions {
	/// Additional allowed words
	dictionary: HashMap<String, Vec<String>>,
	/// Languagetool rules to ignore (WHITESPACE_RULE, ...)
	disabled_checks: HashMap<String, Vec<String>>,

	languages: Vec<String>,

	/// use bundled languagetool
	bundled: bool,
	/// use external JAR for languagetool
	jar_location: Option<String>,
	/// host for remote languagetool
	host: Option<String>,
	/// port for remote languagetool
	port: Option<String>,

	/// Size for chunk send to LanguageTool
	chunk_size: usize,
	/// Duration to wait for additional changes before checking the file
	/// Leave empty to only check on open and save
	#[serde(with = "humantime_serde")]
	on_change: Option<std::time::Duration>,

	/// Project Root
	root: Option<PathBuf>,
	/// Project Main File
	main: Option<PathBuf>,
}

impl Default for InitOptions {
	fn default() -> Self {
		Self {
			dictionary: HashMap::new(),
			disabled_checks: HashMap::new(),
			languages: Vec::new(),

			bundled: false,
			jar_location: None,
			host: None,
			port: None,

			chunk_size: 1000,
			on_change: None,

			root: None,
			main: None,
		}
	}
}

fn create_language_map(codes: Vec<String>) -> HashMap<String, String> {
	codes
		.into_iter()
		.map(|lang| (lang.split('-').next().unwrap_or("").to_owned(), lang))
		.collect()
}

impl InitOptions {
	async fn create_lt(&self) -> anyhow::Result<LanguageTool> {
		let mut lt = LanguageTool::new(
			self.bundled,
			self.jar_location.as_ref(),
			self.host.as_ref(),
			self.port.as_ref(),
		)?;
		for (lang, dict) in &self.dictionary {
			lt.allow_words(lang.clone(), dict).await?;
		}
		for (lang, checks) in &self.disabled_checks {
			lt.disable_checks(lang.clone(), checks).await?;
		}
		Ok(lt)
	}

	fn make_absolute(&mut self) {
		fn make_absolute(cwd: &Path, path: &mut Option<PathBuf>) {
			if let Some(path) = path {
				if path.is_absolute() {
					return;
				}
				*path = cwd.join(&path)
			}
		}
		let cwd = std::env::current_dir().unwrap();
		make_absolute(&cwd, &mut self.main);
		make_absolute(&cwd, &mut self.root);
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	eprintln!("Starting LSP server");

	let (connection, io_threads) = Connection::stdio();

	let capabilities = ServerCapabilities {
		text_document_sync: Some(TextDocumentSyncCapability::Options(
			TextDocumentSyncOptions {
				open_close: Some(true),
				save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
					include_text: Some(false),
				})),
				change: Some(TextDocumentSyncKind::INCREMENTAL),
				..Default::default()
			},
		)),

		code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
		..Default::default()
	};

	let server_capabilities = serde_json::to_value(capabilities).unwrap();
	let initialization_params = match connection.initialize(server_capabilities) {
		Ok(it) => it,
		Err(e) => {
			if e.channel_is_disconnected() {
				io_threads.join()?;
			}
			return Err(e.into());
		},
	};
	let state = State::new(connection, initialization_params).await?;
	state.main_loop().await?;
	io_threads.join()?;

	eprintln!("Shutting down server");
	Ok(())
}

struct Options {
	chunk_size: usize,
	on_change: Option<std::time::Duration>,
	language_codes: HashMap<String, String>,
}

struct State {
	world: LtWorld,
	cache: Cache,
	lt: LanguageTool,
	connection: Connection,
	check: Option<CheckData>,
	options: Options,
}

struct CheckData {
	check_time: std::time::Instant,
	url: Url,
	path: PathBuf,
}

enum Action {
	Message(Message),
	Check(CheckData),
}

impl State {
	pub async fn new(connection: Connection, params: Value) -> anyhow::Result<Self> {
		let params = serde_json::from_value::<InitializeParams>(params)?;
		let options = params.initialization_options.context("No init options")?;

		let mut options = serde_ignored::deserialize::<_, _, InitOptions>(options, |path| {
			eprintln!("Unknown option: {}", path);
		})?;

		let cache = Cache::new();

		options.make_absolute();
		eprintln!("Options: {:#?}", options);
		let lt = options.create_lt().await?;
		let Some(main) = &options.main else {
			return Err(anyhow::anyhow!("main file is required")).unwrap();
		};

		let world = lt_world::LtWorld::new(main.clone(), options.root);

		eprintln!("Compiling document");
		match world.compile() {
			Ok(_) => {},
			Err(err) => {
				eprintln!("Failed to compile document");
				for dia in err {
					eprintln!("\t{:?}", dia);
				}
			},
		}

		Ok(Self {
			world,
			cache,
			lt,
			connection,
			check: None,

			options: Options {
				on_change: options.on_change,
				chunk_size: options.chunk_size,
				language_codes: create_language_map(options.languages),
			},
		})
	}

	pub async fn main_loop(mut self) -> anyhow::Result<()> {
		eprintln!("Waiting for events");
		loop {
			match self.next_action()? {
				Action::Message(msg) => self.message(msg).await?,
				Action::Check(data) => self.check_change(&data.path, data.url).await?,
			}
		}
	}

	fn next_action(&mut self) -> anyhow::Result<Action> {
		if let Some(last_change) = &self.check {
			let msg = self
				.connection
				.receiver
				.recv_deadline(last_change.check_time);
			match msg {
				Ok(msg) => Ok(Action::Message(msg)),
				Err(RecvTimeoutError::Timeout) => Ok(Action::Check(self.check.take().unwrap())),
				Err(err) => Err(err.into()),
			}
		} else {
			let msg = self.connection.receiver.recv()?;
			Ok(Action::Message(msg))
		}
	}

	pub async fn message(&mut self, msg: Message) -> anyhow::Result<()> {
		match msg {
			Message::Request(req) => {
				if self.connection.handle_shutdown(&req)? {
					return Ok(());
				}
				self.request(req).await
			},
			Message::Response(resp) => {
				eprintln!("Unknown response: {:?}", resp);
				Ok(())
			},
			Message::Notification(not) => self.notification(not).await,
		}
	}

	pub async fn request(&mut self, req: Request) -> anyhow::Result<()> {
		let req = match cast_request::<CodeActionRequest>(req) {
			Ok((id, params)) => {
				let action = self.code_action(params).await?;
				send_response::<CodeActionRequest>(&self.connection, id, action)?;
				return Ok(());
			},
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(req)) => req,
		};
		eprintln!("Unknown request: {:?}", req);
		Ok(())
	}

	async fn code_action(
		&self,
		params: CodeActionParams,
	) -> anyhow::Result<Option<CodeActionResponse>> {
		let mut action = CodeActionResponse::new();

		let Some(diagnostic) = params.context.diagnostics.last() else {
			return Ok(None);
		};
		let Some(data) = &diagnostic.data else {
			return Ok(None);
		};

		let replacements = match serde_json::from_value::<Vec<String>>(data.clone()) {
			Ok(r) => r,
			Err(err) => {
				eprintln!("{}", err);
				return Ok(None);
			},
		};

		for (i, value) in replacements.into_iter().enumerate() {
			let title = format!("Replace with \"{}\"", value);
			let replace = TextEdit { range: diagnostic.range, new_text: value };
			let edit = [(params.text_document.uri.clone(), vec![replace])]
				.into_iter()
				.collect();

			action.push(
				CodeAction {
					title,
					is_preferred: Some(i == 0),
					kind: Some(CodeActionKind::QUICKFIX),
					diagnostics: Some(params.context.diagnostics.clone()),
					edit: Some(WorkspaceEdit {
						changes: Some(edit),
						..Default::default()
					}),
					command: None,
					disabled: None,
					data: None,
				}
				.into(),
			);
		}
		Ok(Some(action))
	}

	pub async fn notification(&mut self, not: Notification) -> anyhow::Result<()> {
		let not = match cast_notification::<DidChangeTextDocument>(not) {
			Ok(params) => return self.file_change(params).await,
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<DidSaveTextDocument>(not) {
			Ok(params) => return self.file_save(params).await,
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<DidOpenTextDocument>(not) {
			Ok(params) => return self.file_open(params).await,
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<DidCloseTextDocument>(not) {
			Ok(params) => return self.file_close(params).await,
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<DidChangeConfiguration>(not) {
			Ok(params) => return self.config_change(params).await,
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<Cancel>(not) {
			Ok(_params) => return Ok(()),
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		let not = match cast_notification::<SetTrace>(not) {
			Ok(_params) => return Ok(()),
			Err(err @ ExtractError::JsonError { .. }) => return Err(err.into()),
			Err(ExtractError::MethodMismatch(not)) => not,
		};
		eprintln!("Unknown notification: {:?}", not);
		Ok(())
	}

	async fn file_save(&mut self, params: DidSaveTextDocumentParams) -> anyhow::Result<()> {
		let path = params.text_document.uri.to_file_path().unwrap();
		eprintln!("Save {}", path.display());
		self.check = Some(CheckData {
			check_time: std::time::Instant::now(),
			url: params.text_document.uri,
			path,
		});
		Ok(())
	}

	async fn file_open(&mut self, params: DidOpenTextDocumentParams) -> anyhow::Result<()> {
		let path = params.text_document.uri.to_file_path().unwrap();
		eprintln!("Open {}", path.display());
		self.world.use_shadow_file(&path, params.text_document.text);
		self.check = Some(CheckData {
			check_time: std::time::Instant::now(),
			url: params.text_document.uri,
			path,
		});
		Ok(())
	}

	async fn file_close(&mut self, params: DidCloseTextDocumentParams) -> anyhow::Result<()> {
		let path = &params.text_document.uri.to_file_path().unwrap();
		eprintln!("Close {}", path.display());
		self.world.use_original_file(&path);
		Ok(())
	}

	async fn file_change(&mut self, params: DidChangeTextDocumentParams) -> anyhow::Result<()> {
		let path = params.text_document.uri.to_file_path().unwrap();
		eprintln!("Change {}", path.display());
		let source = self.world.shadow_file(&path).unwrap();

		for change in &params.content_changes {
			if let Some(range) = change.range {
				let start = source
					.line_column_to_byte(range.start.line as usize, range.start.character as usize)
					.unwrap();
				let end = source
					.line_column_to_byte(range.end.line as usize, range.end.character as usize)
					.unwrap();
				source.edit(start..end, &change.text);
			} else {
				source.replace(&change.text);
			}
		}

		let Some(duration) = self.options.on_change else {
			return Ok(());
		};
		self.check = Some(CheckData {
			check_time: std::time::Instant::now() + duration,
			url: params.text_document.uri,
			path,
		});
		Ok(())
	}

	async fn check_change(&mut self, path: &Path, url: Url) -> anyhow::Result<()> {
		eprintln!("Checking: {}", path.display());

		let diagnostics = match self.get_diagnostics(path).await {
			Ok(d) => d,
			Err(err) => {
				eprintln!("{:?}", err);
				return Ok(());
			},
		};
		let l = diagnostics.len();
		let params = PublishDiagnosticsParams { uri: url, version: None, diagnostics };
		send_notification::<PublishDiagnostics>(&self.connection, params)?;
		eprintln!("{} Diagnostics send", l);
		Ok(())
	}

	async fn config_change(&mut self, params: DidChangeConfigurationParams) -> anyhow::Result<()> {
		let mut options =
			match serde_ignored::deserialize::<_, _, InitOptions>(params.settings, |path| {
				eprintln!("Unknown option {}", path);
			}) {
				Ok(o) => o,
				Err(err) => {
					eprintln!("{}", err);
					return Ok(());
				},
			};

		options.make_absolute();
		eprintln!("Options: {:#?}", options);

		self.lt = match options.create_lt().await {
			Ok(lt) => lt,
			Err(err) => {
				eprintln!("{}", err);
				return Ok(());
			},
		};

		if let Some(main) = options.main {
			self.world.update(main, options.root);
		}

		self.options = Options {
			on_change: options.on_change,
			chunk_size: options.chunk_size,
			language_codes: create_language_map(options.languages),
		};

		Ok(())
	}

	async fn get_diagnostics(&mut self, path: &Path) -> anyhow::Result<Vec<Diagnostic>> {
		let doc = match self.world.compile() {
			Ok(doc) => doc,
			Err(err) => {
				eprintln!("Failed to compile document");
				for dia in err {
					eprintln!("\t{:?}", dia);
				}
				return Ok(Vec::new());
			},
		};

		let file_id = self.world.file_id(path);
		let paragraphs =
			typst_languagetool::convert::document(&doc, self.options.chunk_size, file_id);
		let mut collector = typst_languagetool::FileCollector::new(file_id, &self.world);
		let mut next_cache = Cache::new();
		let l = paragraphs.len();
		eprintln!("Checking {} paragraphs", l);
		for (idx, (text, mapping)) in paragraphs.into_iter().enumerate() {
			let lang = self
				.options
				.language_codes
				.get(mapping.short_language())
				.map(|x| x.clone())
				.unwrap_or(mapping.long_language());
			let suggestions = if let Some(suggestions) = self.cache.get(&text) {
				suggestions
			} else {
				eprintln!("Checking {}/{}", idx + 1, l);
				self.lt.check_text(lang, &text).await?
			};
			collector.add(&suggestions, mapping);
			next_cache.insert(text, suggestions);
		}
		self.cache = next_cache;
		eprintln!("Generating diagnostics");

		let (source, diagnostics) = collector.finish();

		let diagnostics = diagnostics
			.into_iter()
			.map(|diagnostic| {
				let (start_line, start_column) =
					byte_to_position(&source, diagnostic.locations[0].start);
				let (end_line, end_column) = byte_to_position(&source, diagnostic.locations[0].end);

				Diagnostic {
					range: Range {
						start: lsp_types::Position {
							line: start_line as u32,
							character: start_column as u32,
						},
						end: lsp_types::Position {
							line: end_line as u32,
							character: end_column as u32,
						},
					},
					severity: Some(DiagnosticSeverity::INFORMATION),
					code: Some(NumberOrString::String(diagnostic.rule_id)),
					code_description: None,
					source: None,
					message: diagnostic.message,
					related_information: None,
					tags: None,
					data: serde_json::to_value(diagnostic.replacements).ok(),
				}
			})
			.collect();

		Ok(diagnostics)
	}
}

fn cast_request<R>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
	R: lsp_types::request::Request,
	R::Params: serde::de::DeserializeOwned,
{
	req.extract(R::METHOD)
}

fn cast_notification<N>(not: Notification) -> Result<N::Params, ExtractError<Notification>>
where
	N: lsp_types::notification::Notification,
	N::Params: serde::de::DeserializeOwned,
{
	not.extract(N::METHOD)
}

#[allow(dead_code)]
fn send_request<R>(connection: &Connection, id: i32, params: R::Params) -> anyhow::Result<()>
where
	R: lsp_types::request::Request,
{
	let message = Message::Request(Request::new(id.into(), R::METHOD.into(), params));
	connection.sender.send(message)?;

	Ok(())
}

fn send_response<R>(connection: &Connection, id: RequestId, result: R::Result) -> anyhow::Result<()>
where
	R: lsp_types::request::Request,
{
	let message = Message::Response(Response::new_ok(id, result));
	connection.sender.send(message)?;
	Ok(())
}

fn send_notification<N>(connection: &Connection, params: N::Params) -> anyhow::Result<()>
where
	N: lsp_types::notification::Notification,
{
	let message = Message::Notification(Notification::new(N::METHOD.into(), params));
	connection.sender.send(message)?;
	Ok(())
}

#[derive(Debug)]
struct Cache {
	cache: HashMap<String, Vec<Suggestion>>,
}

impl Cache {
	pub fn new() -> Self {
		Self { cache: HashMap::new() }
	}

	pub fn get(&mut self, text: &str) -> Option<Vec<Suggestion>> {
		self.cache.remove(text)
	}

	pub fn insert(&mut self, text: String, suggestions: Vec<Suggestion>) {
		self.cache.insert(text, suggestions);
	}
}

fn byte_to_position(source: &Source, index: usize) -> (usize, usize) {
	let line = source.byte_to_line(index).unwrap();
	let start = source.line_to_byte(line).unwrap();
	let head = source.get(start..index).unwrap();
	let column = head.chars().count();
	(line, column)
}
