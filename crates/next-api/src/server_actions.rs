use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bincode::{Decode, Encode};
use next_core::{
    next_manifests::{ActionLayer, ActionManifestWorkerEntry, ServerReferenceManifest},
    util::NextRuntime,
};
use swc_core::{
    atoms::{Atom, atom},
    common::comments::Comments,
    ecma::{
        ast::{
            Decl, ExportSpecifier, Id, ModuleDecl, ModuleItem, ObjectLit, Program,
            PropOrSpread::Prop,
        },
        utils::find_pat_ids,
    },
};
use turbo_rcstr::RcStr;
use turbo_tasks::{
    FxIndexMap, NonLocalValue, ResolvedVc, TryFlatJoinIterExt, TryJoinIterExt, Vc,
    trace::TraceRawVcs,
};
use turbo_tasks_fs::{self, File, FileContent, FileSystemPath};
use turbopack_core::{
    asset::AssetContent,
    chunk::ChunkingContext,
    context::AssetContext,
    emit_collect::EmittedModuleReference,
    file_source::FileSource,
    module::Module,
    module_graph::{ModuleGraph, ModuleGraphLayer, async_module_info::AsyncModulesInfo},
    output::OutputAsset,
    reference_type::{EcmaScriptModulesReferenceSubType, ReferenceType},
    resolve::ModulePart,
    virtual_output::VirtualOutputAsset,
};
use turbopack_ecmascript::{
    EcmascriptParsable, parse::ParseResult, tree_shake::part::module::EcmascriptModulePartAsset,
};

/// Scans the RSC entry point's full module graph looking for exported Server
/// Actions (identifiable by a magic comment in the transformed module's
/// output), and constructs a evaluatable "action loader" entry point and
/// manifest describing the found actions.
///
/// If Server Actions are not enabled, this returns an empty manifest and a None
/// loader.
#[turbo_tasks::function]
pub(crate) async fn create_server_actions_manifest(
    server_action_loader: Vc<Box<dyn Module>>,
    node_root: FileSystemPath,
    page_name: RcStr,
    runtime: NextRuntime,
    module_graph: Vc<ModuleGraph>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
) -> Vc<Box<dyn OutputAsset>> {
    let actions = collect_actions(server_action_loader, module_graph);
    build_manifest(
        node_root,
        page_name,
        runtime,
        actions,
        chunking_context,
        module_graph.async_module_info(),
    )
}

#[turbo_tasks::function]
async fn collect_actions(
    server_action_loader: ResolvedVc<Box<dyn Module>>,
    module_graph: Vc<ModuleGraph>,
) -> Result<Vc<AllActions>> {
    let collected_modules = module_graph.collected_modules().await?;

    // This mirrors what the __turbopack_collect__ in
    // packages/next/src/build/templates/turbopack-action-loader.ts ends up chunking into the chunk.

    // This can be none if there are no server actions
    let actions =
        collected_modules
            .collected_references
            .iter()
            .find_map(|((entry, _loader), actions)| {
                if *entry == server_action_loader {
                    // TODO also filter based on _loader
                    Some(actions)
                } else {
                    None
                }
            });

    Ok(Vc::cell(
        actions
            .into_iter()
            .flatten()
            .map(async |(data, module, _)| {
                let data =
                    ResolvedVc::try_sidecast::<Box<dyn EmittedModuleReference>>(data.reference)
                        .context(
                            "Expected collected server action reference to be \
                             EmittedModuleReference",
                        )?
                        .data()
                        .await?;
                let mut data = data
                    .as_ref()
                    .context("Expected emitted module reference data to be not empty")?
                    .split("|");
                let hash = data.next().unwrap();
                let name = data.next().unwrap();

                Ok((
                    hash.to_string(),
                    (
                        ActionLayer::ActionBrowser,
                        ActionMeta {
                            name: name.to_string(),
                            source_path: "".to_string(), // TODO
                        },
                        *module,
                    ),
                ))
            })
            .try_join()
            .await?,
    ))
}

