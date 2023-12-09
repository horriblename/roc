use std::{
    collections::HashMap,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
};

use bumpalo::Bump;
use roc_can::{abilities::AbilitiesStore, expr::Declarations};
use roc_collections::MutMap;
use roc_load::{CheckedModule, LoadedModule};
use roc_module::symbol::{Interns, ModuleId, Symbol};
use roc_packaging::cache::{self, RocCacheDir};
use roc_region::all::LineInfo;
use roc_reporting::report::RocDocAllocator;
use roc_solve_problem::TypeError;
use roc_types::subs::{Subs, Variable};

use tower_lsp::lsp_types::{
    CompletionItem, Diagnostic, GotoDefinitionResponse, Hover, HoverContents, Location,
    MarkedString, Position, Range, SemanticTokenType, SemanticTokens, SemanticTokensResult,
    TextEdit, Url,
};

mod completion;
mod parse_ast;
mod semantic_tokens;
mod tokens;

use crate::{
    analysis::completion::{field_completion, get_completion_items},
    convert::{
        diag::{IntoLspDiagnostic, ProblemFmt},
        ToRange, ToRocPosition,
    },
};

use self::{parse_ast::Ast, semantic_tokens::arrange_semantic_tokens, tokens::Token};
pub const HIGHLIGHT_TOKENS_LEGEND: &[SemanticTokenType] = Token::LEGEND;

fn format_var_type(
    var: Variable,
    subs: &mut Subs,
    module_id: &ModuleId,
    interns: &Interns,
) -> String {
    let snapshot = subs.snapshot();
    let type_str = roc_types::pretty_print::name_and_print_var(
        var,
        subs,
        *module_id,
        interns,
        roc_types::pretty_print::DebugPrint::NOTHING,
    );
    subs.rollback_to(snapshot);
    type_str
}
pub(crate) fn global_anal(
    source_url: Url,
    source: String,
    version: u32,
) -> (impl FnOnce() -> Vec<AnalyzedDocument>, DocInfo) {
    let fi = source_url.to_file_path().unwrap();
    let src_dir = find_src_dir(&fi).to_path_buf();
    let line_info = LineInfo::new(&source);

    let doc_info = DocInfo {
        url: source_url.clone(),
        line_info: line_info.clone(),
        source: source.clone(),
        version,
    };
    //We will return this before the analisys has completed to enable completion
    let doc_info_return = doc_info.clone();
    let documents_future = move || {
        let arena = Bump::new();
        let loaded = roc_load::load_and_typecheck_str(
            &arena,
            fi,
            &source,
            src_dir,
            roc_target::TargetInfo::default_x86_64(),
            roc_load::FunctionKind::LambdaSet,
            roc_reporting::report::RenderTarget::Generic,
            RocCacheDir::Persistent(cache::roc_cache_dir().as_path()),
            roc_reporting::report::DEFAULT_PALETTE,
        );

        let module = match loaded {
            Ok(module) => module,
            Err(problem) => {
                let all_problems = problem
                    .into_lsp_diagnostic(&())
                    .into_iter()
                    .collect::<Vec<_>>();

                let analyzed_document = AnalyzedDocument {
                    doc_info,
                    analysys_result: AnalysisResult {
                        module: None,
                        diagnostics: all_problems,
                    },
                };

                return vec![analyzed_document];
            }
        };

        let mut documents = vec![];

        let LoadedModule {
            interns,
            mut can_problems,
            mut type_problems,
            mut declarations_by_id,
            sources,
            mut typechecked,
            solved,
            abilities_store,
            ..
        } = module;

        let mut root_module = Some(RootModule {
            subs: solved.into_inner(),
            abilities_store,
        });

        let mut builder = AnalyzedDocumentBuilder {
            interns: &interns,
            module_id_to_url: module_id_to_url_from_sources(&sources),
            can_problems: &mut can_problems,
            type_problems: &mut type_problems,
            declarations_by_id: &mut declarations_by_id,
            typechecked: &mut typechecked,
            root_module: &mut root_module,
        };

        for (module_id, (path, source)) in sources {
            documents.push(builder.build_document(path, source, module_id, version));
        }

        documents
    };
    (documents_future, doc_info_return)
}

fn find_src_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

fn _find_parent_git_repo(path: &Path) -> Option<&Path> {
    let mut path = path;
    loop {
        if path.join(".git").exists() {
            return Some(path);
        }

        path = path.parent()?;
    }
}

fn module_id_to_url_from_sources(sources: &MutMap<ModuleId, (PathBuf, Box<str>)>) -> ModuleIdToUrl {
    sources
        .iter()
        .map(|(module_id, (path, _))| {
            let url = path_to_url(path);
            (*module_id, url)
        })
        .collect()
}

fn path_to_url(path: &Path) -> Url {
    if path.is_relative() {
        // Make it <tmpdir>/path
        let tmpdir = std::env::temp_dir();
        Url::from_file_path(tmpdir.join(path)).unwrap()
    } else {
        Url::from_file_path(path).unwrap()
    }
}

