use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use assistant_slash_command::{SlashCommand, SlashCommandOutput, SlashCommandOutputSection};
use fs::Fs;
use futures::AsyncReadExt;
use gpui::{AppContext, Model, Task, WeakView};
use http::{AsyncBody, HttpClient, HttpClientWithUrl};
use language::LspAdapterDelegate;
use project::{Project, ProjectPath};
use rustdoc::LocalProvider;
use rustdoc::{convert_rustdoc_to_markdown, RustdocStore};
use ui::{prelude::*, ButtonLike, ElevationIndex};
use util::{maybe, ResultExt};
use workspace::Workspace;

#[derive(Debug, Clone, Copy)]
enum RustdocSource {
    /// The docs were sourced from local `cargo doc` output.
    Local,
    /// The docs were sourced from `docs.rs`.
    DocsDotRs,
}

pub(crate) struct RustdocSlashCommand;

impl RustdocSlashCommand {
    async fn build_message(
        fs: Arc<dyn Fs>,
        http_client: Arc<HttpClientWithUrl>,
        crate_name: String,
        module_path: Vec<String>,
        path_to_cargo_toml: Option<&Path>,
    ) -> Result<(RustdocSource, String)> {
        let cargo_workspace_root = path_to_cargo_toml.and_then(|path| path.parent());
        if let Some(cargo_workspace_root) = cargo_workspace_root {
            let mut local_cargo_doc_path = cargo_workspace_root.join("target/doc");
            local_cargo_doc_path.push(&crate_name);
            if !module_path.is_empty() {
                local_cargo_doc_path.push(module_path.join("/"));
            }
            local_cargo_doc_path.push("index.html");

            if let Ok(contents) = fs.load(&local_cargo_doc_path).await {
                let (markdown, _items) = convert_rustdoc_to_markdown(contents.as_bytes())?;

                return Ok((RustdocSource::Local, markdown));
            }
        }

        let version = "latest";
        let path = format!(
            "{crate_name}/{version}/{crate_name}/{module_path}",
            module_path = module_path.join("/")
        );

        let mut response = http_client
            .get(
                &format!("https://docs.rs/{path}"),
                AsyncBody::default(),
                true,
            )
            .await?;

        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .context("error reading docs.rs response body")?;

        if response.status().is_client_error() {
            let text = String::from_utf8_lossy(body.as_slice());
            bail!(
                "status error {}, response: {text:?}",
                response.status().as_u16()
            );
        }

        let (markdown, _items) = convert_rustdoc_to_markdown(&body[..])?;

        Ok((RustdocSource::DocsDotRs, markdown))
    }

    fn path_to_cargo_toml(project: Model<Project>, cx: &mut AppContext) -> Option<Arc<Path>> {
        let worktree = project.read(cx).worktrees().next()?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_path("Cargo.toml")?;
        let path = ProjectPath {
            worktree_id: worktree.id(),
            path: entry.path.clone(),
        };
        Some(Arc::from(
            project.read(cx).absolute_path(&path, cx)?.as_path(),
        ))
    }
}

impl SlashCommand for RustdocSlashCommand {
    fn name(&self) -> String {
        "rustdoc".into()
    }

    fn description(&self) -> String {
        "insert Rust docs".into()
    }

    fn menu_text(&self) -> String {
        "Insert Rust Documentation".into()
    }

    fn requires_argument(&self) -> bool {
        true
    }

    fn complete_argument(
        &self,
        query: String,
        _cancel: Arc<AtomicBool>,
        workspace: Option<WeakView<Workspace>>,
        cx: &mut AppContext,
    ) -> Task<Result<Vec<String>>> {
        let index_provider_deps = maybe!({
            let workspace = workspace.ok_or_else(|| anyhow!("no workspace"))?;
            let workspace = workspace
                .upgrade()
                .ok_or_else(|| anyhow!("workspace was dropped"))?;
            let project = workspace.read(cx).project().clone();
            let fs = project.read(cx).fs().clone();
            let cargo_workspace_root = Self::path_to_cargo_toml(project, cx)
                .and_then(|path| path.parent().map(|path| path.to_path_buf()))
                .ok_or_else(|| anyhow!("no Cargo workspace root found"))?;

            anyhow::Ok((fs, cargo_workspace_root))
        });

        let store = RustdocStore::global(cx);
        cx.background_executor().spawn(async move {
            if let Some((crate_name, rest)) = query.split_once(':') {
                if rest.is_empty() {
                    if let Some((fs, cargo_workspace_root)) = index_provider_deps.log_err() {
                        let provider = Box::new(LocalProvider::new(fs, cargo_workspace_root));
                        // We don't need to hold onto this task, as the `RustdocStore` will hold it
                        // until it completes.
                        let _ = store.clone().index(crate_name.to_string(), provider);
                    }
                }
            }

            let items = store.search(query).await;
            Ok(items)
        })
    }