/// Builds a manifest containing every action's hashed id, with an internal
/// module id which exports a function using that hashed name.
#[turbo_tasks::function]
async fn build_manifest(
    node_root: FileSystemPath,
    page_name: RcStr,
    runtime: NextRuntime,
    actions: Vc<AllActions>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
    async_module_info: Vc<AsyncModulesInfo>,
) -> Result<Vc<Box<dyn OutputAsset>>> {
    let manifest_path_prefix = &page_name;
    let manifest_path = node_root.join(&format!(
        "server/app{manifest_path_prefix}/server-reference-manifest.json",
    ))?;
    let mut manifest = ServerReferenceManifest {
        ..Default::default()
    };

    let key = format!("app{page_name}");

    let actions_value = actions.await?;
    let mapping = match runtime {
        NextRuntime::Edge => &mut manifest.edge,
        NextRuntime::NodeJs => &mut manifest.node,
    };

    let chunk_item_id_strategy = chunking_context.chunk_item_id_strategy().await?;

    // Collect all the action metadata including filenames and location
    let mut action_metadata = Vec::new();
    for (hash_id, (layer, meta, module)) in actions_value.iter() {
        // Use source_path from the action comment if available (contains original .ts/.tsx path),
        // otherwise fall back to module.ident().path() (may be compiled .js path)
        let filename = if !meta.source_path.is_empty() {
            meta.source_path.clone()
        } else {
            let module_path = module.ident().path().await?;
            module_path.to_string()
        };

        action_metadata.push((
            hash_id.clone(),
            (
                *layer,
                meta.name.clone(),
                filename,
                chunk_item_id_strategy.get_id_from_module(**module).await?,
                async_module_info.is_async(*module).await?,
            ),
        ));
    }

    println!("build_manifest {} {:#?}", page_name, action_metadata);

    // Now create the manifest entries
    for (hash_id, (layer, name, filename, module_id, is_async)) in &action_metadata {
        let entry = mapping.entry(hash_id.as_str()).or_default();
        entry.workers.insert(
            &key,
            ActionManifestWorkerEntry {
                module_id: module_id.into(),
                is_async: *is_async,
                exported_name: name.as_str(),
                filename: filename.as_str(),
            },
        );
        entry.layer.insert(&key, *layer);

        // Hoist the filename and exported_name to the entry level
        entry.exported_name = name.as_str();
        entry.filename = filename.as_str();
    }

    Ok(Vc::upcast(VirtualOutputAsset::new(
        manifest_path,
        AssetContent::file(
            FileContent::Content(File::from(serde_json::to_string_pretty(&manifest)?)).cell(),
        ),
    )))
}

/// The ActionBrowser layer's module is in the Client context, and we need to
/// bring it into the RSC context.
pub async fn to_rsc_context(
    client_module: Vc<Box<dyn Module>>,
    entry_path: &str,
    entry_query: &str,
    asset_context: Vc<Box<dyn AssetContext>>,
) -> Result<ResolvedVc<Box<dyn Module>>> {
    // TODO a cleaner solution would something similar to the EcmascriptClientReferenceModule, as
    // opposed to the following hack to construct the RSC module corresponding to this client
    // module.
    let source = FileSource::new_with_query(
        client_module
            .ident()
            .path()
            .await?
            .root()
            .await?
            .join(entry_path)?,
        entry_query.into(),
    );
    let module = asset_context
        .process(
            Vc::upcast(source),
            ReferenceType::EcmaScriptModules(EcmaScriptModulesReferenceSubType::Undefined),
        )
        .module()
        .to_resolved()
        .await?;
    Ok(module)
}

/// Server action info for JSON parsing
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(untagged)]
enum ServerActionInfoRaw {
    /// Old format: just the export name as a string
    Name(String),
    /// New format: object with name
    WithName { name: String },
}

impl ServerActionInfoRaw {
    fn into_action_entry(self) -> ActionEntry {
        match self {
            ServerActionInfoRaw::Name(name) => ActionEntry { name },
            ServerActionInfoRaw::WithName { name } => ActionEntry { name },
        }
    }
}

/// Simplified action entry for storage in turbo_tasks values
#[derive(Clone, Debug, PartialEq, Eq, TraceRawVcs, NonLocalValue, Encode, Decode)]
pub struct ActionEntry {
    pub name: String,
}

