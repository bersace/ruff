use std::sync::Arc;

use rustc_hash::FxHashMap;

use ruff_db::parsed::ParsedModule;
use ruff_index::IndexVec;
use ruff_python_ast as ast;
use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};

use crate::name::Name;
use crate::node_key::NodeKey;
use crate::semantic_index::ast_ids::{
    AstId, AstIdsBuilder, ScopedClassId, ScopedExpressionId, ScopedFunctionId,
};
use crate::semantic_index::definition::{Definition, DefinitionNodeKey};
use crate::semantic_index::symbol::{
    FileScopeId, Scope, ScopeKind, ScopedSymbolId, SymbolFlags, SymbolTableBuilder,
};
use crate::semantic_index::{NodeWithScopeId, SemanticIndex};

pub(super) struct SemanticIndexBuilder<'a> {
    // Builder state
    module: &'a ParsedModule,
    scope_stack: Vec<FileScopeId>,
    /// the definition whose target(s) we are currently walking
    current_definition: Option<Definition>,

    // Semantic Index fields
    scopes: IndexVec<FileScopeId, Scope>,
    symbol_tables: IndexVec<FileScopeId, SymbolTableBuilder>,
    ast_ids: IndexVec<FileScopeId, AstIdsBuilder>,
    scopes_by_expression: FxHashMap<NodeKey, FileScopeId>,
    scopes_by_definition: FxHashMap<DefinitionNodeKey, FileScopeId>,
}

impl<'a> SemanticIndexBuilder<'a> {
    pub(super) fn new(parsed: &'a ParsedModule) -> Self {
        let mut builder = Self {
            module: parsed,
            scope_stack: Vec::new(),
            current_definition: None,

            scopes: IndexVec::new(),
            symbol_tables: IndexVec::new(),
            ast_ids: IndexVec::new(),
            scopes_by_expression: FxHashMap::default(),
            scopes_by_definition: FxHashMap::default(),
        };

        builder.push_scope_with_parent(
            NodeWithScope::new(NodeWithScopeId::Module, Name::new_static("<module>")),
            None,
        );

        builder
    }

    fn current_scope(&self) -> FileScopeId {
        *self
            .scope_stack
            .last()
            .expect("Always to have a root scope")
    }

    fn push_scope(&mut self, node: NodeWithScope) {
        let parent = self.current_scope();
        self.push_scope_with_parent(node, Some(parent));
    }

    fn push_scope_with_parent(&mut self, node: NodeWithScope, parent: Option<FileScopeId>) {
        let children_start = self.scopes.next_index() + 1;
        let scope_kind = node.scope_kind();

        let scope = Scope {
            name: node.name,
            parent,
            node: node.id,
            kind: scope_kind,
            descendents: children_start..children_start,
        };

        let scope_id = self.scopes.push(scope);
        self.symbol_tables.push(SymbolTableBuilder::new());
        let ast_id_scope = self.ast_ids.push(AstIdsBuilder::new());

        debug_assert_eq!(ast_id_scope, scope_id);
        self.scope_stack.push(scope_id);

        if let Some(definition_key) = node.definition_key {
            self.scopes_by_definition.insert(definition_key, scope_id);
        }
    }

    fn pop_scope(&mut self) -> FileScopeId {
        let id = self.scope_stack.pop().expect("Root scope to be present");
        let children_end = self.scopes.next_index();
        let scope = &mut self.scopes[id];
        scope.descendents = scope.descendents.start..children_end;
        id
    }

    fn current_symbol_table(&mut self) -> &mut SymbolTableBuilder {
        let scope_id = self.current_scope();
        &mut self.symbol_tables[scope_id]
    }

    fn current_ast_ids(&mut self) -> &mut AstIdsBuilder {
        let scope_id = self.current_scope();
        &mut self.ast_ids[scope_id]
    }

    fn add_or_update_symbol(&mut self, name: Name, flags: SymbolFlags) -> ScopedSymbolId {
        let symbol_table = self.current_symbol_table();
        symbol_table.add_or_update_symbol(name, flags, None)
    }

    fn add_or_update_symbol_with_definition(
        &mut self,
        name: Name,
        definition: Definition,
    ) -> ScopedSymbolId {
        let symbol_table = self.current_symbol_table();

        symbol_table.add_or_update_symbol(name, SymbolFlags::IS_DEFINED, Some(definition))
    }

    fn with_type_params(
        &mut self,
        name: Name,
        with_params: &WithTypeParams,
        nested: impl FnOnce(&mut Self) -> FileScopeId,
    ) -> FileScopeId {
        let type_params = with_params.type_parameters();

        if let Some(type_params) = type_params {
            let type_params_id = match with_params {
                WithTypeParams::ClassDef { id, .. } => NodeWithScopeId::ClassTypeParams(*id),
                WithTypeParams::FunctionDef { id, .. } => NodeWithScopeId::FunctionTypeParams(*id),
            };

            self.push_scope(NodeWithScope::new(type_params_id, name));
            for type_param in &type_params.type_params {
                let name = match type_param {
                    ast::TypeParam::TypeVar(ast::TypeParamTypeVar { name, .. }) => name,
                    ast::TypeParam::ParamSpec(ast::TypeParamParamSpec { name, .. }) => name,
                    ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple { name, .. }) => name,
                };
                self.add_or_update_symbol(Name::new(name), SymbolFlags::IS_DEFINED);
            }
        }

