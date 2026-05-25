use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::SourceSpan;
use crate::ast::{AstNodeId, Identifier, *};
use crate::diagnostic::{Diagnostic, OORVError};
use crate::oorvir::source::FuncDecl;
use crate::oorvir::source::ValueTyped;

/// A flat mapping from every AST node id to the symbol it resolves to.
pub(crate) type SymbolTable = HashMap<AstNodeId, BoundSymbol>;

/// Resolves identifiers to their definitions and enforces scope rules.
#[derive(Debug)]
pub struct SymbolResolver {
    /// Scope stack for value bindings (signals, constraints, locals, params).
    value_ns: NamespaceStack,
    /// Scope stack for type names (primitive and user-defined types).
    type_ns: NamespaceStack,
    /// Scope stack for function / callable names.
    func_ns: NamespaceStack,
    /// Accumulated resolution results.
    resolved: SymbolTable,
    /// Active quantifier domain bindings, most-recent last.
    quant_domains: Vec<HashMap<String, String>>,
}

/// Language keywords that are off-limits as user-defined identifiers.
pub(crate) const RESERVED_WORDS: [&str; 42] = [
    "type",
    "self",
    "include",
    "use",
    "when",
    "end",
    "with",
    "unless",
    "if",
    "then",
    "else",
    "let",
    "and",
    "or",
    "not",
    "forall",
    "exists",
    "any",
    "true",
    "always",
    "false",
    "error",
    "module",
    "world",
    "class",
    "extends",
    "function",
    "fun",
    "constant",
    "const",
    "signals",
    "signal",
    "constraints",
    "constraint",
    "constrain",
    "global",
    "local",
    "info",
    "alert",
    "violation",
    "override",
    "pre",
];

impl SymbolResolver {
    /// Constructs a fresh resolver pre-loaded with built-in primitive types.
    pub fn new() -> Self {
        let mut type_stack = NamespaceStack::root();

        // Pre-register every primitive type so type annotations resolve immediately.
        for (prim_name, _) in ValueTyped::primitive_types() {
            type_stack.put(prim_name, BoundSymbol::ValueType);
        }

        // Open a second scope so built-in names are distinguished from user ones.
        type_stack.push_scope();

        SymbolResolver {
            value_ns: NamespaceStack::root(),
            type_ns: type_stack,
            func_ns: NamespaceStack::root(),
            resolved: HashMap::new(),
            quant_domains: Vec::new(),
        }
    }

    /// Registers a non-type symbol in the current value scope, checking for
    fn register_symbol(&mut self, sym: BoundSymbol) -> Result<(), OORVError> {
        assert!(!sym.is_type_marker());
        let mut errs = OORVError::new();

        let sym_name = sym
            .identifier()
            .expect("every registered symbol must have a name");
        let sym_span = sym
            .source_span()
            .expect("every user symbol must carry a source span");

        // Reject identifiers that collide with language keywords.
        let normalized = sym_name.to_lowercase();
        if RESERVED_WORDS.contains(&normalized.as_str()) {
            errs.add(
                Diagnostic::error(&format!(
                    "`{sym_name}` is a reserved word and cannot be used as an identifier"
                ))
                .add_span_with_label(
                    sym_span,
                    Some("choose a different name"),
                    true,
                ),
            );
        }

        // Silently discard the anonymous wildcard.
        if normalized == "_" {
            return Ok(());
        }

        let lookup_key = match &sym {
            BoundSymbol::Func(rf) => SymbolKey::Func(rf.name.clone()),
            _ => SymbolKey::Name(sym_name.to_string()),
        };

        if let Some(earlier) = self.value_ns.find_in_top(&lookup_key) {
            errs.add(
                Diagnostic::error(&format!(
                    "`{sym_name}` is declared more than once in this scope"
                ))
                .add_span_with_label(
                    sym_span,
                    Some(&format!("`{sym_name}` re-declared here")),
                    true,
                )
                .maybe_add_span_with_label(
                    earlier.source_span(),
                    Some(&format!("earlier declaration of `{sym_name}` here")),
                    false,
                ),
            );
        } else {
            self.value_ns.put(sym_name, sym.clone());
        }

        errs.into()
    }

    /// Verifies that a type reference resolves to a known type name.
    fn check_type_bound(&mut self, ty: &ValueType) -> Result<(), OORVError> {
        let mut errs = OORVError::new();
        match &ty.kind {
            ValueTypeKind::Named(type_name) => {
                match self.type_ns.find(&SymbolKey::Name(type_name.to_string())) {
                    Some(entry) => {
                        assert!(entry.is_type_marker());
                        self.resolved.insert(ty.node_id, entry);
                    }
                    None => {
                        errs.add(
                            Diagnostic::error(&format!("type `{type_name}` is not in scope"))
                                .add_span_with_label(
                                    ty.span,
                                    Some("referenced here but not defined"),
                                    true,
                                ),
                        );
                    }
                }
            }
            ValueTypeKind::Tuple(inner) => {
                inner.iter().for_each(|elem| {
                    if let Err(e) = self.check_type_bound(elem) {
                        errs.join(e);
                    }
                });
            }
            ValueTypeKind::Optional(wrapped) => {
                if let Err(e) = self.check_type_bound(wrapped) {
                    errs.join(e);
                }
            }
        }
        errs.into()
    }

    /// the current parameter list and verifies any type annotation.
    fn check_param_entry(&mut self, param: &Rc<ParamDecl>) -> Result<(), OORVError> {
        let mut errs = OORVError::new();
        let param_key = SymbolKey::Name(param.name.name.clone());

        if let Some(prior) = self.value_ns.find_in_top(&param_key) {
            errs.add(
                Diagnostic::error(&format!(
                    "parameter `{}` appears more than once in this list",
                    param.name.name
                ))
                .add_span_with_label(
                    param.span,
                    Some(&format!("`{}` repeated here", param.name.name)),
                    true,
                )
                .add_span_with_label(
                    prior
                        .source_span()
                        .expect("parameter symbols always carry spans"),
                    Some(&format!("first occurrence of `{}` here", param.name)),
                    false,
                ),
            );
        } else {
            let entry = BoundSymbol::Param(param.clone());
            if let Err(e) = self.register_symbol(entry.clone()) {
                errs.join(e);
            }
            self.resolved.insert(param.node_id, entry);
        }

        if let Some(ann) = param.annotation.as_ref() {
            if let Err(e) = self.check_type_bound(ann) {
                errs.join(e);
            }
        }

        errs.into()
    }

    /// Main entry point: walks an entire specification and returns the completed
    /// symbol table, or a collected set of errors.
    pub(crate) fn analyze_bindings(&mut self, spec: &OORVAst) -> Result<SymbolTable, OORVError> {
        use crate::oorvir::source::builtins as stdlib;
        use std::collections::HashMap as Map;

        let mut errs = OORVError::new();

        // Build a name -> ValueTyped map of primitives for function signature analysis.
        let prim_lookup: Map<String, ValueTyped> = ValueTyped::primitive_types()
            .into_iter()
            .map(|(n, vt)| (n.to_string(), vt.clone()))
            .collect();

        // Converts an optional AST type annotation to a ValueTyped, substituting
        // known generic parameters by their positional index.
        let resolve_ast_type =
            |t_opt: &Option<ValueType>, generic_map: &Map<String, usize>| -> ValueTyped {
                let Some(t) = t_opt else {
                    return ValueTyped::Any;
                };
                match &t.kind {
                    crate::ast::ValueTypeKind::Named(n) => {
                        if let Some(prim) = prim_lookup.get(n.as_str()) {
                            return prim.clone();
                        }
                        // A name made entirely of uppercase ASCII letters/digits is a generic.
                        let is_generic = n
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
                        if is_generic {
                            return generic_map
                                .get(n.as_str())
                                .map(|&i| ValueTyped::Param(i, n.clone()))
                                .unwrap_or(ValueTyped::Any);
                        }
                        ValueTyped::Any
                    }
                    _ => ValueTyped::Any,
                }
            };

        // Register user-defined functions so their names are visible in constraint bodies.
        for func_def in &spec.functions {
            // All parameters are treated as positional (no label in the source_ir FuncLabel).
            let label_slots: Vec<Option<String>> = func_def.params.iter().map(|_| None).collect();
            let func_label =
                crate::oorvir::source::FuncLabel::new(func_def.name.name.clone(), &label_slots);

            if let Some(existing) = self.func_ns.find(&SymbolKey::Func(func_label.clone())) {
                errs.add(
                    Diagnostic::error(&format!(
                        "function `{}` is declared more than once",
                        func_label.name()
                    ))
                    .add_span_with_label(
                        func_def.name.span,
                        Some("duplicate declaration here"),
                        true,
                    )
                    .maybe_add_span_with_label(
                        existing.source_span(),
                        Some("earlier declaration here"),
                        false,
                    ),
                );
            } else {
                // Collect generic type-variable names from the signature.
                let mut generic_order: Vec<String> = Vec::new();
                let mut generic_positions: Map<String, usize> = Map::new();

                let mut note_generic = |ann: &Option<ValueType>| {
                    if let Some(t) = ann {
                        if let crate::ast::ValueTypeKind::Named(n) = &t.kind {
                            let is_generic = !prim_lookup.contains_key(n.as_str())
                                && n.chars()
                                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
                            if is_generic && !generic_positions.contains_key(n.as_str()) {
                                let pos = generic_order.len();
                                generic_positions.insert(n.clone(), pos);
                                generic_order.push(n.clone());
                            }
                        }
                    }
                };

                for p in &func_def.params {
                    note_generic(&p.annotation);
                }
                note_generic(&func_def.return_type);

                let typed_params: Vec<ValueTyped> = func_def
                    .params
                    .iter()
                    .map(|p| resolve_ast_type(&p.annotation, &generic_positions))
                    .collect();

                let ret_type = resolve_ast_type(&func_def.return_type, &generic_positions);
                let type_params: Vec<ValueTyped> =
                    generic_order.iter().map(|_| ValueTyped::Any).collect();

                let built_decl = FuncDecl {
                    name: func_label,
                    type_params,
                    params: crate::oorvir::source::ParameterDecl::FixedAmount(typed_params),
                    return_ty: ret_type,
                };

                self.func_ns.put_func(&built_decl);
            }
        }

        // Load built-in standard-library functions.
        self.func_ns.put_all_funcs(stdlib::implicit_module());
        self.func_ns.put_all_funcs(stdlib::math_module());

        // Register global constants.
        for cst in &spec.constants {
            if let Err(e) = self.register_symbol(BoundSymbol::Const(cst.clone())) {
                errs.join(e);
            }
            if let Some(ann) = cst.annotation.as_ref() {
                if let Err(e) = self.check_type_bound(ann) {
                    errs.join(e);
                }
            }
        }

        // Register signals (inputs) and verify their type annotations.
        for sig in &spec.signals {
            if let Err(e) = self.register_symbol(BoundSymbol::Signal(sig.clone())) {
                errs.join(e);
            }
            if let Err(e) = self.check_type_bound(&sig.annotation) {
                errs.join(e);
            }

            // Parameters on signals get their own nested scope.
            self.value_ns.push_scope();
            let param_errs: OORVError = sig
                .params
                .iter()
                .flat_map(|p| self.check_param_entry(p).err())
                .flatten()
                .collect();
            errs.join(param_errs);
            self.value_ns.pop_scope();
        }

        // Register constraint outputs (non-alarm ones only).
        for out in &spec.constrains {
            if out.kind != ConstrainKind::Alarm {
                let sym = if out.params.is_empty() {
                    BoundSymbol::Constraint(out.clone())
                } else {
                    BoundSymbol::ParamOut(out.clone())
                };
                if let Err(e) = self.register_symbol(sym) {
                    errs.join(e);
                }
            }
            if let Some(ann) = out.annotation.as_ref() {
                if let Err(e) = self.check_type_bound(ann) {
                    errs.join(e);
                }
            }
        }

        // Walk constraint bodies (start / eval / end expressions).
        if let Err(e) = self.inspect_constraint_bodies(spec) {
            errs.join(e);
        }

        // Walk function bodies.
        for func_def in &spec.functions {
            self.value_ns.push_scope();

            for p in &func_def.params {
                let entry = BoundSymbol::Param(p.clone());
                if let Err(e) = self.register_symbol(entry.clone()) {
                    errs.join(e);
                }
                self.resolved.insert(p.node_id, entry);
                if let Some(ann) = p.annotation.as_ref() {
                    if let Err(e) = self.check_type_bound(ann) {
                        errs.join(e);
                    }
                }
            }

            // Process statements: validate RHS of let-bindings before adding
            // the new name to scope, so self-referential lets are caught.
            for stmt in &func_def.body.decls {
                match stmt {
                    MethodStmt::Let(binding) => {
                        if let Err(e) = self.scan_expr(&binding.expr) {
                            errs.join(e);
                        }
                        let local = BoundSymbol::Local(binding.name.clone());
                        if let Err(e) = self.register_symbol(local) {
                            errs.join(e);
                        }
                    }
                    MethodStmt::Expr(expr) => {
                        if let Err(e) = self.scan_expr(expr) {
                            errs.join(e);
                        }
                    }
                }
            }

            if let Some(ret_expr) = &func_def.body.ret {
                if let Err(e) = self.scan_expr(ret_expr) {
                    errs.join(e);
                }
            }

            self.value_ns.pop_scope();
        }

        Result::from(errs)?;
        Ok(self.resolved.clone())
    }

