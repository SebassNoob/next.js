use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use next_core::{
    all_assets_from_entries, create_page_loader_entry_module, get_asset_path_from_pathname,
    get_edge_resolve_options_context,
    mode::NextMode,
    next_client::{
        get_client_module_options_context, get_client_resolve_options_context,
        get_client_runtime_entries, ClientContextType, RuntimeEntries,
    },
    next_dynamic::NextDynamicTransition,
    next_edge::route_regex::get_named_middleware_regex,
    next_manifests::{
        BuildManifest, EdgeFunctionDefinition, LoadableManifest, MiddlewareMatcher,
        MiddlewaresManifestV2, PagesManifest,
    },
    next_pages::create_page_ssr_entry_module,
    next_server::{
        get_server_module_options_context, get_server_resolve_options_context,
        get_server_runtime_entries, ServerContextType,
    },
    pages_structure::{
        find_pages_structure, PagesDirectoryStructure, PagesStructure, PagesStructureItem,
    },
    util::{get_asset_prefix_from_pathname, parse_config_from_source, NextRuntime},
    PageLoaderAsset,
};
use serde::{Deserialize, Serialize};
use tracing::Instrument;
use turbo_tasks::{
    trace::TraceRawVcs, Completion, TaskInput, TryFlatJoinIterExt, TryJoinIterExt, Value, Vc,
};
use turbopack_binding::{
    turbo::tasks_fs::{
        File, FileContent, FileSystem, FileSystemPath, FileSystemPathOption, VirtualFileSystem,
    },
    turbopack::{
        build::{BuildChunkingContext, EntryChunkGroupResult},
        core::{
            asset::AssetContent,
            chunk::{
                availability_info::AvailabilityInfo, ChunkGroupResult, ChunkingContext,
                EvaluatableAssets,
            },
            context::AssetContext,
            file_source::FileSource,
            issue::IssueSeverity,
            module::Module,
            output::{OutputAsset, OutputAssets},
            reference_type::{
                EcmaScriptModulesReferenceSubType, EntryReferenceSubType, ReferenceType,
            },
            resolve::{origin::PlainResolveOrigin, parse::Request, pattern::Pattern},
            source::Source,
            virtual_output::VirtualOutputAsset,
        },
        ecmascript::{
            chunk::{EcmascriptChunkPlaceable, EcmascriptChunkingContext},
            resolve::esm_resolve,
            EcmascriptModuleAsset,
        },
        turbopack::{
            module_options::ModuleOptionsContext,
            resolve_options_context::ResolveOptionsContext,
            transition::{ContextTransition, TransitionsByName},
            ModuleAssetContext,
        },
    },
};

use crate::{
    dynamic_imports::{
        collect_chunk_group, collect_evaluated_chunk_group, collect_next_dynamic_imports,
        DynamicImportedChunks,
    },
    font::create_font_manifest,
    middleware::{get_js_paths_from_root, get_wasm_paths_from_root, wasm_paths_to_bindings},
    project::Project,
    route::{Endpoint, Route, Routes, WrittenEndpoint},
    server_paths::all_server_paths,
};

#[turbo_tasks::value]
pub struct PagesProject {
    project: Vc<Project>,
    mode: NextMode,
}

#[turbo_tasks::value_impl]
impl PagesProject {
    #[turbo_tasks::function]
    pub async fn new(project: Vc<Project>, mode: NextMode) -> Result<Vc<Self>> {
        Ok(PagesProject { project, mode }.cell())
    }

    #[turbo_tasks::function]
    pub async fn routes(self: Vc<Self>) -> Result<Vc<Routes>> {
        let pages_structure = self.pages_structure();
        let PagesStructure {
            api,
            pages,
            app: _,
            document: _,
            error: _,
        } = &*pages_structure.await?;
        let mut routes = IndexMap::new();

        async fn add_page_to_routes(
            routes: &mut IndexMap<String, Route>,
            page: Vc<PagesStructureItem>,
            make_route: impl Fn(Vc<String>, Vc<String>, Vc<FileSystemPath>) -> Route,
        ) -> Result<()> {
            let PagesStructureItem {
                next_router_path,
                project_path,
                original_path,
            } = *page.await?;
            let pathname = format!("/{}", next_router_path.await?.path);
            let pathname_vc = Vc::cell(pathname.clone());
            let original_name = Vc::cell(format!("/{}", original_path.await?.path));
            let route = make_route(pathname_vc, original_name, project_path);
            routes.insert(pathname, route);
            Ok(())
        }

        async fn add_dir_to_routes(
            routes: &mut IndexMap<String, Route>,
            dir: Vc<PagesDirectoryStructure>,
            make_route: impl Fn(Vc<String>, Vc<String>, Vc<FileSystemPath>) -> Route,
        ) -> Result<()> {
            let mut queue = vec![dir];
            while let Some(dir) = queue.pop() {
                let PagesDirectoryStructure {
                    ref items,
                    ref children,
                    next_router_path: _,
                    project_path: _,
                } = *dir.await?;
                for &item in items.iter() {
                    add_page_to_routes(routes, item, &make_route).await?;
                }
                for &child in children.iter() {
                    queue.push(child);
                }
            }
            Ok(())
        }

        if let Some(api) = api {
            add_dir_to_routes(&mut routes, *api, |pathname, original_name, path| {
                Route::PageApi {
                    endpoint: Vc::upcast(PageEndpoint::new(
                        PageEndpointType::Api,
                        self,
                        pathname,
                        original_name,
                        path,
                        pages_structure,
                    )),
                }
            })
            .await?;
        }

        let app_endpoint = self.app_endpoint_internal().resolve().await?;

        let make_page_route = |pathname, original_name, path| Route::Page {
            html_endpoint: Vc::upcast(PageEndpoint::new_with_depend_on(
                PageEndpointType::Html,
                self,
                pathname,
                original_name,
                path,
                pages_structure,
                app_endpoint,
            )),
            data_endpoint: Vc::upcast(PageEndpoint::new_with_depend_on(
                PageEndpointType::Data,
                self,
                pathname,
                original_name,
                path,
                pages_structure,
                app_endpoint,
            )),
        };

        if let Some(pages) = pages {
            add_dir_to_routes(&mut routes, *pages, make_page_route).await?;
        }

        Ok(Vc::cell(routes))
    }