        let nested_scope = nested(self);

        if type_params.is_some() {
            self.pop_scope();
        }

        nested_scope
    }

    pub(super) fn build(mut self) -> SemanticIndex {
        let module = self.module;
        self.visit_body(module.suite());

        // Pop the root scope
        self.pop_scope();
        assert!(self.scope_stack.is_empty());

        assert!(self.current_definition.is_none());

        let mut symbol_tables: IndexVec<_, _> = self
            .symbol_tables
            .into_iter()
            .map(|builder| Arc::new(builder.finish()))
            .collect();

        let mut ast_ids: IndexVec<_, _> = self
            .ast_ids
            .into_iter()
            .map(super::ast_ids::AstIdsBuilder::finish)
            .collect();

        self.scopes.shrink_to_fit();
        ast_ids.shrink_to_fit();
        symbol_tables.shrink_to_fit();
        self.scopes_by_expression.shrink_to_fit();

        SemanticIndex {
            symbol_tables,
            scopes: self.scopes,
            ast_ids,
            scopes_by_definition: self.scopes_by_definition,
            scopes_by_expression: self.scopes_by_expression,
        }
    }

    fn visit_expression_with_id(&mut self, expr: &ast::Expr, expression_id: ScopedExpressionId) {
        self.scopes_by_expression
            .insert(NodeKey::from_node(expr), self.current_scope());

        match expr {
            ast::Expr::Name(ast::ExprName { id, ctx, .. }) => {
                let flags = match ctx {
                    ast::ExprContext::Load => SymbolFlags::IS_USED,
                    ast::ExprContext::Store => SymbolFlags::IS_DEFINED,
                    ast::ExprContext::Del => SymbolFlags::IS_DEFINED,
                    ast::ExprContext::Invalid => SymbolFlags::empty(),
                };
                match self.current_definition {
                    Some(definition) if flags.contains(SymbolFlags::IS_DEFINED) => {
                        self.add_or_update_symbol_with_definition(Name::new(id), definition);
                    }
                    _ => {
                        self.add_or_update_symbol(Name::new(id), flags);
                    }
                }

                walk_expr(self, expr);
            }
            ast::Expr::Named(node) => {
                debug_assert!(self.current_definition.is_none());

                self.current_definition = Some(Definition::NamedExpr(expression_id));
                // TODO walrus in comprehensions is implicitly nonlocal
                self.visit_expr(&node.target);
                self.current_definition = None;
                self.visit_expr(&node.value);
            }
            ast::Expr::If(ast::ExprIf {
                body, test, orelse, ..
            }) => {
                // TODO detect statically known truthy or falsy test (via type inference, not naive
                // AST inspection, so we can't simplify here, need to record test expression in CFG
                // for later checking)

                self.visit_expr(test);

                // let if_branch = self.flow_graph_builder.add_branch(self.current_flow_node());

                // self.set_current_flow_node(if_branch);
                // self.insert_constraint(test);
                self.visit_expr(body);

                // let post_body = self.current_flow_node();

                // self.set_current_flow_node(if_branch);
                self.visit_expr(orelse);

                // let post_else = self
                //     .flow_graph_builder
                //     .add_phi(self.current_flow_node(), post_body);

                // self.set_current_flow_node(post_else);
            }
            _ => {
                walk_expr(self, expr);
            }
        }
    }
}

impl Visitor<'_> for SemanticIndexBuilder<'_> {
    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        let module = self.module;