    /// Validates the start/eval/end sub-expressions for all constraint outputs.
    fn inspect_constraint_bodies(&mut self, spec: &OORVAst) -> Result<(), OORVError> {
        let mut errs = OORVError::new();

        for out in &spec.constrains {
            self.value_ns.push_scope();

            // Validate any parameters declared on this constraint.
            let param_errs: OORVError = out
                .params
                .iter()
                .flat_map(|p| self.check_param_entry(p).err())
                .flatten()
                .collect();
            errs.join(param_errs);

            // Validate start clause.
            if let Some(start_node) = &out.start {
                if let Some(init_expr) = &start_node.expression {
                    if let Err(e) = self.scan_expr(init_expr) {
                        errs.join(e);
                    }
                }
                let pacing_err = match &start_node.pacing {
                    ast::PacingNode::NotAnnotated(_) => Ok(()),
                    ast::PacingNode::Global(e)
                    | ast::PacingNode::Local(e)
                    | ast::PacingNode::Unspecified(e) => self.scan_expr(e),
                };
                if let Err(e) = pacing_err {
                    errs.join(e);
                }
                if let Some(cond) = &start_node.condition {
                    if let Err(e) = self.scan_expr(cond) {
                        errs.join(e);
                    }
                }
            }

            // Validate end clause.
            if let Some(end_node) = &out.end {
                if let Err(e) = self.scan_expr(&end_node.condition) {
                    errs.join(e);
                }
                let pacing_err = match &end_node.pacing {
                    ast::PacingNode::NotAnnotated(_) => Ok(()),
                    ast::PacingNode::Global(e)
                    | ast::PacingNode::Local(e)
                    | ast::PacingNode::Unspecified(e) => self.scan_expr(e),
                };
                if let Err(e) = pacing_err {
                    errs.join(e);
                }
            }

            // Validate eval clauses (pacing and filter/condition parts first).
            for eval_clause in &out.eval {
                let pacing_err = match &eval_clause.pacing {
                    ast::PacingNode::NotAnnotated(_) => Ok(()),
                    ast::PacingNode::Global(e)
                    | ast::PacingNode::Local(e)
                    | ast::PacingNode::Unspecified(e) => self.scan_expr(e),
                };
                if let Err(e) = pacing_err {
                    errs.join(e);
                }
                if let Some(eval_cond) = &eval_clause.condition {
                    if let Err(e) = self.scan_expr(eval_cond) {
                        errs.join(e);
                    }
                }
            }

            // Make `self` visible for the expression part of eval clauses.
            if out.kind != ConstrainKind::Alarm {
                self.value_ns
                    .put("self", BoundSymbol::Constraint(out.clone()));
            }

            for eval_clause in &out.eval {
                if let Some(body) = &eval_clause.expression {
                    if let Err(e) = self.scan_expr(body) {
                        errs.join(e);
                    }
                }
            }

            self.value_ns.pop_scope();
        }

        errs.into()
    }

    /// Resolves a plain identifier reference and records it in the symbol table.
    fn bind_variable_ref(&mut self, node: &ExprNode, ident: &Identifier) -> Result<(), Diagnostic> {
        let key = SymbolKey::Name(ident.name.clone());

        if let Some(sym) = self.value_ns.find(&key) {
            assert!(!sym.is_type_marker());
            self.resolved.insert(node.node_id, sym);
            return Ok(());
        }

        // Try rewriting the identifier using active quantifier domain bindings.
        for domain_scope in self.quant_domains.iter().rev() {
            for (binding, domain) in domain_scope.iter() {
                let prefix = format!("{binding}::");
                if let Some(suffix) = ident.name.strip_prefix(&prefix) {
                    let rewritten = format!("{domain}::{suffix}");
                    if let Some(sym) = self.value_ns.find(&SymbolKey::Name(rewritten)) {
                        assert!(!sym.is_type_marker());
                        self.resolved.insert(node.node_id, sym);
                        return Ok(());
                    }
                }
            }
        }

        Err(Diagnostic::error(&format!(
            "identifier `{}` cannot be found in the current scope",
            &ident.name
        ))
        .add_span_with_label(ident.span, Some("not defined here"), true))
    }

    /// Resolves a function or callable reference and records it in the symbol table.
    fn bind_callable_ref(&mut self, node: &ExprNode, label: &FuncLabel) -> Result<(), Diagnostic> {
        let display = label.to_string();

        if let Some(sym) = self.func_ns.find(&SymbolKey::Func(label.clone().into())) {
            assert!(sym.is_callable());
            self.resolved.insert(node.node_id, sym);
            return Ok(());
        }

        // A parameterized constraint behaves like a function application.
        if let Some(BoundSymbol::ParamOut(out)) = self
            .value_ns
            .find(&SymbolKey::Name(label.name.name.clone()))
        {
            self.resolved
                .insert(node.node_id, BoundSymbol::ParamOut(out));
            return Ok(());
        }

        Err(Diagnostic::error(&format!(
            "callable `{display}` is not defined in the current scope"
        ))
        .add_span_with_label(label.name.span, Some("not found"), true))
    }

    /// Recursively traverses an expression node, resolving all identifiers.
    fn scan_expr(&mut self, node: &ExprNode) -> Result<(), OORVError> {
        use crate::ast::ExprVariant::*;

        match &node.kind {
            Identifier(id) => self.bind_variable_ref(node, id).map_err(OORVError::from),

            SignalAccess(inner, _) | Shift(inner, _) => self.scan_expr(inner),

            Binary(_, lhs, rhs) => Err(OORVError::combine(
                self.scan_expr(lhs),
                self.scan_expr(rhs),
                |_, _| {},
            )),

            Literal(_) | MissingExpr => Ok(()),

            Ite(cond, then_branch, else_branch) => [cond, then_branch, else_branch]
                .iter()
                .flat_map(|e| self.scan_expr(e).err())
                .flatten()
                .collect::<OORVError>()
                .into(),

            Bracket(inner) | Unary(_, inner) | Field(inner, _) => self.scan_expr(inner),

            Tuple(elements) => elements
                .iter()
                .flat_map(|e| self.scan_expr(e).err())
                .flatten()
                .collect::<OORVError>()
                .into(),

            Function(label, type_args, arg_exprs) => {
                let callable_errs: OORVError = self
                    .bind_callable_ref(node, label)
                    .map_err(OORVError::from)
                    .into();
                let ty_errs: OORVError = type_args
                    .iter()
                    .flat_map(|ty| self.check_type_bound(ty).err())
                    .flatten()
                    .collect();
                let arg_errs: OORVError = arg_exprs
                    .iter()
                    .flat_map(|e| self.scan_expr(e).err())
                    .flatten()
                    .collect();
                callable_errs
                    .into_iter()
                    .chain(ty_errs)
                    .chain(arg_errs)
                    .collect::<OORVError>()
                    .into()
            }

            Default(base, fallback) => Err(OORVError::combine(
                self.scan_expr(base),
                self.scan_expr(fallback),
                |_, _| {},
            )),

            Method(receiver, label, type_args, extra_args) => {
                // Treat method call as a function with the receiver prepended.
                let call_label = FuncLabel {
                    name: label.name.clone(),
                    arg_names: std::iter::once(None)
                        .chain(label.arg_names.clone())
                        .collect(),
                };
                let callable_errs: OORVError = self
                    .bind_callable_ref(node, &call_label)
                    .map_err(OORVError::from)
                    .into();
                let ty_errs: OORVError = type_args
                    .iter()
                    .flat_map(|ty| self.check_type_bound(ty).err())
                    .flatten()
                    .collect();
                let recv_errs: OORVError = self.scan_expr(receiver).into();
                let arg_errs: OORVError = extra_args
                    .iter()
                    .flat_map(|e| self.scan_expr(e).err())
                    .flatten()
                    .collect();
                callable_errs
                    .into_iter()
                    .chain(ty_errs)
                    .chain(recv_errs)
                    .chain(arg_errs)
                    .collect::<OORVError>()
                    .into()
            }

            Quantified(_quant, var_idents, domain_idents, body) => {
                let mut errs = OORVError::new();
                self.value_ns.push_scope();
                self.quant_domains.push(HashMap::new());

                let mut registered: HashSet<String> = HashSet::new();
                for (var, domain) in var_idents.iter().zip(domain_idents.iter()) {
                    if registered.insert(var.name.clone()) {
                        let qvar = Rc::new(crate::ast::Identifier::new(var.name.clone(), var.span));
                        if let Err(e) = self.register_symbol(BoundSymbol::QuantifiedVar(qvar)) {
                            errs.join(e);
                        }
                        if let Some(top) = self.quant_domains.last_mut() {
                            top.insert(var.name.clone(), domain.name.clone());
                        }
                    }
                }

                if let Err(e) = self.scan_expr(body) {
                    errs.join(e);
                }

                self.quant_domains.pop();
                self.value_ns.pop_scope();
                errs.into()
            }
        }
    }
}