struct RootModule {
    subs: Subs,
    abilities_store: AbilitiesStore,
}

struct AnalyzedDocumentBuilder<'a> {
    interns: &'a Interns,
    module_id_to_url: ModuleIdToUrl,
    can_problems: &'a mut MutMap<ModuleId, Vec<roc_problem::can::Problem>>,
    type_problems: &'a mut MutMap<ModuleId, Vec<TypeError>>,
    declarations_by_id: &'a mut MutMap<ModuleId, Declarations>,
    typechecked: &'a mut MutMap<ModuleId, CheckedModule>,
    root_module: &'a mut Option<RootModule>,
}

impl<'a> AnalyzedDocumentBuilder<'a> {
    fn build_document(
        &mut self,
        path: PathBuf,
        source: Box<str>,
        module_id: ModuleId,
        version: u32,
    ) -> AnalyzedDocument {
        let subs;
        let abilities;
        let declarations;

        if let Some(m) = self.typechecked.remove(&module_id) {
            subs = m.solved_subs.into_inner();
            abilities = m.abilities_store;
            declarations = m.decls;
        } else {
            let rm = self.root_module.take().unwrap();
            subs = rm.subs;
            abilities = rm.abilities_store;
            declarations = self.declarations_by_id.remove(&module_id).unwrap();
        }

        let analyzed_module = AnalyzedModule {
            subs,
            abilities,
            declarations,
            module_id,
            interns: self.interns.clone(),
            module_id_to_url: self.module_id_to_url.clone(),
        };

        let line_info = LineInfo::new(&source);
        let diagnostics = self.build_diagnostics(&path, &source, &line_info, module_id);

        AnalyzedDocument {
            doc_info: DocInfo {
                url: path_to_url(&path),
                line_info,
                source: source.into(),
                version,
            },
            analysys_result: AnalysisResult {
                module: Some(analyzed_module),
                diagnostics,
            },
        }
    }

    fn build_diagnostics(
        &mut self,
        source_path: &Path,
        source: &str,
        line_info: &LineInfo,
        module_id: ModuleId,
    ) -> Vec<Diagnostic> {
        let lines: Vec<_> = source.lines().collect();

        let alloc = RocDocAllocator::new(&lines, module_id, self.interns);

        let mut all_problems = Vec::new();
        let fmt = ProblemFmt {
            alloc: &alloc,
            line_info,
            path: source_path,
        };

        let can_problems = self.can_problems.remove(&module_id).unwrap_or_default();

        let type_problems = self.type_problems.remove(&module_id).unwrap_or_default();

        for can_problem in can_problems {
            if let Some(diag) = can_problem.into_lsp_diagnostic(&fmt) {
                all_problems.push(diag);
            }
        }

        for type_problem in type_problems {
            if let Some(diag) = type_problem.into_lsp_diagnostic(&fmt) {
                all_problems.push(diag);
            }
        }

        all_problems
    }
}

type ModuleIdToUrl = HashMap<ModuleId, Url>;

#[derive(Debug, Clone)]
struct AnalyzedModule {
    module_id: ModuleId,
    interns: Interns,
    subs: Subs,
    abilities: AbilitiesStore,
    declarations: Declarations,
    // We need this because ModuleIds are not stable between compilations, so a ModuleId visible to
    // one module may not be true global to the language server.
    module_id_to_url: ModuleIdToUrl,
}

//This needs a rewrite
/*
Requirements:
We should be able to get the latest results of analysis as well as the last good analysis resutl
We should be able to get the source, lineinfo and url of the document without having completed the anaylys
We need a way for functions that need the completed anaylisis to be completed to wait on that completion

*/
/*
access completed result
I could make a type that either contains an async task or the async result, when you access it it blocks and either awaits the task or returns the completed result.
I could make it so that it contains an async mutex and it holds the first lock open then it is created it releases the lock once the task is completed
*/

#[derive(Debug, Clone)]

pub struct AnalyzedDocument {
    //TOODO make not pugb
    pub doc_info: DocInfo,
    pub analysys_result: AnalysisResult,
}
#[derive(Debug, Clone)]
pub struct DocInfo {
    pub url: Url,
    pub line_info: LineInfo,
    pub source: String,
    pub version: u32,
}
impl DocInfo {
    fn whole_document_range(&self) -> Range {
        let start = Position::new(0, 0);
        let end = Position::new(self.line_info.num_lines(), 0);
        Range::new(start, end)
    }
    pub fn get_prefix_at_position(&self, position: Position) -> String {
        let position = position.to_roc_position(&self.line_info);
        let offset = position.offset;
        let source = self.source.as_bytes().split_at(offset as usize).0;
        // writeln!(std::io::stderr(), "prefix source{:?}", self.source);

        // let last_few = self
        //     .source
        //     .split_at((offset - 5) as usize)
        //     .1
        //     .split_at((offset + 5) as usize)
        //     .0;
        // let splitter = last_few.split_at(5);

        // writeln!(
        //     std::io::stderr(),
        //     "starting to get completion items at offset:{:?} content:: '{:?}|{:?}'",
        //     offset,
        //     splitter.0,
        //     splitter.1
        // );
        let mut symbol = source
            .iter()
            .rev()
            //TODO proper regex here
            .take_while(|&&a| matches!(a as char,'a'..='z'|'A'..='Z'|'0'..='9'|'_'|'.'))
            .map(|&a| a)
            .collect::<Vec<u8>>();
        symbol.reverse();

        String::from_utf8(symbol).unwrap()
    }
    pub fn format(&self) -> Option<Vec<TextEdit>> {
        let source = &self.source;
        let arena = &Bump::new();

        let ast = Ast::parse(arena, source).ok()?;
        let fmt = ast.fmt();

        if source == fmt.as_str() {
            None
        } else {
            let range = self.whole_document_range();
            let text_edit = TextEdit::new(range, fmt.to_string().to_string());
            Some(vec![text_edit])
        }
    }