    fn run(
        self: Arc<Self>,
        argument: Option<&str>,
        workspace: WeakView<Workspace>,
        _delegate: Arc<dyn LspAdapterDelegate>,
        cx: &mut WindowContext,
    ) -> Task<Result<SlashCommandOutput>> {
        let Some(argument) = argument else {
            return Task::ready(Err(anyhow!("missing crate name")));
        };
        let Some(workspace) = workspace.upgrade() else {
            return Task::ready(Err(anyhow!("workspace was dropped")));
        };

        let project = workspace.read(cx).project().clone();
        let fs = project.read(cx).fs().clone();
        let http_client = workspace.read(cx).client().http_client();
        let path_to_cargo_toml = Self::path_to_cargo_toml(project, cx);

        let mut path_components = argument.split("::");
        let crate_name = match path_components
            .next()
            .ok_or_else(|| anyhow!("missing crate name"))
        {
            Ok(crate_name) => crate_name.to_string(),
            Err(err) => return Task::ready(Err(err)),
        };
        let item_path = path_components.map(ToString::to_string).collect::<Vec<_>>();

        let text = cx.background_executor().spawn({
            let rustdoc_store = RustdocStore::global(cx);
            let crate_name = crate_name.clone();
            let item_path = item_path.clone();
            async move {
                let item_docs = rustdoc_store
                    .load(crate_name.clone(), Some(item_path.join("::")))
                    .await;

                if let Ok(item_docs) = item_docs {
                    anyhow::Ok((RustdocSource::Local, item_docs.docs().to_owned()))
                } else {
                    Self::build_message(
                        fs,
                        http_client,
                        crate_name,
                        item_path,
                        path_to_cargo_toml.as_deref(),
                    )
                    .await
                }
            }
        });

        let crate_name = SharedString::from(crate_name);
        let module_path = if item_path.is_empty() {
            None
        } else {
            Some(SharedString::from(item_path.join("::")))
        };
        cx.foreground_executor().spawn(async move {
            let (source, text) = text.await?;
            let range = 0..text.len();
            Ok(SlashCommandOutput {
                text,
                sections: vec![SlashCommandOutputSection {
                    range,
                    render_placeholder: Arc::new(move |id, unfold, _cx| {
                        RustdocPlaceholder {
                            id,
                            unfold,
                            source,
                            crate_name: crate_name.clone(),
                            module_path: module_path.clone(),
                        }
                        .into_any_element()
                    }),
                }],
                run_commands_in_text: false,
            })
        })
    }
}

#[derive(IntoElement)]
struct RustdocPlaceholder {
    pub id: ElementId,
    pub unfold: Arc<dyn Fn(&mut WindowContext)>,
    pub source: RustdocSource,
    pub crate_name: SharedString,
    pub module_path: Option<SharedString>,
}

impl RenderOnce for RustdocPlaceholder {
    fn render(self, _cx: &mut WindowContext) -> impl IntoElement {
        let unfold = self.unfold;

        let crate_path = self
            .module_path
            .map(|module_path| format!("{crate_name}::{module_path}", crate_name = self.crate_name))
            .unwrap_or(self.crate_name.to_string());

        ButtonLike::new(self.id)
            .style(ButtonStyle::Filled)
            .layer(ElevationIndex::ElevatedSurface)
            .child(Icon::new(IconName::FileRust))
            .child(Label::new(format!(
                "rustdoc ({source}): {crate_path}",
                source = match self.source {
                    RustdocSource::Local => "local",
                    RustdocSource::DocsDotRs => "docs.rs",
                }
            )))
            .on_click(move |_, cx| unfold(cx))
    }
}