/// A stack of scopes, each holding a flat map of symbol bindings.
/// The innermost (most-recently pushed) scope is searched first.
#[derive(Debug)]
pub(crate) struct NamespaceStack {
    frames: Vec<HashMap<SymbolKey, BoundSymbol>>,
}

impl NamespaceStack {
    /// Creates a new stack with a single empty root scope.
    fn root() -> Self {
        NamespaceStack {
            frames: vec![HashMap::new()],
        }
    }

    /// Pushes a new empty scope onto the stack.
    fn push_scope(&mut self) {
        self.frames.push(HashMap::new());
    }

    /// Removes the innermost scope. Panics if only the root scope remains.
    fn pop_scope(&mut self) {
        assert!(self.frames.len() > 1, "attempted to pop the root scope");
        self.frames.pop();
    }

    /// Looks up a key by searching from the innermost scope outward.
    fn find(&self, key: &SymbolKey) -> Option<BoundSymbol> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.get(key).cloned())
    }

    /// Looks up a key only in the topmost (innermost) scope.
    fn find_in_top(&self, key: &SymbolKey) -> Option<BoundSymbol> {
        self.frames
            .last()
            .expect("namespace stack is empty")
            .get(key)
            .cloned()
    }

    /// Inserts a value symbol under the given name into the current scope.
    /// Caller is responsible for duplicate checking before calling this.
    fn put(&mut self, name: &str, sym: BoundSymbol) {
        self.frames
            .last_mut()
            .expect("namespace stack is empty")
            .insert(SymbolKey::Name(name.to_string()), sym);
    }

    /// Inserts a function declaration into the current scope.
    /// Caller is responsible for duplicate checking before calling this.
    pub(crate) fn put_func(&mut self, decl: &FuncDecl) {
        self.frames
            .last_mut()
            .expect("namespace stack is empty")
            .insert(
                SymbolKey::Func(decl.name.clone()),
                BoundSymbol::Func(Rc::new(decl.clone())),
            );
    }

    /// Inserts a slice of function declarations into the current scope.
    pub(crate) fn put_all_funcs(&mut self, decls: Vec<&FuncDecl>) {
        decls.into_iter().for_each(|d| self.put_func(d));
    }
}

// ---------------------------------------------------------------------------
// BoundSymbol -- the resolved declaration kind
// ---------------------------------------------------------------------------

/// Every distinct category of name that can appear in an OORV specification.
#[derive(Debug, Clone)]
pub(crate) enum BoundSymbol {
    /// A declared constant.
    Const(Rc<ConstDecl>),
    /// An input signal stream.
    Signal(Rc<Signal>),
    /// A non-parameterised constraint output.
    Constraint(Rc<Constrain>),
    /// A parameterised constraint output (acts like a function at call sites).
    ParamOut(Rc<Constrain>),
    /// A local `let` binding inside a function body.
    Local(Identifier),
    /// A built-in or user-declared value type.
    ValueType,
    /// A formal parameter in a function or constraint header.
    Param(Rc<ParamDecl>),
    /// A user-defined function.
    Func(Rc<FuncDecl>),
    /// A variable bound by a quantifier (`forall`/`exists`).
    QuantifiedVar(Rc<ast::Identifier>),
}

/// The key type used inside [`NamespaceStack`] to disambiguate functions from
/// plain identifiers sharing the same base name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum SymbolKey {
    /// A function, identified by its full label (name + arity).
    Func(crate::oorvir::source::FuncLabel),
    /// Any non-function identifier.
    Name(String),
}

impl BoundSymbol {
    /// Returns the source span associated with this symbol, if any.
    fn source_span(&self) -> Option<SourceSpan> {
        match self {
            BoundSymbol::Const(c) => Some(c.name.span),
            BoundSymbol::Signal(s) => Some(s.span),
            BoundSymbol::Constraint(o) | BoundSymbol::ParamOut(o) => o.name().map(|n| n.span),
            BoundSymbol::Local(id) => Some(id.span),
            BoundSymbol::Param(p) => Some(p.span),
            BoundSymbol::ValueType | BoundSymbol::Func(_) => None,
            BoundSymbol::QuantifiedVar(id) => Some(id.span),
        }
    }

    /// Returns the plain string identifier for this symbol, if it has one.
    fn identifier(&self) -> Option<&str> {
        match self {
            BoundSymbol::Const(c) => Some(&c.name.name),
            BoundSymbol::Signal(s) => Some(&s.name.name),
            BoundSymbol::Constraint(o) | BoundSymbol::ParamOut(o) => {
                o.name().map(|n| n.name.as_str())
            }
            BoundSymbol::Local(id) => Some(&id.name),
            BoundSymbol::Param(p) => Some(&p.name.name),
            BoundSymbol::ValueType | BoundSymbol::Func(_) => None,
            BoundSymbol::QuantifiedVar(id) => Some(&id.name),
        }
    }

    /// Returns `true` if this symbol represents a type rather than a value.
    fn is_type_marker(&self) -> bool {
        matches!(self, BoundSymbol::ValueType)
    }

    /// Returns `true` if this symbol can appear in call-expression position.
    fn is_callable(&self) -> bool {
        matches!(self, BoundSymbol::Func(_) | BoundSymbol::ParamOut(_))
    }
}

impl From<FuncLabel> for crate::oorvir::source::FuncLabel {
    fn from(f: FuncLabel) -> Self {
        crate::oorvir::source::FuncLabel::new(
            f.name.name,
            &f.arg_names
                .iter()
                .map(|op| op.clone().map(|ident| ident.name))
                .collect::<Vec<_>>(),
        )
    }
}

use crate::ast::{
    self, AccessMode, FuncLabel, OORVAst, StartDecl, TokenLiteral as AstLiteral, ValueType,
};
use serde::{Deserialize, Serialize};

use crate::oorvir::source::{
    AccessMode as IRAccess, Constant as IrConstant, Constraint, ConstraintKind, EndNode, EvalNode,
    ExprNodeIdx, ExprVariant, Expression, ExpressionRegistry, FnExprKind, Inlined, Literal,
    OORVIr1, PacingNode as SourcePacingNode, ParamDecl as SourceParamDecl, Shift,
    Signal as SourceSignal, StartNode, StreamIdx, TimedFrequency, WidenExprKind,
};

impl OORVIr1 {
    pub(crate) fn from_ast(ast: OORVAst) -> Result<Self, OORVError> {
        let mut sym_resolver = SymbolResolver::new();
        let binding_map = sym_resolver.analyze_bindings(&ast)?;
        let func_registry: HashMap<String, FuncDecl> = binding_map
            .values()
            .filter(|decl| matches!(decl, BoundSymbol::Func(_)))
            .map(|decl| {
                if let BoundSymbol::Func(fun_decl) = decl {
                    (fun_decl.name.name().to_owned(), (**fun_decl).clone())
                } else {
                    unreachable!("assured by filter")
                }
            })
            .collect();

        let mut stream_index_map = HashMap::new();

        for (index, constraint_def) in ast.constrains.iter().enumerate() {
            let si = StreamIdx::Constraint(index);
            if let Some(name) = constraint_def.name() {
                stream_index_map.insert(name.name.clone(), si);
            }
        }
        for (index, signal) in ast.signals.iter().enumerate() {
            let si = StreamIdx::Signal(index);
            stream_index_map.insert(signal.name.name.clone(), si);
        }
        let stream_index_map = stream_index_map;
        IrBuilder::build(binding_map, stream_index_map, ast, func_registry)
            .map_err(|e| e.into_diagnostic().into())
    }
}

/// Describes all error conditions that can arise while lowering the OORV AST to its source_ir form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoweringError {
    /// An identifier resolved to a function where a stream reference was required.
    StreamExpectedGotFunc(SourceSpan, FuncLabel),
    /// The expression in stream-reference position does not denote a valid stream.
    BadStreamRef(SourceSpan, String),
    /// A constant declaration is missing its required type annotation.
    UntypedConstant(SourceSpan),
    /// The numeric literal could not be parsed into a valid number.
    LiteralParseFailure(SourceSpan),
    /// The activation condition expression is not a valid frequency.
    BadFrequencyAnnotation(SourceSpan, String),
    /// A realtime shift offset could not be interpreted as a frequency.
    BadTimeOffset(SourceSpan),
    /// The window duration could not be converted to the expected type.
    BadWindowDuration(String, SourceSpan),
    /// A required expression is absent and cannot be lowered.
    AbsentExpression(SourceSpan),
    /// A `widen` call is missing its required type argument.
    WidenNeedsTypeArg(SourceSpan),
    /// The annotated type name could not be resolved.
    UnresolvableType(ValueType, String, SourceSpan),
    /// The referenced function is not declared in the current scope.
    UndeclaredCallable(SourceSpan),
    /// A unit postfix was found on a non-time literal, which is not permitted.
    ForbiddenUnitSuffix(SourceSpan),
    /// An access to a parameterised stream omits the required argument list.
    ParametricStreamNoArgs(SourceSpan),
    /// A parameterised stream has no start clause.
    ParametricStreamNoStart(SourceSpan),
    /// A stream has a start clause but no formal parameters.
    StartWithoutParams(SourceSpan),
    /// The parameter count and start-expression count of a stream do not match.
    /// SourceSpan of the output, number of parameters, number of start expressions
    ParamStartCountMismatch(SourceSpan, usize, usize),
    /// Instance aggregation cannot be applied to individual parameterised instances.
    AggregationOnInstance(SourceSpan),
    /// Instance aggregation requires a parameterised stream.
    AggregationOnPlainStream(SourceSpan),
    /// An output stream representing a alarm does not have a eval when condition
    AlarmMissingEvalWhen(SourceSpan),
    /// A local or global pacing annotation must carry a frequency value.
    PacingNotFrequency(SourceSpan),
    /// A local periodic pacing was found on an unstarted (non-dynamic) stream.
    LocalFreqWithoutStart(SourceSpan),
    /// A start clause may not carry a local periodic pacing annotation.
    LocalFreqInStartClause(SourceSpan),
    /// The same tag key appears more than once on a single stream.
    TagUsedTwice(String, SourceSpan, SourceSpan),
    /// Lambda expressions are only allowed inside filtered aggregation contexts.
    LambdaOutsideAggregation(SourceSpan),
    /// The length of a constant tuple literal differs from its annotated tuple type.
    TupleLenMismatch(SourceSpan, usize, usize),
}