/// Parses the Server Actions comment for all exported action function names.
///
/// Action names are stored in a leading BlockComment prefixed by
/// `__next_internal_action_entry_do_not_use__`.
pub fn parse_server_actions(
    program: &Program,
    comments: &dyn Comments,
) -> Option<(BTreeMap<String, ActionEntry>, String, String)> {
    let byte_pos = match program {
        Program::Module(m) => m.span.lo,
        Program::Script(s) => s.span.lo,
    };
    comments.get_leading(byte_pos).and_then(|comments| {
        comments.iter().find_map(|c| {
            c.text
                .split_once("__next_internal_action_entry_do_not_use__")
                .and_then(|(_, actions)| {
                    // Try to parse as tuple format: (actions_map, entry_path, entry_query)
                    if let Ok((raw, entry_path, entry_query)) = serde_json::from_str::<(
                        BTreeMap<String, ServerActionInfoRaw>,
                        String,
                        String,
                    )>(actions)
                    {
                        let converted: BTreeMap<String, ActionEntry> = raw
                            .into_iter()
                            .map(|(k, v)| (k, v.into_action_entry()))
                            .collect();
                        return Some((converted, entry_path, entry_query));
                    }
                    // Fall back to just actions map (old format without entry path/query)
                    let raw: BTreeMap<String, ServerActionInfoRaw> =
                        serde_json::from_str(actions).ok()?;
                    let converted: BTreeMap<String, ActionEntry> = raw
                        .into_iter()
                        .map(|(k, v)| (k, v.into_action_entry()))
                        .collect();
                    Some((converted, String::new(), String::new()))
                })
        })
    })
}
/// Inspects the comments inside [Module] looking for the magic actions comment.
/// If found, we return the mapping of every action's hashed id to the name of
/// the exported action function. If not, we return a None.
#[turbo_tasks::function]
async fn parse_actions(module: ResolvedVc<Box<dyn Module>>) -> Result<Vc<OptionActionMap>> {
    let Some(ecmascript_asset) = ResolvedVc::try_sidecast::<Box<dyn EcmascriptParsable>>(module)
    else {
        return Ok(Vc::cell(None));
    };

    if let Some(module) = ResolvedVc::try_downcast_type::<EcmascriptModulePartAsset>(module)
        && matches!(
            module.await?.part,
            ModulePart::Evaluation | ModulePart::Facade
        )
    {
        return Ok(Vc::cell(None));
    }

    let original_parsed = ecmascript_asset.parse_original().resolve().await?;

    let ParseResult::Ok {
        program: original,
        comments,
        ..
    } = &*original_parsed.await?
    else {
        // The file might be parse-able, but this is reported separately.
        return Ok(Vc::cell(None));
    };

    let Some((mut actions, entry_path, entry_query)) = parse_server_actions(original, comments)
    else {
        return Ok(Vc::cell(None));
    };

    let fragment = ecmascript_asset.failsafe_parse().resolve().await?;

    if fragment != original_parsed {
        let ParseResult::Ok {
            program: fragment, ..
        } = &*fragment.await?
        else {
            // The file might be be parse-able, but this is reported separately.
            return Ok(Vc::cell(None));
        };

        let all_exports = all_export_names(fragment);
        actions.retain(|_, entry| all_exports.iter().any(|export| export == &entry.name));
    }

    let mut actions = FxIndexMap::from_iter(actions.into_iter());
    actions.sort_keys();
    Ok(Vc::cell(Some(
        ActionMap {
            actions,
            entry_path,
            entry_query,
        }
        .resolved_cell(),
    )))
}