    #[turbo_tasks::function]
    async fn to_endpoint(
        self: Vc<Self>,
        item: Vc<PagesStructureItem>,
        ty: PageEndpointType,
    ) -> Result<Vc<PageEndpoint>> {
        let PagesStructureItem {
            next_router_path,
            project_path,
            original_path,
        } = *item.await?;
        let pathname = format!("/{}", next_router_path.await?.path);
        let pathname_vc = Vc::cell(pathname.clone());
        let original_name = Vc::cell(format!("/{}", original_path.await?.path));
        let path = project_path;
        Ok(PageEndpoint::new(
            ty,
            self,
            pathname_vc,
            original_name,
            path,
            self.pages_structure(),
        ))
    }

    #[turbo_tasks::function]
    pub async fn document_endpoint(self: Vc<Self>) -> Result<Vc<Box<dyn Endpoint>>> {
        Ok(Vc::upcast(self.to_endpoint(
            self.pages_structure().await?.document,
            PageEndpointType::SsrOnly,
        )))
    }

    #[turbo_tasks::function]
    async fn app_endpoint_internal(self: Vc<Self>) -> Result<Vc<PageEndpoint>> {
        Ok(self.to_endpoint(self.pages_structure().await?.app, PageEndpointType::Html))
    }

    #[turbo_tasks::function]
    pub async fn app_endpoint(self: Vc<Self>) -> Result<Vc<Box<dyn Endpoint>>> {
        Ok(Vc::upcast(self.to_endpoint(
            self.pages_structure().await?.app,
            PageEndpointType::Html,
        )))
    }

    #[turbo_tasks::function]
    pub async fn error_endpoint(self: Vc<Self>) -> Result<Vc<Box<dyn Endpoint>>> {
        Ok(Vc::upcast(self.to_endpoint(
            self.pages_structure().await?.error,
            PageEndpointType::Html,
        )))
    }

    #[turbo_tasks::function]
    fn project(&self) -> Vc<Project> {
        self.project
    }

    #[turbo_tasks::function]
    fn pages_structure(&self) -> Vc<PagesStructure> {
        let next_router_fs = Vc::upcast::<Box<dyn FileSystem>>(VirtualFileSystem::new());
        let next_router_root = next_router_fs.root();
        find_pages_structure(
            self.project.project_path(),
            next_router_root,
            self.project.next_config().page_extensions(),
        )
    }

    #[turbo_tasks::function]
    async fn pages_dir(self: Vc<Self>) -> Result<Vc<FileSystemPath>> {
        Ok(if let Some(pages) = self.pages_structure().await?.pages {
            pages.project_path()
        } else {
            self.project().project_path().join("pages".to_string())
        })
    }

    #[turbo_tasks::function]
    fn transitions(self: Vc<Self>) -> Vc<TransitionsByName> {
        Vc::cell(
            [(
                "next-dynamic".to_string(),
                Vc::upcast(NextDynamicTransition::new(self.client_transition())),
            )]
            .into_iter()
            .collect(),
        )
    }

    #[turbo_tasks::function]
    fn client_transition(self: Vc<Self>) -> Vc<ContextTransition> {
        ContextTransition::new(
            self.project().client_compile_time_info(),
            self.client_module_options_context(),
            self.client_resolve_options_context(),
            Vc::cell("client".to_string()),
        )
    }

    #[turbo_tasks::function]
    async fn client_module_options_context(self: Vc<Self>) -> Result<Vc<ModuleOptionsContext>> {
        let this = self.await?;
        Ok(get_client_module_options_context(
            self.project().project_path(),
            self.project().execution_context(),
            self.project().client_compile_time_info().environment(),
            Value::new(ClientContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
        ))
    }

    #[turbo_tasks::function]
    async fn client_resolve_options_context(self: Vc<Self>) -> Result<Vc<ResolveOptionsContext>> {
        let this = self.await?;
        Ok(get_client_resolve_options_context(
            self.project().project_path(),
            Value::new(ClientContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
            self.project().execution_context(),
        ))
    }

    #[turbo_tasks::function]
    pub(super) fn client_module_context(self: Vc<Self>) -> Vc<Box<dyn AssetContext>> {
        Vc::upcast(ModuleAssetContext::new(
            self.transitions(),
            self.project().client_compile_time_info(),
            self.client_module_options_context(),
            self.client_resolve_options_context(),
            Vc::cell("client".to_string()),
        ))
    }

    #[turbo_tasks::function]
    pub(super) fn ssr_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            self.transitions(),
            self.project().server_compile_time_info(),
            self.ssr_module_options_context(),
            self.ssr_resolve_options_context(),
            Vc::cell("ssr".to_string()),
        )
    }