impl LoweringError {
    pub(crate) fn into_diagnostic(self) -> Diagnostic {
        let diag = match self {
            LoweringError::StreamExpectedGotFunc(span, name) => {
                Diagnostic::error("an identifier in stream position resolved to a function name")
                    .add_span_with_label(
                        span,
                        Some(&format!("function `{name}` referenced here")),
                        true,
                    )
            }
            LoweringError::BadStreamRef(span, reason) => {
                Diagnostic::error(&format!("not a valid stream reference: {reason}"))
                    .add_span_with_label(span, Some("invalid reference here"), true)
            }
            LoweringError::UntypedConstant(span) => {
                Diagnostic::error("constant requires an explicit type annotation")
                    .add_span_with_label(span, Some("annotation missing here"), true)
            }
            LoweringError::LiteralParseFailure(span) => Diagnostic::error(
                "numeric literal could not be parsed",
            )
            .add_span_with_label(span, Some("bad literal here"), true),
            LoweringError::BadFrequencyAnnotation(span, reason) => {
                Diagnostic::error(&format!("invalid frequency annotation: {reason}"))
                    .add_span_with_label(span, Some("annotation here"), true)
            }
            LoweringError::BadTimeOffset(span) => Diagnostic::error(
                "time offset is not a recognised format",
            )
            .add_span_with_label(span, Some("offset here"), true),
            LoweringError::BadWindowDuration(reason, span) => Diagnostic::error(
                "window duration could not be converted",
            )
            .add_span_with_label(span, Some(reason.as_str()), true),
            LoweringError::AbsentExpression(span) => Diagnostic::error(
                "expression expected but none was found",
            )
            .add_span_with_label(span, Some("empty expression here"), true),
            LoweringError::WidenNeedsTypeArg(span) => Diagnostic::error(
                "`widen` requires exactly one type argument",
            )
            .add_span_with_label(span, Some("type argument missing here"), true),
            LoweringError::UnresolvableType(ty, reason, span) => {
                Diagnostic::error(&format!("cannot resolve type `{ty}`: {reason}"))
                    .add_span_with_label(span, Some("type annotation here"), true)
            }
            LoweringError::UndeclaredCallable(span) => Diagnostic::error(
                "call to undeclared function",
            )
            .add_span_with_label(span, Some("unknown callable here"), true),
            LoweringError::ForbiddenUnitSuffix(span) => {
                Diagnostic::error("unit suffix is only allowed on time literals")
                    .add_span_with_label(span, Some("non-time literal here"), true)
            }
            LoweringError::ParametricStreamNoArgs(span) => {
                Diagnostic::error("parameterised stream access is missing argument list")
                    .add_span_with_label(span, Some("arguments expected here"), true)
            }
            LoweringError::ParametricStreamNoStart(span) => {
                Diagnostic::error("stream declares parameters but has no start clause")
                    .add_span_with_label(span, Some("stream declared here"), true)
                    .add_note("add a start clause: `start with (e1, ..., eN) if cond`")
            }
            LoweringError::StartWithoutParams(span) => {
                Diagnostic::error("stream has a start clause but declares no parameters")
                    .add_span_with_label(span, Some("start clause found here"), true)
            }
            LoweringError::ParamStartCountMismatch(span, paras, targets) => {
                Diagnostic::error(&format!(
                    "parameter count ({paras}) does not match start-expression count ({targets})"
                ))
                .add_span_with_label(span, Some("mismatch here"), true)
            }
            LoweringError::AggregationOnInstance(span) => Diagnostic::error(
                "instance aggregation must target all instances, not a single one",
            )
            .add_span_with_label(span, Some("remove argument list here"), true),
            LoweringError::AggregationOnPlainStream(span) => {
                Diagnostic::error("instance aggregation requires a parameterised stream")
                    .add_span_with_label(span, Some("non-parameterised stream here"), true)
            }
            LoweringError::AlarmMissingEvalWhen(span) => {
                Diagnostic::error("alarm eval clause must include an eval-when condition")
                    .add_span_with_label(span, Some("condition missing here"), true)
            }
            LoweringError::PacingNotFrequency(span) => {
                Diagnostic::error("pacing annotation must be a frequency value")
                    .add_span_with_label(span, Some("non-frequency expression here"), true)
            }
            LoweringError::LocalFreqWithoutStart(span) => {
                Diagnostic::error("local-frequency pacing requires a start clause")
                    .add_span_with_label(span, Some("unstarted stream here"), false)
            }
            LoweringError::LocalFreqInStartClause(span) => {
                Diagnostic::error("start clause cannot carry a local periodic pacing")
                    .add_span_with_label(span, Some("local pacing found here"), true)
            }
            LoweringError::TagUsedTwice(key, first, second) => {
                Diagnostic::error(&format!("tag \"{key}\" applied to the same stream twice"))
                    .add_span_with_label(first, Some("first use here"), false)
                    .add_span_with_label(second, Some("duplicate use here"), true)
            }
            LoweringError::LambdaOutsideAggregation(span) => {
                Diagnostic::error("lambda expression is not permitted outside filtered aggregation")
                    .add_span_with_label(span, Some("lambda here"), true)
            }
            LoweringError::TupleLenMismatch(span, expr_len, ty_len) => Diagnostic::error(&format!(
                "tuple literal has {expr_len} elements but annotated type expects {ty_len}"
            ))
            .add_span_with_label(span, Some("tuple here"), true),
        };
        diag.try_attach_source()
    }
}

#[derive(Debug)]
struct IrBuilder {
    binding_map: HashMap<AstNodeId, BoundSymbol>,
    stream_index_map: HashMap<String, StreamIdx>,
    expr_counter: u32,
}

impl IrBuilder {
    fn build(
        binding_map: HashMap<AstNodeId, BoundSymbol>,
        stream_index_map: HashMap<String, StreamIdx>,
        ast: OORVAst,
        func_registry: HashMap<String, FuncDecl>,
    ) -> Result<OORVIr1, LoweringError> {
        IrBuilder {
            binding_map,
            stream_index_map,
            expr_counter: 0,
        }
        .convert_all_streams(ast, func_registry)
    }