fn all_export_names(program: &Program) -> Vec<Atom> {
    match program {
        Program::Module(m) => {
            let mut exports = Vec::new();
            for item in m.body.iter() {
                match item {
                    ModuleItem::ModuleDecl(
                        ModuleDecl::ExportDefaultExpr(..) | ModuleDecl::ExportDefaultDecl(..),
                    ) => {
                        exports.push(atom!("default"));
                    }
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(decl)) => match &decl.decl {
                        Decl::Class(c) => {
                            exports.push(c.ident.sym.clone());
                        }
                        Decl::Fn(f) => {
                            exports.push(f.ident.sym.clone());
                        }
                        Decl::Var(v) => {
                            let ids: Vec<Id> = find_pat_ids(v);
                            exports.extend(ids.into_iter().map(|id| id.0));
                        }
                        _ => {}
                    },
                    ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(decl)) => {
                        if is_turbopack_internal_var(&decl.with) {
                            continue;
                        }

                        for s in decl.specifiers.iter() {
                            match s {
                                ExportSpecifier::Named(named) => {
                                    exports.push(
                                        named
                                            .exported
                                            .as_ref()
                                            .unwrap_or(&named.orig)
                                            .atom()
                                            .into_owned(),
                                    );
                                }
                                ExportSpecifier::Default(_) => {
                                    exports.push(atom!("default"));
                                }
                                ExportSpecifier::Namespace(e) => {
                                    exports.push(e.name.atom().into_owned());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            exports
        }

        _ => {
            vec![]
        }
    }
}

fn is_turbopack_internal_var(with: &Option<Box<ObjectLit>>) -> bool {
    with.as_deref()
        .and_then(|v| {
            v.props.iter().find_map(|p| match p {
                Prop(prop) => match &**prop {
                    swc_core::ecma::ast::Prop::KeyValue(key_value_prop) => {
                        if key_value_prop.key.as_ident()?.sym == "__turbopack_var__" {
                            Some(key_value_prop.value.as_lit()?.as_bool()?.value)
                        } else {
                            None
                        }
                    }
                    _ => None,
                },
                _ => None,
            })
        })
        .unwrap_or(false)
}

/// Action metadata including name and source path
#[derive(Clone, Debug, PartialEq, Eq, TraceRawVcs, NonLocalValue, Encode, Decode)]
pub struct ActionMeta {
    pub name: String,
    /// The original source file path (from entry_path in the action comment)
    pub source_path: String,
}

type HashToLayerNameModule = Vec<(
    String,
    (ActionLayer, ActionMeta, ResolvedVc<Box<dyn Module>>),
)>;

/// A mapping of every module which exports a Server Action, with the hashed id
/// and exported name of each found action.
#[turbo_tasks::value(transparent)]
pub struct AllActions(HashToLayerNameModule);

#[turbo_tasks::value_impl]
impl AllActions {
    #[turbo_tasks::function]
    pub fn empty() -> Vc<Self> {
        Vc::cell(Default::default())
    }
}

/// Maps the hashed action id to the action's exported function name and location.
#[turbo_tasks::value]
#[derive(Debug)]
pub struct ActionMap {
    #[bincode(with = "turbo_bincode::indexmap")]
    pub actions: FxIndexMap<String, ActionEntry>,
    pub entry_path: String,
    pub entry_query: String,
}

/// An Option wrapper around [ActionMap].
#[turbo_tasks::value(transparent)]
struct OptionActionMap(Option<ResolvedVc<ActionMap>>);

type LayerAndActions = (ActionLayer, ResolvedVc<ActionMap>);
/// A mapping of every module module containing Server Actions, mapping to its layer and actions.
#[turbo_tasks::value(transparent)]
pub struct AllModuleActions(
    #[bincode(with = "turbo_bincode::indexmap")]
    FxIndexMap<ResolvedVc<Box<dyn Module>>, LayerAndActions>,
);

#[turbo_tasks::function]
pub async fn map_server_actions(
    graph: ResolvedVc<ModuleGraphLayer>,
) -> Result<Vc<AllModuleActions>> {
    let actions = graph
        .await?
        .iter_nodes()
        .map(async |module| {
            // TODO: compare module contexts instead?
            let layer = match module.ident().await?.layer.as_ref() {
                Some(layer) if layer.name() == "app-rsc" || layer.name() == "app-edge-rsc" => {
                    ActionLayer::Rsc
                }
                Some(layer) if layer.name() == "app-client" => ActionLayer::ActionBrowser,
                // TODO really ignore SSR?
                _ => return Ok(None),
            };
            // TODO the old implementation did parse_actions(to_rsc_context(module))
            // is that really necessary?
            Ok(parse_actions(*module)
                .await?
                .map(|action_map| (module, (layer, action_map))))
        })
        .try_flat_join()
        .await?;
    Ok(Vc::cell(actions.into_iter().collect()))
}