        match stmt {
            ast::Stmt::FunctionDef(function_def) => {
                #[allow(unsafe_code)]
                let function_id = unsafe {
                    // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
                    // the current statement must be a child of `module`.
                    self.current_ast_ids()
                        .record_function_definition(function_def, module)
                };

                let name = Name::new(&function_def.name.id);

                self.add_or_update_symbol_with_definition(
                    name.clone(),
                    Definition::FunctionDef(function_id),
                );

                for decorator in &function_def.decorator_list {
                    self.visit_decorator(decorator);
                }

                let scope = self.current_scope();

                self.with_type_params(
                    name.clone(),
                    &WithTypeParams::FunctionDef {
                        node: function_def,
                        id: AstId::new(scope, function_id),
                    },
                    |builder| {
                        builder.visit_parameters(&function_def.parameters);
                        for expr in &function_def.returns {
                            builder.visit_annotation(expr);
                        }

                        builder.push_scope(NodeWithScope::with_definition(
                            NodeWithScopeId::Function(AstId::new(scope, function_id)),
                            name,
                            function_def,
                        ));
                        builder.visit_body(&function_def.body);
                        builder.pop_scope()
                    },
                );
            }
            ast::Stmt::ClassDef(class) => {
                #[allow(unsafe_code)]
                let class_id = unsafe {
                    // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
                    // the current statement must be a child of `module`.
                    self.current_ast_ids()
                        .record_class_definition(class, module)
                };

                let name = Name::new(&class.name.id);
                self.add_or_update_symbol_with_definition(name.clone(), Definition::from(class_id));

                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }

                let scope = self.current_scope();
                self.with_type_params(
                    name.clone(),
                    &WithTypeParams::ClassDef {
                        node: class,
                        id: AstId::new(scope, class_id),
                    },
                    |builder| {
                        if let Some(arguments) = &class.arguments {
                            builder.visit_arguments(arguments);
                        }

                        builder.push_scope(NodeWithScope::with_definition(
                            NodeWithScopeId::Class(AstId::new(scope, class_id)),
                            name,
                            class,
                        ));
                        builder.visit_body(&class.body);

                        builder.pop_scope()
                    },
                );
            }
            ast::Stmt::Import(ast::StmtImport { names, .. }) => {
                let scope_id = self.current_scope();
                for alias in names {
                    let symbol_name = if let Some(asname) = &alias.asname {
                        asname.id.as_str()
                    } else {
                        alias.name.id.split('.').next().unwrap()
                    };

                    #[allow(unsafe_code)]
                    let alias_id = unsafe {
                        // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
                        // the current statement must be a child of `module`.
                        self.current_ast_ids().record_alias(alias, module)
                    };

                    let def = Definition::ImportAlias(alias_id);
                    self.add_or_update_symbol_with_definition(Name::new(symbol_name), def);
                    self.scopes_by_definition.insert(alias.into(), scope_id);
                }
            }
            ast::Stmt::ImportFrom(ast::StmtImportFrom {
                module: _,
                names,
                level: _,
                ..
            }) => {
                let scope_id = self.current_scope();

                for alias in names {
                    let symbol_name = if let Some(asname) = &alias.asname {
                        asname.id.as_str()
                    } else {
                        alias.name.id.as_str()
                    };

                    #[allow(unsafe_code)]
                    let alias_id = unsafe {
                        // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
                        // the current statement must be a child of `module`.
                        self.current_ast_ids().record_alias(alias, module)
                    };

                    let def = Definition::ImportAlias(alias_id);
                    self.add_or_update_symbol_with_definition(Name::new(symbol_name), def);
                    self.scopes_by_definition.insert(alias.into(), scope_id);
                }
            }
            ast::Stmt::Assign(node) => {
                debug_assert!(self.current_definition.is_none());

                self.visit_expr(&node.value);
                for target in &node.targets {
                    #[allow(unsafe_code)]
                    let expression_id = unsafe {
                        // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
                        // the current expression must be a child of `module`.
                        self.current_ast_ids().record_expression(target, module)
                    };

                    self.current_definition = Some(Definition::Target(expression_id));
                    self.visit_expression_with_id(target, expression_id);
                    self.current_definition = None;
                }
            }
            _ => {
                walk_stmt(self, stmt);
            }
        }
    }

    fn visit_expr(&mut self, expr: &'_ ast::Expr) {
        let module = self.module;
        #[allow(unsafe_code)]
        let expression_id = unsafe {
            // SAFETY: The builder only visits nodes that are part of `module`. This guarantees that
            // the current expression must be a child of `module`.
            self.current_ast_ids().record_expression(expr, module)
        };

        self.visit_expression_with_id(expr, expression_id);
    }
}

enum WithTypeParams<'a> {
    ClassDef {
        node: &'a ast::StmtClassDef,
        id: AstId<ScopedClassId>,
    },
    FunctionDef {
        node: &'a ast::StmtFunctionDef,
        id: AstId<ScopedFunctionId>,
    },
}

impl<'a> WithTypeParams<'a> {
    fn type_parameters(&self) -> Option<&'a ast::TypeParams> {
        match self {
            WithTypeParams::ClassDef { node, .. } => node.type_params.as_deref(),
            WithTypeParams::FunctionDef { node, .. } => node.type_params.as_deref(),
        }
    }
}

struct NodeWithScope {
    id: NodeWithScopeId,
    definition_key: Option<DefinitionNodeKey>,
    name: Name,
}

impl NodeWithScope {
    fn new(id: NodeWithScopeId, name: Name) -> Self {
        Self {
            id,
            definition_key: None,
            name,
        }
    }

    fn with_definition(id: NodeWithScopeId, name: Name, key: impl Into<DefinitionNodeKey>) -> Self {
        Self {
            id,
            definition_key: Some(key.into()),
            name,
        }
    }

    fn scope_kind(&self) -> ScopeKind {
        match self.id {
            NodeWithScopeId::Module => ScopeKind::Module,
            NodeWithScopeId::Class(_) => ScopeKind::Class,
            NodeWithScopeId::Function(_) => ScopeKind::Function,
            NodeWithScopeId::ClassTypeParams(_) | NodeWithScopeId::FunctionTypeParams(_) => {
                ScopeKind::Annotation
            }
        }
    }
}