    fn convert_all_streams(
        mut self,
        ast: OORVAst,
        func_registry: HashMap<String, FuncDecl>,
    ) -> Result<OORVIr1, LoweringError> {
        let OORVAst {
            constants: _, //handled through naming analysis
            includes: _,
            classes: _,
            signals,
            constrains,
            functions,
            members,
            nodecnts: _,
            //global_tags,
        } = ast;
        let object_domains: HashMap<String, String> = members
            .iter()
            .map(|member| (member.name.name.clone(), member.ty_name.name.clone()))
            .collect();
        let mut exprid_to_expr = HashMap::new();
        let mut ir_outputs = vec![];
        let mut constrain_idx = 0;
        for (index, constraint_def) in constrains.into_iter().enumerate() {
            let si = StreamIdx::Constraint(index);
            let ast::Constrain {
                kind,
                params,
                start,
                eval,
                end,
                annotation,
                node_id: _,
                span: _,
                level: _,
                override_flag: _,
                module_name: _,
                class_name: _,
            } = (*constraint_def).clone();
            let params = Self::translate_param_decls(params)?;
            let ty = annotation
                .as_ref()
                .map_or(Ok(None), |ty| {
                    Self::translate_type(ty)
                        .map(Some)
                        .map_err(|reason| (reason, ty.clone(), ty.span))
                })
                .map_err(|(reason, ty, span)| LoweringError::UnresolvableType(ty, reason, span))?;

            let start = self.translate_start_decl(start, &mut exprid_to_expr, si)?;
            let eval = eval
                .into_iter()
                .map(|eval| {
                    self.translate_eval_decl(eval, &mut exprid_to_expr, si, start.is_some())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let end = self.translate_end_decl(end, &mut exprid_to_expr, si)?;
            let level = constraint_def
                .level
                .as_ref()
                .map(|id| id.name.clone())
                .unwrap_or_else(|| "".to_string());

            //Check that if the output has parameters it has a start condition with a target and the other way around
            if !params.is_empty() && start.as_ref().and_then(|st| st.expression).is_none() {
                return Err(LoweringError::ParametricStreamNoStart(constraint_def.span));
            }

            if let Some(target) = start.as_ref().and_then(|st| st.expression) {
                let start_expr = &exprid_to_expr[&target];
                if params.is_empty() {
                    return Err(LoweringError::StartWithoutParams(start_expr.span));
                }
                // check that they are equal length
                let num_start_expr = match &start_expr.kind {
                    ExprVariant::Tuple(elements) => elements.len(),
                    _ => 1,
                };
                if num_start_expr != params.len() {
                    return Err(LoweringError::ParamStartCountMismatch(
                        constraint_def.span,
                        params.len(),
                        num_start_expr,
                    ));
                }
            }

            let new_kind = match kind {
                ast::ConstrainKind::Output(name) => ConstraintKind::Output(name.name),
                ast::ConstrainKind::Alarm => {
                    let new_kind = ConstraintKind::Alarm(constrain_idx);
                    constrain_idx += 1;
                    new_kind
                }
            };

            // if output stream represents a alarm, every eval clause needs to have a eval-when condition
            if let ConstraintKind::Alarm(_) = new_kind {
                for clause in &eval {
                    if clause.condition.is_none() {
                        return Err(LoweringError::AlarmMissingEvalWhen(clause.span));
                    }
                }
            }

            ir_outputs.push(Constraint {
                kind: new_kind,
                si,
                params,
                start,
                eval,
                end,
                ty,
                level,
                span: constraint_def.span,
            });
        }
        let ir_outputs = ir_outputs;

        let ir_inputs: Vec<SourceSignal> = signals
            .into_iter()
            .enumerate()
            .map(|(sig_idx, signal_def)| {
                Ok(SourceSignal {
                    ty: Self::translate_type(&signal_def.annotation).map_err(|reason| {
                        LoweringError::UnresolvableType(
                            signal_def.annotation.clone(),
                            reason,
                            signal_def.span,
                        )
                    })?,
                    name: signal_def.name.to_string(),
                    si: StreamIdx::Signal(sig_idx),
                    span: signal_def.span,
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;

        // Before we destructure fields from `self` (which would move some fields out),
        // lower all function bodies into expressions and register them. This must be
        // done while `self` is still wholly available.
        let mut func_bodies: HashMap<String, ExprNodeIdx> = HashMap::new();
        for f in functions.iter() {
            let lowered = self.translate_func_body(f, &mut exprid_to_expr)?;
            let eid = Self::register_expr(&mut exprid_to_expr, lowered);
            func_bodies.insert(f.name.name.clone(), eid);
        }

        let IrBuilder { .. } = self;
        // (function bodies already lowered above)

        let expr_registry = ExpressionRegistry::new(exprid_to_expr, func_registry, func_bodies);

        Ok(OORVIr1 {
            signals: ir_inputs,
            constraints: ir_outputs,
            object_domains,
            expr_registry,
            types: None,
            dependencies: None,
            layers: None,
            memory: None,
        })
    }

    fn translate_type(ast_ty: &ValueType) -> Result<ValueTyped, String> {
        use crate::ast::ValueTypeKind;
        match &ast_ty.kind {
            ValueTypeKind::Tuple(vec) => {
                let inner: Result<Vec<ValueTyped>, String> =
                    vec.iter().map(Self::translate_type).collect();
                inner.map(ValueTyped::Tuple)
            }
            ValueTypeKind::Optional(inner) => {
                Self::translate_type(inner).map(|inner| ValueTyped::Option(inner.into()))
            }
            ValueTypeKind::Named(string) => {
                if string == "String" {
                    return Ok(ValueTyped::String);
                }
                if string == "Bool" {
                    return Ok(ValueTyped::Bool);
                }
                if let Some(size_str) = string.strip_prefix("Int") {
                    if string.len() == 3 {
                        return Ok(ValueTyped::Int(64));
                    } else {
                        let size: u32 = size_str
                            .parse()
                            .map_err(|_| "Invalid char followed Int type annotation".to_string())?;
                        return Ok(ValueTyped::Int(size));
                    }
                }
                if let Some(size_str) = string.strip_prefix("UInt") {
                    if string.len() == 4 {
                        return Ok(ValueTyped::UInt(64));
                    } else {
                        let size: u32 = size_str.parse().map_err(|_| {
                            "Invalid char followed UInt type annotation".to_string()
                        })?;
                        return Ok(ValueTyped::UInt(size));
                    }
                }
                if let Some(size_str) = string.strip_prefix("Float") {
                    if string.len() == 5 {
                        return Ok(ValueTyped::Float(64));
                    } else {
                        let size: u32 = size_str.parse().map_err(|_| {
                            "Invalid char followed Float type annotation".to_string()
                        })?;
                        return Ok(ValueTyped::Float(size));
                    }
                }
                if let Some(size_str) = string.strip_prefix("Fixed") {
                    let (total_bits, fractional_bits) = Self::split_fixed_widths(size_str)?;
                    return Ok(ValueTyped::Fixed(total_bits, fractional_bits));
                }
                if let Some(size_str) = string.strip_prefix("UFixed") {
                    let (total_bits, fractional_bits) = Self::split_fixed_widths(size_str)?;
                    return Ok(ValueTyped::UFixed(total_bits, fractional_bits));
                }
                if string == "Bytes" {
                    return Ok(ValueTyped::Bytes);
                }
                Err("unknown type".into())
            }
        }
    }

    fn split_fixed_widths(annotation: &str) -> Result<(u32, u32), String> {
        if annotation.is_empty() {
            return Ok((64, 32));
        }
        match annotation.split_once('_') {
            Some((total_bits_str, fractional_bits_str)) => {
                let total_bits: u32 = total_bits_str
                    .parse()
                    .map_err(|_| "Invalid bit length for total bits of fixed-point type")?;
                let fractional_bits: u32 = fractional_bits_str
                    .parse()
                    .map_err(|_| "Invalid bit length for total bits of fixed-point type")?;
                Ok((total_bits, fractional_bits))
            }
            None => {
                let total_bits: u32 = annotation
                    .parse()
                    .map_err(|_| "Invalid bit length for fixed-point type")?;
                let fractional_bits = total_bits / 2;
                Ok((total_bits, fractional_bits))
            }
        }
    }

    fn find_stream_ref(
        &mut self,
        expr: &ast::ExprNode,
        current_output: StreamIdx,
        check_parameter: bool,
    ) -> Result<(StreamIdx, Vec<Expression>), LoweringError> {
        match &expr.kind {
            ast::ExprVariant::Identifier(_) => match &self.binding_map[&expr.node_id] {
                BoundSymbol::Signal(i) => {
                    Ok((self.stream_index_map[i.name.name.as_str()], Vec::new()))
                }
                BoundSymbol::Constraint(o) => {
                    Ok((self.stream_index_map[&o.name().unwrap().name], Vec::new()))
                }
                BoundSymbol::ParamOut(o) if !check_parameter => {
                    Ok((self.stream_index_map[&o.name().unwrap().name], Vec::new()))
                }
                BoundSymbol::ParamOut(_) => Err(LoweringError::ParametricStreamNoArgs(expr.span)),
                BoundSymbol::QuantifiedVar(_) => Err(LoweringError::BadStreamRef(
                    expr.span,
                    String::from("Non-identifier transformed to StreamIdx"),
                )),
                _ => Err(LoweringError::BadStreamRef(
                    expr.span,
                    String::from("Non-identifier transformed to StreamIdx"),
                )),
            },
            ast::ExprVariant::Function(name, _, args) => match &self.binding_map[&expr.node_id] {
                BoundSymbol::ParamOut(o) => Ok((
                    self.stream_index_map[&o.name().unwrap().name],
                    args.iter()
                        .map(|e| self.translate_expr(e.clone(), current_output))
                        .collect::<Result<Vec<_>, LoweringError>>()?,
                )),
                _ => Err(LoweringError::StreamExpectedGotFunc(
                    expr.span,
                    name.clone(),
                )),
            },
            _ => Err(LoweringError::BadStreamRef(
                expr.span,
                format!("{:?}", expr.kind),
            )),
        }
    }

    fn alloc_expr_id(&mut self) -> ExprNodeIdx {
        let ret = self.expr_counter;
        self.expr_counter += 1;
        ExprNodeIdx(ret)
    }

    fn translate_literal(lit: &AstLiteral) -> Result<Literal, LoweringError> {
        Ok(match &lit.kind {
            ast::LiteralKind::Boolean(b) => Literal::Bool(*b),
            ast::LiteralKind::Text(s) | ast::LiteralKind::RawText(s) => Literal::Str(s.clone()),
            ast::LiteralKind::Tuple(_) => {
                unreachable!("only allowed in constant's, which are inlined in syntactic sugar")
            }
            ast::LiteralKind::Number(num_str, postfix) => {
                match postfix {
                    Some(s) if !s.is_empty() => {
                        return Err(LoweringError::ForbiddenUnitSuffix(lit.span))
                    }
                    _ => {}
                }

                if num_str.contains('.') {
                    // Floating Point
                    Literal::Decimal(
                        num_str
                            .parse()
                            .map_err(|_| LoweringError::LiteralParseFailure(lit.span))?,
                    )
                } else if num_str.starts_with('-') {
                    Literal::SInt(
                        num_str
                            .parse()
                            .map_err(|_| LoweringError::LiteralParseFailure(lit.span))?,
                    )
                } else {
                    Literal::UInt(
                        num_str
                            .parse()
                            .map_err(|_| LoweringError::LiteralParseFailure(lit.span))?,
                    )
                }
            }
        })
    }

    fn extract_frequency(
        &mut self,
        freq: &ast::ExprNode,
    ) -> Result<Option<TimedFrequency>, LoweringError> {
        if let ast::ExprVariant::Literal(l) = &freq.kind {
            if let ast::LiteralKind::Number(_, Some(_)) = &l.kind {
                let val = freq
                    .parse_freqspec()
                    .map_err(|reason| LoweringError::BadFrequencyAnnotation(freq.span, reason))?;
                return Ok(Some(TimedFrequency {
                    span: freq.span,
                    rate: val,
                }));
            }
        }
        Ok(None)
    }

    fn translate_pacing(
        &mut self,
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
        pt: ast::PacingNode,
        current: StreamIdx,
        defaults_to_global: bool,
    ) -> std::result::Result<SourcePacingNode, LoweringError> {
        match pt {
            ast::PacingNode::NotAnnotated(span) => Ok(SourcePacingNode::Unspecified(span)),
            ast::PacingNode::Global(freq) => {
                let freq = self
                    .extract_frequency(&freq)?
                    .ok_or(LoweringError::PacingNotFrequency(freq.span))?;
                Ok(SourcePacingNode::GlobalTick(freq))
            }
            ast::PacingNode::Local(freq) => {
                let freq = self
                    .extract_frequency(&freq)?
                    .ok_or(LoweringError::PacingNotFrequency(freq.span))?;
                Ok(SourcePacingNode::LocalTick(freq))
            }
            ast::PacingNode::Unspecified(pt_expr) => {
                if let Some(freq) = self.extract_frequency(&pt_expr)? {
                    if defaults_to_global {
                        Ok(SourcePacingNode::GlobalTick(freq))
                    } else {
                        Ok(SourcePacingNode::LocalTick(freq))
                    }
                } else {
                    Ok(SourcePacingNode::Event(Self::register_expr(
                        exprid_to_expr,
                        self.translate_expr(pt_expr, current)?,
                    )))
                }
            }
        }
    }

    fn translate_expr(
        &mut self,
        ast_expression: ast::ExprNode,
        current_output: StreamIdx,
    ) -> Result<Expression, LoweringError> {
        let new_id = self.alloc_expr_id();
        let span = ast_expression.span;
        let kind: ExprVariant = match ast_expression.kind {
            ast::ExprVariant::Literal(lit) => {
                let constant = Self::translate_literal(&lit)?;
                ExprVariant::LoadConstant(IrConstant::Basic(constant))
            }
            ast::ExprVariant::Identifier(_) => match &self.binding_map[&ast_expression.node_id] {
                BoundSymbol::Constraint(o) => {
                    let si = self.stream_index_map[&o.name().unwrap().name];
                    ExprVariant::StreamAccess(si, IRAccess::Strict, Vec::new())
                }
                BoundSymbol::Signal(i) => {
                    let si = self.stream_index_map[i.name.name.as_str()];
                    ExprVariant::StreamAccess(si, IRAccess::Strict, Vec::new())
                }
                BoundSymbol::Const(c) => {
                    let ty = c
                        .annotation
                        .as_ref()
                        .ok_or(LoweringError::UntypedConstant(span))?;
                    let ty = Self::translate_type(ty).map_err(|reason| {
                        LoweringError::UnresolvableType(ty.clone(), reason, span)
                    })?;
                    let lit = c.value.clone();
                    self.evaluate_constant(lit, ty)?
                }

                BoundSymbol::Param(p) => ExprVariant::ParameterAccess(current_output, p.position),
                BoundSymbol::Local(ident) => {
                    return Err(LoweringError::BadStreamRef(span, ident.name.clone()));
                }
                BoundSymbol::ParamOut(_) => {
                    return Err(LoweringError::ParametricStreamNoArgs(span));
                }
                BoundSymbol::Func(_) | BoundSymbol::ValueType => {
                    unreachable!("Identifiers can only refer to streams")
                }
                BoundSymbol::QuantifiedVar(q) => {
                    ExprVariant::QuantifiedVar(crate::oorvir::source::Ident {
                        name: q.name.clone(),
                    })
                }
            },
            ast::ExprVariant::SignalAccess(expr, kind) => {
                let access_kind = match kind {
                    AccessMode::Strict => IRAccess::Cached,
                    AccessMode::Cached => IRAccess::Strict,
                    AccessMode::Get => IRAccess::Get,
                    AccessMode::Fresh => IRAccess::Fresh,
                };
                let (expr_ref, args) = self.find_stream_ref(expr.as_ref(), current_output, true)?;
                ExprVariant::StreamAccess(expr_ref, access_kind, args)
            }
            ast::ExprVariant::Default(expr, def) => ExprVariant::Default {
                expr: Box::new(self.translate_expr(*expr, current_output)?),
                default: Box::new(self.translate_expr(*def, current_output)?),
            },
            ast::ExprVariant::Shift(ref target_expr, offset) => {
                let ir_offset = match offset {
                    ast::Shift::Discrete(0) => None,
                    ast::Shift::Discrete(i) if i > 0 => {
                        Some(Shift::FutureDiscrete(i.unsigned_abs().into()))
                    }
                    ast::Shift::Discrete(i) => Some(Shift::PastDiscrete(i.unsigned_abs().into())),
                };
                let (expr_ref, args) = self.find_stream_ref(target_expr, current_output, true)?;
                let kind = ir_offset.map(IRAccess::Shift).unwrap_or(IRAccess::Strict);
                ExprVariant::StreamAccess(expr_ref, kind, args)
            }
            ast::ExprVariant::Binary(op, left, right) => {
                use crate::ast::BinaryOp;

                use crate::oorvir::source::ArithLogOp::*;
                let arith_op = match op {
                    BinaryOp::Add => Add,
                    BinaryOp::Sub => Sub,
                    BinaryOp::Mul => Mul,
                    BinaryOp::Div => Div,
                    BinaryOp::Rem => Rem,
                    BinaryOp::Pow => Pow,
                    BinaryOp::And => And,
                    BinaryOp::Or => Or,
                    BinaryOp::BitXor => BitXor,
                    BinaryOp::BitAnd => BitAnd,
                    BinaryOp::BitOr => BitOr,
                    BinaryOp::Shl => Shl,
                    BinaryOp::Shr => Shr,
                    BinaryOp::Eq => Eq,
                    BinaryOp::Lt => Lt,
                    BinaryOp::Le => Le,
                    BinaryOp::Ne => Ne,
                    BinaryOp::Ge => Ge,
                    BinaryOp::Gt => Gt,
                };
                let arguments: Vec<Expression> = vec![
                    self.translate_expr(*left, current_output)?,
                    self.translate_expr(*right, current_output)?,
                ];
                ExprVariant::ArithLog(arith_op, arguments)
            }
            ast::ExprVariant::Unary(op, arg) => {
                use crate::ast::UnaryOp;
                use crate::oorvir::source::ArithLogOp::*;
                let arith_op = match op {
                    UnaryOp::Not => Not,
                    UnaryOp::Neg => Neg,
                    UnaryOp::BitNot => BitNot,
                };
                let arguments: Vec<Expression> = vec![self.translate_expr(*arg, current_output)?];
                ExprVariant::ArithLog(arith_op, arguments)
            }
            ast::ExprVariant::Ite(cond, cons, alt) => {
                let condition = Box::new(self.translate_expr(*cond, current_output)?);
                let consequence = Box::new(self.translate_expr(*cons, current_output)?);
                let alternative = Box::new(self.translate_expr(*alt, current_output)?);
                ExprVariant::Ite {
                    condition,
                    consequence,
                    alternative,
                }
            }
            ast::ExprVariant::Bracket(inner) => {
                return self.translate_expr(*inner, current_output);
            }
            ast::ExprVariant::MissingExpr => return Err(LoweringError::AbsentExpression(span)),
            ast::ExprVariant::Tuple(inner) => ExprVariant::Tuple(
                inner
                    .into_iter()
                    .map(|ex| self.translate_expr(ex, current_output))
                    .collect::<Result<Vec<_>, LoweringError>>()?,
            ),
            ast::ExprVariant::Field(inner_exp, ident) => {
                let num: usize = ident.name.parse().expect("checked in AST verifier");
                let inner = Box::new(self.translate_expr(*inner_exp, current_output)?);
                ExprVariant::TupleAccess(inner, num)
            }
            ast::ExprVariant::Method(base, name, type_param, mut args) => {
                // Method Access is same as function application with base as first parameter
                args.insert(0, *base);
                self.translate_func_call(
                    false,
                    ast_expression.node_id,
                    &span,
                    current_output,
                    ast::ExprVariant::Function(name, type_param, args),
                )?
            }
            ast::ExprVariant::Function(..) => self.translate_func_call(
                true,
                ast_expression.node_id,
                &span,
                current_output,
                ast_expression.kind,
            )?,
            ast::ExprVariant::Quantified(quant, bindings1, bindings2, inner) => {
                // Convert AST quantified expression to source_ir quantified expression.
                // We do not expand IdentList domains here; expansion can be performed
                // at a later stage if desired. For now create a source_ir::Quantified node
                // with transformed inner expression.
                let ir_bindings1: Vec<crate::oorvir::source::Ident> = bindings1
                    .into_iter()
                    .map(|id| crate::oorvir::source::Ident {
                        name: id.name.clone(),
                    })
                    .collect();

                let ir_bindings2: Vec<crate::oorvir::source::Ident> = bindings2
                    .into_iter()
                    .map(|id| crate::oorvir::source::Ident {
                        name: id.name.clone(),
                    })
                    .collect();

                let inner_ir = Box::new(self.translate_expr(*inner, current_output)?);

                let ir_quant = match quant {
                    ast::Quantifier::Forall => crate::oorvir::source::Quantifier::Forall,
                    ast::Quantifier::Exists => crate::oorvir::source::Quantifier::Exists,
                };

                ExprVariant::Quantified(ir_quant, ir_bindings1, ir_bindings2, inner_ir)
            }
        };
        let aaa = Expression {
            kind,
            eid: new_id,
            span,
        };
        Ok(aaa)
    }

    fn evaluate_constant(
        &mut self,
        lit: AstLiteral,
        ty: ValueTyped,
    ) -> Result<ExprVariant, LoweringError> {
        match (lit, ty) {
            (
                AstLiteral {
                    kind: ast::LiteralKind::Tuple(xs),
                    span,
                    ..
                },
                ValueTyped::Tuple(tys),
            ) => {
                if xs.len() != tys.len() {
                    return Err(LoweringError::TupleLenMismatch(span, xs.len(), tys.len()));
                }
                let inner = xs
                    .into_iter()
                    .zip(tys)
                    .map(|(x, ty)| {
                        Ok(Expression {
                            kind: self.evaluate_constant(x, ty)?,
                            eid: self.alloc_expr_id(),
                            span,
                        })
                    })
                    .collect::<Result<Vec<_>, LoweringError>>()?;
                Ok(ExprVariant::Tuple(inner))
            }
            (lit, ty) => Ok(ExprVariant::LoadConstant(IrConstant::Inlined(Inlined {
                lit: Self::translate_literal(&lit)?,
                ty,
            }))),
        }
    }

    /// Unifies the transformation of function and method applications to the internal representation
    fn translate_func_in_body(
        &mut self,
        allow_parametric: bool,
        id: AstNodeId,
        span: &SourceSpan,
        current_output: StreamIdx,
        kind: ast::ExprVariant,
        param_map: &std::collections::HashMap<String, (usize, crate::oorvir::source::ValueTyped)>,
        locals: &std::collections::HashMap<String, Expression>,
    ) -> Result<ExprVariant, LoweringError> {
        let (name, type_param, args) =
            if let ast::ExprVariant::Function(name, type_param, args) = kind {
                (name, type_param, args)
            } else {
                unreachable!()
            };
        let decl = self
            .binding_map
            .get(&id)
            .ok_or(LoweringError::UndeclaredCallable(*span))?;
        match decl {
            BoundSymbol::Func(_) => {
                let name = name.name.name;
                let args: Vec<Expression> = args
                    .into_iter()
                    .map(|ex| self.translate_body_expr(ex, param_map, locals))
                    .collect::<Result<Vec<_>, LoweringError>>()?;

                if name.starts_with("widen") {
                    let widen_arg = args
                        .first()
                        .ok_or(LoweringError::WidenNeedsTypeArg(*span))?;
                    Ok(ExprVariant::Widen(WidenExprKind {
                        expr: Box::new(widen_arg.clone()),
                        ty: match type_param.first() {
                            Some(t) => Self::translate_type(t).map_err(|reason| {
                                LoweringError::UnresolvableType(t.clone(), reason, *span)
                            })?,
                            None => todo!("error case"),
                        },
                    }))
                } else {
                    Ok(ExprVariant::Function(FnExprKind {
                        name,
                        args,
                        type_param: type_param
                            .into_iter()
                            .map(|t| {
                                Self::translate_type(&t).map_err(|reason| {
                                    LoweringError::UnresolvableType(t, reason, *span)
                                })
                            })
                            .collect::<Result<Vec<_>, LoweringError>>()?,
                    }))
                }
            }
            BoundSymbol::ParamOut(_) => {
                if allow_parametric {
                    Ok(ExprVariant::StreamAccess(
                        self.stream_index_map[&name.name.name],
                        IRAccess::Strict,
                        args.into_iter()
                            .map(|ex| self.translate_expr(ex, current_output))
                            .collect::<Result<Vec<_>, LoweringError>>()?,
                    ))
                } else {
                    Err(LoweringError::UndeclaredCallable(*span))
                }
            }
            _ => Err(LoweringError::UndeclaredCallable(*span)),
        }
    }

    /// Unifies the transformation of function and method applications to the internal representation
    fn translate_func_call(
        &mut self,
        allow_parametric: bool,
        id: AstNodeId,
        span: &SourceSpan,
        current_output: StreamIdx,
        kind: ast::ExprVariant,
    ) -> Result<ExprVariant, LoweringError> {
        let (name, type_param, args) =
            if let ast::ExprVariant::Function(name, type_param, args) = kind {
                (name, type_param, args)
            } else {
                unreachable!()
            };
        let decl = self
            .binding_map
            .get(&id)
            .ok_or(LoweringError::UndeclaredCallable(*span))?;
        match decl {
            BoundSymbol::Func(_) => {
                let name = name.name.name;
                let args: Vec<Expression> = args
                    .into_iter()
                    .map(|ex| self.translate_expr(ex, current_output))
                    .collect::<Result<Vec<_>, LoweringError>>()?;

                if name.starts_with("widen") {
                    let widen_arg = args
                        .first()
                        .ok_or(LoweringError::WidenNeedsTypeArg(*span))?;
                    Ok(ExprVariant::Widen(WidenExprKind {
                        expr: Box::new(widen_arg.clone()),
                        ty: match type_param.first() {
                            Some(t) => Self::translate_type(t).map_err(|reason| {
                                LoweringError::UnresolvableType(t.clone(), reason, *span)
                            })?,
                            None => todo!("error case"),
                        },
                    }))
                } else {
                    Ok(ExprVariant::Function(FnExprKind {
                        name,
                        args,
                        type_param: type_param
                            .into_iter()
                            .map(|t| {
                                Self::translate_type(&t).map_err(|reason| {
                                    LoweringError::UnresolvableType(t, reason, *span)
                                })
                            })
                            .collect::<Result<Vec<_>, LoweringError>>()?,
                    }))
                }
            }
            BoundSymbol::ParamOut(_) => {
                if allow_parametric {
                    Ok(ExprVariant::StreamAccess(
                        self.stream_index_map[&name.name.name],
                        IRAccess::Strict,
                        args.into_iter()
                            .map(|ex| self.translate_expr(ex, current_output))
                            .collect::<Result<Vec<_>, LoweringError>>()?,
                    ))
                } else {
                    Err(LoweringError::UndeclaredCallable(*span))
                }
            }
            _ => Err(LoweringError::UndeclaredCallable(*span)),
        }
    }

    /// Adds an expression Id and the expression into the hash map and returns the id.
    fn register_expr(
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
        expr: Expression,
    ) -> ExprNodeIdx {
        let id = expr.eid;
        exprid_to_expr.insert(id, expr);
        id
    }

    /// Lowers a function/method body (AST) into a single source_ir `Expression`.
    /// This implements "scheme B": locals (`let` bindings) are transformed and
    /// their RHS expressions are inserted into `exprid_to_expr`; subsequent
    /// references to local names are replaced with the previously-built expressions
    /// (cloned). Function parameters are lowered to `ParameterAccess` with a
    /// dummy `StreamIdx` (the runtime ignores the first field and only uses the index).
    fn translate_func_body(
        &mut self,
        f: &ast::GlobalMethodDecl,
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
    ) -> Result<Expression, LoweringError> {
        // Use a dedicated binding record to keep parameter metadata explicit.
        struct ParamEntry {
            param_name: String,
            param_index: usize,
            param_type: crate::oorvir::source::ValueTyped,
        }

        // Build parameter bindings first, then derive maps only when needed.
        let mut param_entries: Vec<ParamEntry> = Vec::new();
        for p in f.params.iter() {
            let ty = if let Some(annotation) = p.annotation.as_ref() {
                Self::translate_type(annotation).map_err(|reason| {
                    LoweringError::UnresolvableType(annotation.clone(), reason, p.span)
                })?
            } else {
                crate::oorvir::source::ValueTyped::Any
            };
            param_entries.push(ParamEntry {
                param_name: p.name.name.clone(),
                param_index: p.position,
                param_type: ty,
            });
        }

        // Keep lowered local let-bindings for later identifier substitution.
        let mut local_bindings: HashMap<String, Expression> = HashMap::new();

        // Track the last statement expression as fallback return value.
        let mut last_stmt_expr: Option<Expression> = None;
        for stmt in f.body.decls.iter() {
            match stmt {
                ast::MethodStmt::Let(ld) => {
                    // Re-materialize a lightweight parameter map for expression lowering.
                    let param_map: std::collections::HashMap<
                        String,
                        (usize, crate::oorvir::source::ValueTyped),
                    > = param_entries
                        .iter()
                        .map(|pb| {
                            (
                                pb.param_name.clone(),
                                (pb.param_index, pb.param_type.clone()),
                            )
                        })
                        .collect();

                    let rhs =
                        self.translate_body_expr(ld.expr.clone(), &param_map, &local_bindings)?;
                    Self::register_expr(exprid_to_expr, rhs.clone());
                    local_bindings.insert(ld.name.name.clone(), rhs);
                }
                ast::MethodStmt::Expr(e) => {
                    let param_map: std::collections::HashMap<
                        String,
                        (usize, crate::oorvir::source::ValueTyped),
                    > = param_entries
                        .iter()
                        .map(|pb| {
                            (
                                pb.param_name.clone(),
                                (pb.param_index, pb.param_type.clone()),
                            )
                        })
                        .collect();

                    let ee = self.translate_body_expr(e.clone(), &param_map, &local_bindings)?;
                    Self::register_expr(exprid_to_expr, ee.clone());
                    last_stmt_expr = Some(ee);
                }
            }
        }

        // Resolve function return expression in a deterministic order.
        let return_expr = if let Some(ret) = &f.body.ret {
            let param_map: std::collections::HashMap<
                String,
                (usize, crate::oorvir::source::ValueTyped),
            > = param_entries
                .iter()
                .map(|pb| {
                    (
                        pb.param_name.clone(),
                        (pb.param_index, pb.param_type.clone()),
                    )
                })
                .collect();

            self.translate_body_expr(ret.clone(), &param_map, &local_bindings)?
        } else if let Some(last_expr) = last_stmt_expr {
            last_expr
        } else {
            // Keep legacy behavior: an empty body defaults to `false`.
            let new_id = self.alloc_expr_id();
            Expression {
                kind: ExprVariant::LoadConstant(IrConstant::Basic(Literal::Bool(false))),
                eid: new_id,
                span: f.span,
            }
        };

        Ok(return_expr)
    }

    /// Internal helper used by `transform_function_body` which largely mirrors
    /// `transform_expression` but gives precedence to local `let` bindings and
    /// function parameters when resolving identifiers.
    fn translate_body_expr(
        &mut self,
        ast_expression: ast::ExprNode,
        param_map: &std::collections::HashMap<String, (usize, crate::oorvir::source::ValueTyped)>,
        locals: &std::collections::HashMap<String, Expression>,
    ) -> Result<Expression, LoweringError> {
        // This function is intentionally similar to `transform_expression`, but the
        // `Ident` branch first checks `locals` and `param_map` before consulting
        // `self.binding_map`.
        let new_id = self.alloc_expr_id();
        let span = ast_expression.span;
        let kind: ExprVariant = match ast_expression.kind {
            ast::ExprVariant::Identifier(id) => {
                let name = id.name.clone();
                if let Some(local_expr) = locals.get(&name) {
                    return Ok(local_expr.clone());
                }
                if let Some((idx, at)) = param_map.get(&name) {
                    return Ok(Expression {
                        kind: ExprVariant::FunctionParameterAccess(
                            crate::oorvir::source::Ident { name: name.clone() },
                            at.clone(),
                            *idx,
                        ),
                        eid: new_id,
                        span,
                    });
                }
                // fallback to regular resolution using binding_map
                match &self.binding_map[&ast_expression.node_id] {
                    BoundSymbol::Constraint(o) => {
                        let si = self.stream_index_map[&o.name().unwrap().name];
                        ExprVariant::StreamAccess(si, IRAccess::Strict, Vec::new())
                    }
                    BoundSymbol::Signal(i) => {
                        let si = self.stream_index_map[i.name.name.as_str()];
                        ExprVariant::StreamAccess(si, IRAccess::Strict, Vec::new())
                    }
                    BoundSymbol::Const(c) => {
                        let ty = c
                            .annotation
                            .as_ref()
                            .ok_or(LoweringError::UntypedConstant(span))?;
                        let ty = Self::translate_type(ty).map_err(|reason| {
                            LoweringError::UnresolvableType(ty.clone(), reason, span)
                        })?;
                        let lit = c.value.clone();
                        self.evaluate_constant(lit, ty)?
                    }
                    BoundSymbol::Param(p) => {
                        let at = p.annotation.as_ref().map_or(
                            Ok(crate::oorvir::source::ValueTyped::Any),
                            |ty| {
                                Self::translate_type(ty).map_err(|reason| {
                                    LoweringError::UnresolvableType(ty.clone(), reason, span)
                                })
                            },
                        )?;
                        ExprVariant::FunctionParameterAccess(
                            crate::oorvir::source::Ident {
                                name: p.name.to_string(),
                            },
                            at,
                            p.position,
                        )
                    }
                    BoundSymbol::Local(ident) => {
                        return Err(LoweringError::BadStreamRef(span, ident.name.clone()));
                    }
                    BoundSymbol::ParamOut(_) => {
                        return Err(LoweringError::ParametricStreamNoArgs(span));
                    }
                    BoundSymbol::Func(_) | BoundSymbol::ValueType => {
                        unreachable!("Identifiers can only refer to streams")
                    }
                    BoundSymbol::QuantifiedVar(q) => {
                        ExprVariant::QuantifiedVar(crate::oorvir::source::Ident {
                            name: q.name.clone(),
                        })
                    }
                }
            }
            // Delegate other variants to the main transformer but using recursion where needed.
            ast::ExprVariant::Literal(lit) => {
                let constant = Self::translate_literal(&lit)?;
                ExprVariant::LoadConstant(IrConstant::Basic(constant))
            }
            ast::ExprVariant::SignalAccess(expr, kind) => {
                let access_kind = match kind {
                    AccessMode::Strict => IRAccess::Cached,
                    AccessMode::Cached => IRAccess::Strict,
                    AccessMode::Get => IRAccess::Get,
                    AccessMode::Fresh => IRAccess::Fresh,
                };
                let (expr_ref, args) =
                    self.find_stream_ref(&expr, StreamIdx::Constraint(0), true)?;
                ExprVariant::StreamAccess(expr_ref, access_kind, args)
            }
            ast::ExprVariant::Default(expr, def) => ExprVariant::Default {
                expr: Box::new(self.translate_body_expr(*expr, param_map, locals)?),
                default: Box::new(self.translate_body_expr(*def, param_map, locals)?),
            },
            ast::ExprVariant::Shift(ref target_expr, offset) => {
                let ir_offset = match offset {
                    ast::Shift::Discrete(0) => None,
                    ast::Shift::Discrete(i) if i > 0 => {
                        Some(Shift::FutureDiscrete(i.unsigned_abs().into()))
                    }
                    ast::Shift::Discrete(i) => Some(Shift::PastDiscrete(i.unsigned_abs().into())),
                };
                let (expr_ref, args) =
                    self.find_stream_ref(target_expr, StreamIdx::Constraint(0), true)?;
                let kind = ir_offset.map(IRAccess::Shift).unwrap_or(IRAccess::Strict);
                ExprVariant::StreamAccess(expr_ref, kind, args)
            }
            ast::ExprVariant::Binary(op, left, right) => {
                use crate::ast::BinaryOp;
                use crate::oorvir::source::ArithLogOp::*;
                let arith_op = match op {
                    BinaryOp::Add => Add,
                    BinaryOp::Sub => Sub,
                    BinaryOp::Mul => Mul,
                    BinaryOp::Div => Div,
                    BinaryOp::Rem => Rem,
                    BinaryOp::Pow => Pow,
                    BinaryOp::And => And,
                    BinaryOp::Or => Or,
                    BinaryOp::BitXor => BitXor,
                    BinaryOp::BitAnd => BitAnd,
                    BinaryOp::BitOr => BitOr,
                    BinaryOp::Shl => Shl,
                    BinaryOp::Shr => Shr,
                    BinaryOp::Eq => Eq,
                    BinaryOp::Lt => Lt,
                    BinaryOp::Le => Le,
                    BinaryOp::Ne => Ne,
                    BinaryOp::Ge => Ge,
                    BinaryOp::Gt => Gt,
                };
                let arguments: Vec<Expression> = vec![
                    self.translate_body_expr(*left, param_map, locals)?,
                    self.translate_body_expr(*right, param_map, locals)?,
                ];
                ExprVariant::ArithLog(arith_op, arguments)
            }
            ast::ExprVariant::Unary(op, arg) => {
                use crate::oorvir::source::ArithLogOp::*;
                let arith_op = match op {
                    UnaryOp::Not => Not,
                    UnaryOp::Neg => Neg,
                    UnaryOp::BitNot => BitNot,
                };
                let arguments: Vec<Expression> =
                    vec![self.translate_body_expr(*arg, param_map, locals)?];
                ExprVariant::ArithLog(arith_op, arguments)
            }
            ast::ExprVariant::Ite(cond, cons, alt) => {
                let condition = Box::new(self.translate_body_expr(*cond, param_map, locals)?);
                let consequence = Box::new(self.translate_body_expr(*cons, param_map, locals)?);
                let alternative = Box::new(self.translate_body_expr(*alt, param_map, locals)?);
                ExprVariant::Ite {
                    condition,
                    consequence,
                    alternative,
                }
            }
            ast::ExprVariant::Bracket(inner) => {
                return self.translate_body_expr(*inner, param_map, locals)
            }
            ast::ExprVariant::MissingExpr => return Err(LoweringError::AbsentExpression(span)),
            ast::ExprVariant::Tuple(inner) => ExprVariant::Tuple(
                inner
                    .into_iter()
                    .map(|ex| self.translate_body_expr(ex, param_map, locals))
                    .collect::<Result<Vec<_>, LoweringError>>()?,
            ),
            ast::ExprVariant::Field(inner_exp, ident) => {
                let num: usize = ident.name.parse().expect("checked in AST verifier");
                let inner = Box::new(self.translate_body_expr(*inner_exp, param_map, locals)?);
                ExprVariant::TupleAccess(inner, num)
            }
            ast::ExprVariant::Method(base, name, type_param, mut args) => {
                args.insert(0, *base);
                self.translate_func_in_body(
                    false,
                    ast_expression.node_id,
                    &span,
                    StreamIdx::Constraint(0),
                    ast::ExprVariant::Function(name, type_param, args),
                    param_map,
                    locals,
                )?
            }
            ast::ExprVariant::Function(..) => self.translate_func_in_body(
                true,
                ast_expression.node_id,
                &span,
                StreamIdx::Constraint(0),
                ast_expression.kind,
                param_map,
                locals,
            )?,
            ast::ExprVariant::Quantified(quant, idents1, idents2, inner) => {
                // AST now contains a preprocessed ordered list of idents.
                // Map each ident to a source_ir VarBinding with a trivial domain (IdentPath to the same name).
                let ir_bindings1: Vec<crate::oorvir::source::Ident> = idents1
                    .into_iter()
                    .map(|id| crate::oorvir::source::Ident {
                        name: id.name.clone(),
                    })
                    .collect();
                let ir_bindings2: Vec<crate::oorvir::source::Ident> = idents2
                    .into_iter()
                    .map(|id| crate::oorvir::source::Ident {
                        name: id.name.clone(),
                    })
                    .collect();
                let inner_ir = Box::new(self.translate_body_expr(*inner, param_map, locals)?);
                let ir_quant = match quant {
                    ast::Quantifier::Forall => crate::oorvir::source::Quantifier::Forall,
                    ast::Quantifier::Exists => crate::oorvir::source::Quantifier::Exists,
                };
                ExprVariant::Quantified(ir_quant, ir_bindings1, ir_bindings2, inner_ir)
            }
        };
        Ok(Expression {
            kind,
            eid: new_id,
            span,
        })
    }

    fn translate_start_decl(
        &mut self,
        start_spec: Option<StartDecl>,
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
        current_output: StreamIdx,
    ) -> Result<Option<StartNode>, LoweringError> {
        start_spec.map_or(Ok(None), |start_spec| {
            let StartDecl {
                expression,
                pacing,
                condition,
                span,
                ..
            } = start_spec;
            let expression = expression.map_or(Ok(None), |expr| {
                if let ast::ExprVariant::Bracket(ref exp) = expr.kind {
                    if let ast::ExprVariant::MissingExpr = exp.kind {
                        return Ok(None);
                    }
                }
                let exp = self.translate_expr(expr, current_output)?;
                Ok(Some(Self::register_expr(exprid_to_expr, exp)))
            })?;
            let pacing = self.translate_pacing(exprid_to_expr, pacing, current_output, true)?;
            if let SourcePacingNode::LocalTick(f) = pacing {
                return Err(LoweringError::LocalFreqInStartClause(f.span));
            }

            let condition = condition.map_or(Ok(None), |cond_expr| {
                let e = self.translate_expr(cond_expr, current_output)?;
                Ok(Some(Self::register_expr(exprid_to_expr, e)))
            })?;
            Ok(Some(StartNode {
                expression,
                pacing,
                condition,
                span,
            }))
        })
    }

    fn translate_eval_decl(
        &mut self,
        eval_spec: ast::EvalDecl,
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
        current_output: StreamIdx,
        has_start: bool,
    ) -> Result<EvalNode, LoweringError> {
        let eval_expr = if let Some(eval_expr) = eval_spec.expression {
            self.translate_expr(eval_expr, current_output)?
        } else {
            unreachable!("Empty tuple is inserted if the expression is unspecified or the parser reports an error");
        };
        let eval_expr_id = Self::register_expr(exprid_to_expr, eval_expr);

        let condition = eval_spec.condition.map_or(Ok(None), |cond| {
            let cond_expr = self.translate_expr(cond, current_output)?;
            Ok(Some(Self::register_expr(exprid_to_expr, cond_expr)))
        })?;
        let pacing =
            self.translate_pacing(exprid_to_expr, eval_spec.pacing, current_output, !has_start)?;
        if let SourcePacingNode::LocalTick(f) = pacing {
            if !has_start {
                return Err(LoweringError::LocalFreqWithoutStart(f.span));
            }
        }

        Ok(EvalNode {
            expression: eval_expr_id,
            condition,
            pacing,
            span: eval_spec.span,
        })
    }

    fn translate_end_decl(
        &mut self,
        end_spec: Option<ast::EndDecl>,
        exprid_to_expr: &mut HashMap<ExprNodeIdx, Expression>,
        current_output: StreamIdx,
    ) -> Result<Option<EndNode>, LoweringError> {
        end_spec.map_or(Ok(None), |end_spec| {
            let pacing =
                self.translate_pacing(exprid_to_expr, end_spec.pacing, current_output, true)?;
            let condition = Self::register_expr(
                exprid_to_expr,
                self.translate_expr(end_spec.condition, current_output)?,
            );
            Ok(Some(EndNode {
                condition,
                pacing,
                span: end_spec.span,
            }))
        })
    }

    fn translate_param_decls(
        params: Vec<Rc<ast::ParamDecl>>,
    ) -> Result<Vec<SourceParamDecl>, LoweringError> {
        let params = params
            .iter()
            .enumerate()
            .map(|(ix, p)| {
                assert_eq!(ix, p.position);
                p.annotation
                    .as_ref()
                    .map_or(Ok(None), |ty| {
                        Self::translate_type(ty)
                            .map(Some)
                            .map_err(|reason| (reason, ty.clone(), p.span))
                    })
                    .map(|p_ty| SourceParamDecl {
                        name: p.name.name.clone(),
                        ty: p_ty,
                        position: p.position,
                        span: p.span,
                    })
            })
            .collect::<Result<Vec<SourceParamDecl>, (String, ValueType, SourceSpan)>>()
            .map_err(|(reason, ty, span)| LoweringError::UnresolvableType(ty, reason, span))?;
        Ok(params)
    }
}