    pub fn semantic_tokens(&self) -> Option<SemanticTokensResult> {
        let source = &self.source;
        let arena = &Bump::new();

        let ast = Ast::parse(arena, source).ok()?;
        let tokens = ast.semantic_tokens();

        let data = arrange_semantic_tokens(tokens, &self.line_info);

        Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        }))
    }
}
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    module: Option<AnalyzedModule>,
    diagnostics: Vec<Diagnostic>,
}

impl AnalyzedDocument {
    pub fn url(&self) -> &Url {
        &self.doc_info.url
    }

    fn line_info(&self) -> &LineInfo {
        &self.doc_info.line_info
    }

    fn module(&self) -> Option<&AnalyzedModule> {
        self.analysys_result.module.as_ref()
    }
    pub fn doc_info(&self) -> &DocInfo {
        &self.doc_info
    }

    fn location(&self, range: Range) -> Location {
        Location {
            uri: self.doc_info.url.clone(),
            range,
        }
    }

    pub fn type_checked(&self) -> bool {
        self.analysys_result.module.is_some()
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.analysys_result.diagnostics.clone()
    }

    pub fn symbol_at(&self, position: Position) -> Option<Symbol> {
        let line_info = self.line_info();

        let position = position.to_roc_position(line_info);

        let AnalyzedModule {
            declarations,
            abilities,
            ..
        } = self.module()?;

        let found_symbol =
            roc_can::traverse::find_closest_symbol_at(position, declarations, abilities)?;

        Some(found_symbol.implementation_symbol())
    }

    pub fn hover(&self, position: Position) -> Option<Hover> {
        let line_info = self.line_info();

        let pos = position.to_roc_position(line_info);

        let AnalyzedModule {
            subs,
            declarations,
            module_id,
            interns,
            ..
        } = self.module()?;

        let (region, var) = roc_can::traverse::find_closest_type_at(pos, declarations)?;
        let type_str = format_var_type(var, &mut subs.clone(), module_id, interns);

        let range = region.to_range(self.line_info());

        Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(type_str)),
            range: Some(range),
        })
    }

    pub fn definition(&self, symbol: Symbol) -> Option<GotoDefinitionResponse> {
        let AnalyzedModule { declarations, .. } = self.module()?;

        let found_declaration = roc_can::traverse::find_declaration(symbol, declarations)?;

        let range = found_declaration.region().to_range(self.line_info());

        Some(GotoDefinitionResponse::Scalar(self.location(range)))
    }

    pub(crate) fn module_url(&self, module_id: ModuleId) -> Option<Url> {
        self.module()?.module_id_to_url.get(&module_id).cloned()
    }

    pub fn completion_items(
        &self,
        position: Position,
        latest_doc: &DocInfo,
        symbol_prefix: String,
    ) -> Option<Vec<CompletionItem>> {
        let symbol_prefix = latest_doc.get_prefix_at_position(position);
        writeln!(
            std::io::stderr(),
            "starting to get completion items for prefix: {:?} docVersion:{:?}",
            symbol_prefix,
            latest_doc.version
        );
        let len_diff = latest_doc.source.len() as i32 - self.doc_info.source.len() as i32;

        let mut position = position.to_roc_position(&latest_doc.line_info);
        //TODO: this is kind of a hack and should be removed once we can do some minimal parsing without full type checking
        position.offset = (position.offset as i32 - len_diff - 1) as u32;
        writeln!(
            std::io::stderr(),
            "completion offset: {:?}",
            position.offset
        );

        let AnalyzedModule {
            module_id,
            interns,
            subs,
            declarations,
            ..
        } = self.module()?;

        let is_field_completion = symbol_prefix.contains('.');
        if is_field_completion {
            field_completion(
                position,
                symbol_prefix,
                &declarations,
                &interns,
                &mut subs.clone(),
                &module_id,
            )
        } else {
            let completions = get_completion_items(
                position,
                symbol_prefix,
                declarations,
                &mut subs.clone(),
                module_id,
                interns,
            );
            writeln!(std::io::stderr(), "got completions: ");
            Some(completions)
        }
    }
}