    /// Returns a context specific to pages/api.
    /// This mimics the current configuration in next-dev.
    /// (https://github.com/vercel/next.js/blob/9b4b0847ed4a1025e73bec16a9ee11766e632e14/packages/next/src/build/webpack-config.ts#L1381-L1385)
    #[turbo_tasks::function]
    pub(super) fn api_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            self.transitions(),
            self.project().server_compile_time_info(),
            self.api_module_options_context(),
            self.ssr_resolve_options_context(),
            Vc::cell("api".to_string()),
        )
    }

    #[turbo_tasks::function]
    pub(super) fn ssr_data_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            self.transitions(),
            self.project().server_compile_time_info(),
            self.ssr_data_module_options_context(),
            self.ssr_resolve_options_context(),
            Vc::cell("ssr-data".to_string()),
        )
    }

    #[turbo_tasks::function]
    pub(super) fn edge_ssr_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            Default::default(),
            self.project().edge_compile_time_info(),
            self.ssr_module_options_context(),
            self.edge_ssr_resolve_options_context(),
            Vc::cell("edge-ssr".to_string()),
        )
    }

    #[turbo_tasks::function]
    pub(super) fn edge_api_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            Default::default(),
            self.project().edge_compile_time_info(),
            self.api_module_options_context(),
            self.edge_ssr_resolve_options_context(),
            Vc::cell("edge-api".to_string()),
        )
    }

    #[turbo_tasks::function]
    pub(super) fn edge_ssr_data_module_context(self: Vc<Self>) -> Vc<ModuleAssetContext> {
        ModuleAssetContext::new(
            Default::default(),
            self.project().edge_compile_time_info(),
            self.ssr_data_module_options_context(),
            self.edge_ssr_resolve_options_context(),
            Vc::cell("edge-ssr-data".to_string()),
        )
    }

    #[turbo_tasks::function]
    async fn ssr_module_options_context(self: Vc<Self>) -> Result<Vc<ModuleOptionsContext>> {
        let this = self.await?;
        Ok(get_server_module_options_context(
            self.project().project_path(),
            self.project().execution_context(),
            Value::new(ServerContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
        ))
    }

    #[turbo_tasks::function]
    async fn api_module_options_context(self: Vc<Self>) -> Result<Vc<ModuleOptionsContext>> {
        let this = self.await?;
        Ok(get_server_module_options_context(
            self.project().project_path(),
            self.project().execution_context(),
            Value::new(ServerContextType::PagesApi {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
        ))
    }

    #[turbo_tasks::function]
    async fn ssr_data_module_options_context(self: Vc<Self>) -> Result<Vc<ModuleOptionsContext>> {
        let this = self.await?;
        Ok(get_server_module_options_context(
            self.project().project_path(),
            self.project().execution_context(),
            Value::new(ServerContextType::PagesData {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
        ))
    }

    #[turbo_tasks::function]
    async fn ssr_resolve_options_context(self: Vc<Self>) -> Result<Vc<ResolveOptionsContext>> {
        let this = self.await?;
        Ok(get_server_resolve_options_context(
            self.project().project_path(),
            // NOTE(alexkirsz) This could be `PagesData` for the data endpoint, but it doesn't
            // matter (for now at least) because `get_server_resolve_options_context` doesn't
            // differentiate between the two.
            Value::new(ServerContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
            self.project().execution_context(),
        ))
    }

    #[turbo_tasks::function]
    async fn edge_ssr_resolve_options_context(self: Vc<Self>) -> Result<Vc<ResolveOptionsContext>> {
        let this = self.await?;
        Ok(get_edge_resolve_options_context(
            self.project().project_path(),
            // NOTE(alexkirsz) This could be `PagesData` for the data endpoint, but it doesn't
            // matter (for now at least) because `get_server_resolve_options_context` doesn't
            // differentiate between the two.
            Value::new(ServerContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
            self.project().execution_context(),
        ))
    }

    #[turbo_tasks::function]
    async fn client_runtime_entries(self: Vc<Self>) -> Result<Vc<EvaluatableAssets>> {
        let this = self.await?;
        let client_runtime_entries = get_client_runtime_entries(
            self.project().project_path(),
            Value::new(ClientContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
            self.project().next_config(),
            self.project().execution_context(),
        );
        Ok(client_runtime_entries.resolve_entries(self.client_module_context()))
    }

    #[turbo_tasks::function]
    async fn runtime_entries(self: Vc<Self>) -> Result<Vc<RuntimeEntries>> {
        let this = self.await?;
        Ok(get_server_runtime_entries(
            Value::new(ServerContextType::Pages {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
        ))
    }

    #[turbo_tasks::function]
    async fn data_runtime_entries(self: Vc<Self>) -> Result<Vc<RuntimeEntries>> {
        let this = self.await?;
        Ok(get_server_runtime_entries(
            Value::new(ServerContextType::PagesData {
                pages_dir: self.pages_dir(),
            }),
            this.mode,
        ))
    }

    #[turbo_tasks::function]
    fn ssr_runtime_entries(self: Vc<Self>) -> Vc<EvaluatableAssets> {
        let ssr_runtime_entries = self.runtime_entries();
        ssr_runtime_entries.resolve_entries(Vc::upcast(self.ssr_module_context()))
    }

    #[turbo_tasks::function]
    fn ssr_data_runtime_entries(self: Vc<Self>) -> Vc<EvaluatableAssets> {
        let ssr_data_runtime_entries = self.data_runtime_entries();
        ssr_data_runtime_entries.resolve_entries(Vc::upcast(self.ssr_module_context()))
    }

    #[turbo_tasks::function]
    fn edge_ssr_runtime_entries(self: Vc<Self>) -> Vc<EvaluatableAssets> {
        let ssr_runtime_entries = self.runtime_entries();
        ssr_runtime_entries.resolve_entries(Vc::upcast(self.edge_ssr_module_context()))
    }

    #[turbo_tasks::function]
    fn edge_ssr_data_runtime_entries(self: Vc<Self>) -> Vc<EvaluatableAssets> {
        let ssr_data_runtime_entries = self.data_runtime_entries();
        ssr_data_runtime_entries.resolve_entries(Vc::upcast(self.edge_ssr_module_context()))
    }
}

#[turbo_tasks::value]
struct PageEndpoint {
    ty: PageEndpointType,
    pages_project: Vc<PagesProject>,
    pathname: Vc<String>,
    original_name: Vc<String>,
    path: Vc<FileSystemPath>,
    pages_structure: Vc<PagesStructure>,
    depend_on: Option<Vc<PageEndpoint>>,
}

#[derive(Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Debug, TaskInput, TraceRawVcs)]
enum PageEndpointType {
    Api,
    Html,
    Data,
    SsrOnly,
}

#[turbo_tasks::value_impl]
impl PageEndpoint {
    #[turbo_tasks::function]
    fn new(
        ty: PageEndpointType,
        pages_project: Vc<PagesProject>,
        pathname: Vc<String>,
        original_name: Vc<String>,
        path: Vc<FileSystemPath>,
        pages_structure: Vc<PagesStructure>,
    ) -> Vc<Self> {
        PageEndpoint {
            ty,
            pages_project,
            pathname,
            original_name,
            path,
            pages_structure,
            depend_on: None,
        }
        .cell()
    }

    #[turbo_tasks::function]
    fn new_with_depend_on(
        ty: PageEndpointType,
        pages_project: Vc<PagesProject>,
        pathname: Vc<String>,
        original_name: Vc<String>,
        path: Vc<FileSystemPath>,
        pages_structure: Vc<PagesStructure>,
        depend_on: Vc<PageEndpoint>,
    ) -> Vc<Self> {
        PageEndpoint {
            ty,
            pages_project,
            pathname,
            original_name,
            path,
            pages_structure,
            depend_on: Some(depend_on),
        }
        .cell()
    }

    #[turbo_tasks::function]
    async fn source(self: Vc<Self>) -> Result<Vc<Box<dyn Source>>> {
        let this = self.await?;
        Ok(Vc::upcast(FileSource::new(this.path)))
    }

    #[turbo_tasks::function]
    async fn client_chunks_chunk_group(self: Vc<Self>) -> Result<Vc<ChunkGroupResult>> {
        async move {
            let this = self.await?;

            let client_module_context = this.pages_project.client_module_context();
            let client_module = create_page_loader_entry_module(
                client_module_context,
                self.source(),
                this.pathname,
            );

            let Some(client_module) =
                Vc::try_resolve_downcast_type::<EcmascriptModuleAsset>(client_module).await?
            else {
                bail!("expected an ECMAScript module asset");
            };

            let client_main_module = esm_resolve(
                Vc::upcast(PlainResolveOrigin::new(
                    client_module_context,
                    this.pages_project
                        .project()
                        .project_path()
                        .join("_".to_string()),
                )),
                Request::parse(Value::new(Pattern::Constant(
                    "next/dist/client/next-dev-turbopack.js".to_string(),
                ))),
                Value::new(EcmaScriptModulesReferenceSubType::Undefined),
                IssueSeverity::Error.cell(),
                None,
            )
            .first_module()
            .await?
            .context("expected next/dist/client/next-dev-turbopack.js to resolve to a module")?;

            let Some(client_main_module) =
                Vc::try_resolve_downcast_type::<EcmascriptModuleAsset>(client_main_module).await?
            else {
                bail!("expected an ECMAScript module asset");
            };

            let client_chunking_context = this.pages_project.project().client_chunking_context();

            let (availability_info, base_assets) = if let Some(depend_on) = this.depend_on {
                let chunk_group = depend_on.client_chunks_chunk_group().await?;
                (chunk_group.availability_info, Some(chunk_group.assets))
            } else {
                (AvailabilityInfo::Root, None)
            };

            let ChunkGroupResult {
                assets,
                availability_info,
            } = *client_chunking_context
                .evaluated_chunk_group(
                    client_module.ident(),
                    this.pages_project
                        .client_runtime_entries()
                        .with_entry(Vc::upcast(client_main_module))
                        .with_entry(Vc::upcast(client_module)),
                    Value::new(availability_info),
                )
                .await?;

            let mut new_assets = assets.await?.clone_value();

            new_assets.push(Vc::upcast(PageLoaderAsset::new(
                this.pages_project.project().client_root(),
                this.pathname,
                self.client_relative_path(),
                assets,
            )));

            let new_assets = if let Some(base_assets) = base_assets {
                base_assets
                    .await?
                    .iter()
                    .copied()
                    .chain(new_assets.into_iter())
                    .collect()
            } else {
                new_assets
            };

            Ok(ChunkGroupResult {
                assets: Vc::cell(new_assets),
                availability_info,
            }
            .cell())
        }
        .instrument(tracing::info_span!("page client side rendering"))
        .await
    }

    #[turbo_tasks::function]
    async fn client_chunks(self: Vc<Self>) -> Result<Vc<OutputAssets>> {
        Ok(self.client_chunks_chunk_group().await?.assets)
    }

    #[turbo_tasks::function]
    async fn internal_ssr_chunk(
        self: Vc<Self>,
        reference_type: Value<ReferenceType>,
        node_path: Vc<FileSystemPath>,
        project_root: Vc<FileSystemPath>,
        module_context: Vc<ModuleAssetContext>,
        edge_module_context: Vc<ModuleAssetContext>,
        chunking_context: Vc<BuildChunkingContext>,
        edge_chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
        runtime_entries: Vc<EvaluatableAssets>,
        edge_runtime_entries: Vc<EvaluatableAssets>,
    ) -> Result<Vc<SsrChunk>> {
        async move {
            let this = self.await?;

            let ssr_module = module_context
                .process(self.source(), reference_type.clone())
                .module();

            let config = parse_config_from_source(ssr_module).await?;
            let is_edge = matches!(config.runtime, NextRuntime::Edge);

            if is_edge {
                let availablility_info = if let Some(depend_on) = this.depend_on {
                    match *depend_on.ssr_chunk().await? {
                        SsrChunk::Edge {
                            availability_info, ..
                        } => availability_info,
                        _ => AvailabilityInfo::Root,
                    }
                } else {
                    AvailabilityInfo::Root
                };

                let ssr_module = create_page_ssr_entry_module(
                    this.pathname,
                    reference_type,
                    project_root,
                    Vc::upcast(edge_module_context),
                    self.source(),
                    this.original_name,
                    this.pages_structure,
                    config.runtime,
                    this.pages_project.project().next_config(),
                );

                let mut evaluatable_assets = edge_runtime_entries.await?.clone_value();
                let evaluatable = Vc::try_resolve_sidecast(ssr_module)
                    .await?
                    .context("could not process page loader entry module")?;
                evaluatable_assets.push(evaluatable);

                let chunk_group = edge_chunking_context
                    .evaluated_chunk_group(
                        ssr_module.ident(),
                        Vc::cell(evaluatable_assets.clone()),
                        Value::new(availablility_info),
                    )
                    .await?;

                let edge_files = chunk_group.assets;

                let availability_info = chunk_group.availability_info;
                let dynamic_import_modules = collect_next_dynamic_imports(ssr_module).await?;
                let dynamic_import_entries = collect_evaluated_chunk_group(
                    edge_chunking_context,
                    dynamic_import_modules,
                    Vc::cell(evaluatable_assets.clone()),
                    Value::new(availability_info),
                )
                .await?;

                Ok(SsrChunk::Edge {
                    files: edge_files,
                    dynamic_import_entries,
                    availability_info,
                }
                .cell())
            } else {
                let pathname = &**this.pathname.await?;

                let availablility_info = if let Some(depend_on) = this.depend_on {
                    match *depend_on.ssr_chunk().await? {
                        SsrChunk::NodeJs {
                            availability_info, ..
                        } => availability_info,
                        _ => AvailabilityInfo::Root,
                    }
                } else {
                    AvailabilityInfo::Root
                };

                // `/_app` and `/_document` never get rendered directly so they don't need to be
                // wrapped in the route module.
                let ssr_module = if pathname == "/_app" || pathname == "/_document" {
                    let Some(ssr_module) =
                        Vc::try_resolve_downcast::<Box<dyn EcmascriptChunkPlaceable>>(ssr_module)
                            .await?
                    else {
                        bail!("expected an ECMAScript chunk placeable module");
                    };

                    ssr_module
                } else {
                    create_page_ssr_entry_module(
                        this.pathname,
                        reference_type,
                        project_root,
                        Vc::upcast(module_context),
                        self.source(),
                        this.original_name,
                        this.pages_structure,
                        config.runtime,
                        this.pages_project.project().next_config(),
                    )
                };

                let asset_path = get_asset_path_from_pathname(pathname, ".js");

                let ssr_entry_chunk_path_string = format!("pages{asset_path}");
                let ssr_entry_chunk_path = node_path.join(ssr_entry_chunk_path_string);
                let EntryChunkGroupResult {
                    asset: ssr_entry_chunks,
                    availability_info,
                } = *chunking_context
                    .entry_chunk_group(
                        ssr_entry_chunk_path,
                        ssr_module,
                        runtime_entries,
                        Value::new(availablility_info),
                    )
                    .await?;

                let dynamic_import_modules = collect_next_dynamic_imports(ssr_module).await?;
                let dynamic_import_entries = collect_chunk_group(
                    chunking_context,
                    dynamic_import_modules,
                    Value::new(availability_info),
                )
                .await?;

                Ok(SsrChunk::NodeJs {
                    entry: ssr_entry_chunks,
                    dynamic_import_entries,
                    availability_info,
                }
                .cell())
            }
        }
        .instrument(tracing::info_span!("page server side rendering"))
        .await
    }

    #[turbo_tasks::function]
    async fn ssr_chunk(self: Vc<Self>) -> Result<Vc<SsrChunk>> {
        let this = self.await?;
        Ok(self.internal_ssr_chunk(
            Value::new(ReferenceType::Entry(EntryReferenceSubType::Page)),
            this.pages_project
                .project()
                .node_root()
                .join("server".to_string()),
            this.pages_project.project().project_path(),
            this.pages_project.ssr_module_context(),
            this.pages_project.edge_ssr_module_context(),
            this.pages_project.project().server_chunking_context(),
            this.pages_project.project().edge_chunking_context(),
            this.pages_project.ssr_runtime_entries(),
            this.pages_project.edge_ssr_runtime_entries(),
        ))
    }

    #[turbo_tasks::function]
    async fn ssr_data_chunk(self: Vc<Self>) -> Result<Vc<SsrChunk>> {
        let this = self.await?;
        Ok(self.internal_ssr_chunk(
            Value::new(ReferenceType::Entry(EntryReferenceSubType::Page)),
            this.pages_project
                .project()
                .node_root()
                .join("server/data".to_string()),
            this.pages_project.project().project_path(),
            this.pages_project.ssr_data_module_context(),
            this.pages_project.edge_ssr_data_module_context(),
            this.pages_project.project().server_chunking_context(),
            this.pages_project.project().edge_chunking_context(),
            this.pages_project.ssr_data_runtime_entries(),
            this.pages_project.edge_ssr_data_runtime_entries(),
        ))
    }

    #[turbo_tasks::function]
    async fn api_chunk(self: Vc<Self>) -> Result<Vc<SsrChunk>> {
        let this = self.await?;
        Ok(self.internal_ssr_chunk(
            Value::new(ReferenceType::Entry(EntryReferenceSubType::PagesApi)),
            this.pages_project
                .project()
                .node_root()
                .join("server".to_string()),
            this.pages_project.project().project_path(),
            this.pages_project.api_module_context(),
            this.pages_project.edge_api_module_context(),
            this.pages_project.project().server_chunking_context(),
            this.pages_project.project().edge_chunking_context(),
            this.pages_project.ssr_runtime_entries(),
            this.pages_project.edge_ssr_runtime_entries(),
        ))
    }

    #[turbo_tasks::function]
    async fn pages_manifest(
        self: Vc<Self>,
        entry_chunk: Vc<Box<dyn OutputAsset>>,
    ) -> Result<Vc<Box<dyn OutputAsset>>> {
        let this = self.await?;
        let node_root = this.pages_project.project().node_root();
        let chunk_path = entry_chunk.ident().path().await?;

        let asset_path = node_root
            .join("server".to_string())
            .await?
            .get_path_to(&chunk_path)
            .context("ssr chunk entry path must be inside the node root")?;

        let pages_manifest = PagesManifest {
            pages: [(this.pathname.await?.clone_value(), asset_path.to_string())]
                .into_iter()
                .collect(),
        };
        let manifest_path_prefix = get_asset_prefix_from_pathname(&this.pathname.await?);
        Ok(Vc::upcast(VirtualOutputAsset::new(
            node_root.join(format!(
                "server/pages{manifest_path_prefix}/pages-manifest.json",
            )),
            AssetContent::file(File::from(serde_json::to_string_pretty(&pages_manifest)?).into()),
        )))
    }

    #[turbo_tasks::function]
    async fn react_loadable_manifest(
        self: Vc<Self>,
        dynamic_import_entries: Vc<DynamicImportedChunks>,
    ) -> Result<Vc<OutputAssets>> {
        let this = self.await?;
        let node_root = this.pages_project.project().node_root();
        let pages_dir = this.pages_project.pages_dir().await?;

        let dynamic_import_entries = &*dynamic_import_entries.await?;

        let mut output = vec![];
        let mut loadable_manifest: HashMap<String, LoadableManifest> = Default::default();
        for (origin, dynamic_imports) in dynamic_import_entries.into_iter() {
            let origin_path = &*origin.ident().path().await?;

            for (import, chunk_output) in dynamic_imports {
                let chunk_output = chunk_output.await?;
                output.extend(chunk_output.iter().copied());

                // https://github.com/vercel/next.js/blob/b7c85b87787283d8fb86f705f67bdfabb6b654bb/packages/next-swc/crates/next-transform-dynamic/src/lib.rs#L230
                // For the pages dir, next_dynamic transform puts relative paths to the pages
                // dir for the origin import.
                let id = format!(
                    "{} -> {}",
                    pages_dir
                        .get_path_to(origin_path)
                        .map_or_else(|| origin_path.to_string(), |path| path.to_string()),
                    import
                );

                let server_path = node_root.join("server".to_string());
                let server_path_value = server_path.await?;
                let files = chunk_output
                    .iter()
                    .map(move |file| {
                        let server_path_value = server_path_value.clone();
                        async move {
                            Ok(server_path_value
                                .get_path_to(&*file.ident().path().await?)
                                .map(|path| path.to_string()))
                        }
                    })
                    .try_flat_join()
                    .await?;

                let manifest_item = LoadableManifest {
                    id: id.clone(),
                    files,
                };

                loadable_manifest.insert(id, manifest_item);
            }
        }

        let loadable_path_prefix = get_asset_prefix_from_pathname(&this.pathname.await?);
        let loadable_manifest = Vc::upcast(VirtualOutputAsset::new(
            node_root.join(format!(
                "server/pages{loadable_path_prefix}/react-loadable-manifest.json"
            )),
            AssetContent::file(
                FileContent::Content(File::from(serde_json::to_string_pretty(
                    &loadable_manifest,
                )?))
                .cell(),
            ),
        ));

        output.push(loadable_manifest);
        Ok(Vc::cell(output))
    }

    #[turbo_tasks::function]
    async fn build_manifest(
        self: Vc<Self>,
        client_chunks: Vc<OutputAssets>,
    ) -> Result<Vc<Box<dyn OutputAsset>>> {
        let this = self.await?;
        let node_root = this.pages_project.project().node_root();
        let client_relative_path = this.pages_project.project().client_relative_path();
        let client_relative_path_ref = client_relative_path.await?;
        let build_manifest = BuildManifest {
            pages: [(
                this.pathname.await?.clone_value(),
                client_chunks
                    .await?
                    .iter()
                    .copied()
                    .map(|chunk| {
                        let client_relative_path_ref = client_relative_path_ref.clone();
                        async move {
                            let chunk_path = chunk.ident().path().await?;
                            Ok(client_relative_path_ref
                                .get_path_to(&chunk_path)
                                .context("client chunk entry path must be inside the client root")?
                                .to_string())
                        }
                    })
                    .try_join()
                    .await?,
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let manifest_path_prefix = get_asset_prefix_from_pathname(&this.pathname.await?);
        Ok(Vc::upcast(VirtualOutputAsset::new(
            node_root.join(format!(
                "server/pages{manifest_path_prefix}/build-manifest.json",
            )),
            AssetContent::file(File::from(serde_json::to_string_pretty(&build_manifest)?).into()),
        )))
    }

    #[turbo_tasks::function]
    fn output_assets(self: Vc<Self>) -> Vc<OutputAssets> {
        self.output().output_assets()
    }

    #[turbo_tasks::function]
    async fn output(self: Vc<Self>) -> Result<Vc<PageEndpointOutput>> {
        let this = self.await?;

        let mut server_assets = vec![];
        let mut client_assets = vec![];

        let ssr_chunk = match this.ty {
            PageEndpointType::Html => {
                let client_chunks = self.client_chunks();
                client_assets.extend(client_chunks.await?.iter().copied());
                let build_manifest = self.build_manifest(client_chunks);
                server_assets.push(build_manifest);
                self.ssr_chunk()
            }
            PageEndpointType::Data => self.ssr_data_chunk(),
            PageEndpointType::Api => self.api_chunk(),
            PageEndpointType::SsrOnly => self.ssr_chunk(),
        };

        let pathname = this.pathname.await?;
        let original_name = this.original_name.await?;

        let client_assets = OutputAssets::new(client_assets);

        let next_font_manifest_output = create_font_manifest(
            this.pages_project.project().client_root(),
            this.pages_project.project().node_root(),
            this.pages_project.pages_dir(),
            "",
            &pathname,
            &original_name,
            client_assets,
            false,
        )
        .await?;
        server_assets.push(next_font_manifest_output);

        let page_output = match *ssr_chunk.await? {
            SsrChunk::NodeJs {
                entry,
                dynamic_import_entries,
                ..
            } => {
                let pages_manifest = self.pages_manifest(entry);
                server_assets.push(pages_manifest);
                server_assets.push(entry);

                let loadable_manifest_output = self.react_loadable_manifest(dynamic_import_entries);
                server_assets.extend(loadable_manifest_output.await?.iter().copied());

                PageEndpointOutput::NodeJs {
                    entry_chunk: entry,
                    server_assets: Vc::cell(server_assets),
                    client_assets,
                }
            }
            SsrChunk::Edge {
                files,
                dynamic_import_entries,
                ..
            } => {
                let node_root = this.pages_project.project().node_root();
                let files_value = files.await?;
                if let Some(&file) = files_value.first() {
                    let pages_manifest = self.pages_manifest(file);
                    server_assets.push(pages_manifest);
                }
                server_assets.extend(files_value.iter().copied());

                // the next-edge-ssr-loader templates expect the manifests to be stored in
                // global variables defined in these files
                //
                // they are created in `setup-dev-bundler.ts`
                let mut file_paths_from_root = vec![
                    "server/server-reference-manifest.js".to_string(),
                    "server/middleware-build-manifest.js".to_string(),
                    "server/middleware-react-loadable-manifest.js".to_string(),
                    "server/next-font-manifest.js".to_string(),
                ];
                let mut wasm_paths_from_root = vec![];

                let node_root_value = node_root.await?;

                file_paths_from_root
                    .extend(get_js_paths_from_root(&node_root_value, &files_value).await?);

                let all_output_assets = all_assets_from_entries(files).await?;

                wasm_paths_from_root
                    .extend(get_wasm_paths_from_root(&node_root_value, &all_output_assets).await?);

                let named_regex = get_named_middleware_regex(&pathname);
                let matchers = MiddlewareMatcher {
                    regexp: Some(named_regex),
                    original_source: pathname.to_string(),
                    ..Default::default()
                };
                let original_name = this.original_name.await?;
                let edge_function_definition = EdgeFunctionDefinition {
                    files: file_paths_from_root,
                    wasm: wasm_paths_to_bindings(wasm_paths_from_root),
                    name: pathname.to_string(),
                    page: original_name.to_string(),
                    regions: None,
                    matchers: vec![matchers],
                    ..Default::default()
                };
                let middleware_manifest_v2 = MiddlewaresManifestV2 {
                    sorted_middleware: vec![pathname.to_string()],
                    functions: [(pathname.to_string(), edge_function_definition)]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                };
                let manifest_path_prefix = get_asset_prefix_from_pathname(&this.pathname.await?);
                let middleware_manifest_v2 = Vc::upcast(VirtualOutputAsset::new(
                    node_root.join(format!(
                        "server/pages{manifest_path_prefix}/middleware-manifest.json"
                    )),
                    AssetContent::file(
                        FileContent::Content(File::from(serde_json::to_string_pretty(
                            &middleware_manifest_v2,
                        )?))
                        .cell(),
                    ),
                ));
                server_assets.push(middleware_manifest_v2);

                let loadable_manifest_output = self.react_loadable_manifest(dynamic_import_entries);
                server_assets.extend(loadable_manifest_output.await?.iter().copied());

                PageEndpointOutput::Edge {
                    files,
                    server_assets: Vc::cell(server_assets),
                    client_assets,
                }
            }
        };

        Ok(page_output.cell())
    }

    #[turbo_tasks::function]
    fn client_relative_path(&self) -> Vc<FileSystemPathOption> {
        Vc::cell(Some(self.pages_project.project().client_relative_path()))
    }
}

#[turbo_tasks::value_impl]
impl Endpoint for PageEndpoint {
    #[turbo_tasks::function]
    async fn write_to_disk(self: Vc<Self>) -> Result<Vc<WrittenEndpoint>> {
        let this = self.await?;
        let span = {
            let original_name = this.original_name.await?;
            match this.ty {
                PageEndpointType::Html => {
                    tracing::info_span!("page endpoint HTML", name = *original_name)
                }
                PageEndpointType::Data => {
                    tracing::info_span!("page endpoint data", name = *original_name)
                }
                PageEndpointType::Api => {
                    tracing::info_span!("page endpoint API", name = *original_name)
                }
                PageEndpointType::SsrOnly => {
                    tracing::info_span!("page endpoint SSR", name = *original_name)
                }
            }
        };
        async move {
            let output = self.output();
            // Must use self.output_assets() instead of output.output_assets() to make it a
            // single operation
            let output_assets = self.output_assets();

            this.pages_project
                .project()
                .emit_all_output_assets(Vc::cell(output_assets))
                .await?;

            let node_root = this.pages_project.project().node_root();
            let server_paths = all_server_paths(output_assets, node_root)
                .await?
                .clone_value();

            let node_root = &node_root.await?;
            let written_endpoint = match *output.await? {
                PageEndpointOutput::NodeJs { entry_chunk, .. } => WrittenEndpoint::NodeJs {
                    server_entry_path: node_root
                        .get_path_to(&*entry_chunk.ident().path().await?)
                        .context("ssr chunk entry path must be inside the node root")?
                        .to_string(),
                    server_paths,
                },
                PageEndpointOutput::Edge { .. } => WrittenEndpoint::Edge { server_paths },
            };

            Ok(written_endpoint.cell())
        }
        .instrument(span)
        .await
    }

    #[turbo_tasks::function]
    async fn server_changed(self: Vc<Self>) -> Result<Vc<Completion>> {
        Ok(self
            .await?
            .pages_project
            .project()
            .server_changed(self.output().server_assets()))
    }

    #[turbo_tasks::function]
    async fn client_changed(self: Vc<Self>) -> Result<Vc<Completion>> {
        Ok(self
            .await?
            .pages_project
            .project()
            .client_changed(self.output().client_assets()))
    }
}

#[turbo_tasks::value]
enum PageEndpointOutput {
    NodeJs {
        entry_chunk: Vc<Box<dyn OutputAsset>>,
        server_assets: Vc<OutputAssets>,
        client_assets: Vc<OutputAssets>,
    },
    Edge {
        files: Vc<OutputAssets>,
        server_assets: Vc<OutputAssets>,
        client_assets: Vc<OutputAssets>,
    },
}

#[turbo_tasks::value_impl]
impl PageEndpointOutput {
    #[turbo_tasks::function]
    pub async fn output_assets(self: Vc<Self>) -> Result<Vc<OutputAssets>> {
        let server_assets = self.server_assets().await?;
        let client_assets = self.client_assets().await?;
        Ok(Vc::cell(
            server_assets
                .iter()
                .cloned()
                .chain(client_assets.iter().cloned())
                .collect(),
        ))
    }

    #[turbo_tasks::function]
    pub fn server_assets(&self) -> Vc<OutputAssets> {
        match *self {
            PageEndpointOutput::NodeJs { server_assets, .. }
            | PageEndpointOutput::Edge { server_assets, .. } => server_assets,
        }
    }

    #[turbo_tasks::function]
    pub fn client_assets(&self) -> Vc<OutputAssets> {
        match *self {
            PageEndpointOutput::NodeJs { client_assets, .. }
            | PageEndpointOutput::Edge { client_assets, .. } => client_assets,
        }
    }
}

#[turbo_tasks::value]
pub enum SsrChunk {
    NodeJs {
        entry: Vc<Box<dyn OutputAsset>>,
        dynamic_import_entries: Vc<DynamicImportedChunks>,
        availability_info: AvailabilityInfo,
    },
    Edge {
        files: Vc<OutputAssets>,
        dynamic_import_entries: Vc<DynamicImportedChunks>,
        availability_info: AvailabilityInfo,
    },
}
