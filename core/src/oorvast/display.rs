use std::fmt::{Display, Formatter, Result};

use crate::ast::*;

// Join elements of a slice with a delimiter, wrapped in prefix/suffix.
fn join_list<T: Display>(
    f: &mut Formatter<'_>,
    items: &[T],
    open: &str,
    close: &str,
    sep: &str,
) -> Result {
    write!(f, "{open}")?;
    let mut iter = items.iter();
    if let Some(first) = iter.next() {
        write!(f, "{first}")?;
        for rest in iter {
            write!(f, "{sep}{rest}")?;
        }
    }
    write!(f, "{close}")?;
    Ok(())
}

// Format an optional value between a prefix and suffix.
fn surround_opt<T: Display>(opt: &Option<T>, before: &str, after: &str) -> String {
    opt.as_ref()
        .map(|v| format!("{before}{v}{after}"))
        .unwrap_or_default()
}

// Format an optional type annotation.
fn type_annotation(ty: &Option<ValueType>) -> String {
    surround_opt(ty, ": ", "")
}

// ──────────────────────────────────────────────
//  Primitive types
// ──────────────────────────────────────────────

impl Display for AstNodeId {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{}", self.0)
    }
}

impl Display for Identifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.name)
    }
}

impl Display for TimeUnit {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let s = match self {
            TimeUnit::Nanosecond => "ns",
            TimeUnit::Microsecond => "μs",
            TimeUnit::Millisecond => "ms",
            TimeUnit::Second => "s",
            TimeUnit::Minute => "min",
            TimeUnit::Hour => "h",
            TimeUnit::Day => "d",
            TimeUnit::Week => "w",
            TimeUnit::Year => "a",
        };
        write!(f, "{s}")
    }
}

impl Display for Shift {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Shift::Discrete(n) => write!(f, "{n}"),
        }
    }
}

impl Display for InstanceSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            InstanceSelection::Fresh => write!(f, "fresh"),
            InstanceSelection::All => write!(f, "all"),
        }
    }
}

// ──────────────────────────────────────────────
//  Operators
// ──────────────────────────────────────────────

impl Display for BinaryOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        use BinaryOp::*;
        let sym = match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            Pow => "**",
            And => "∧",
            Or => "∨",
            Eq => "=",
            Lt => "<",
            Le => "≤",
            Ne => "≠",
            Gt => ">",
            Ge => "≥",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Shl => "<<",
            Shr => ">>",
        };
        write!(f, "{sym}")
    }
}

impl Display for UnaryOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let sym = match self {
            UnaryOp::Not => "!",
            UnaryOp::Neg => "-",
            UnaryOp::BitNot => "~",
        };
        write!(f, "{sym}")
    }
}

// ──────────────────────────────────────────────
//  Literals
// ──────────────────────────────────────────────

impl Display for TokenLiteral {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match &self.kind {
            LiteralKind::Boolean(b) => write!(f, "{b}"),
            LiteralKind::Number(v, unit) => write!(f, "{v}{}", unit.clone().unwrap_or_default()),
            LiteralKind::Text(s) => write!(f, "\"{s}\""),
            LiteralKind::Tuple(elems) => join_list(f, elems, "(", ")", ", "),
            LiteralKind::RawText(s) => {
                // Determine the minimum number of '#' needed so r#"..."# is unambiguous.
                let mut hashes = 0;
                while s.contains(&format!("{}\"", "#".repeat(hashes))) {
                    hashes += 1;
                }
                let pad = "#".repeat(hashes);
                write!(f, "r{pad}\"{s}\"{pad}")
            }
        }
    }
}

// ──────────────────────────────────────────────
//  Type annotations
// ──────────────────────────────────────────────

impl Display for ValueTypeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            ValueTypeKind::Named(name) => write!(f, "{name}"),
            ValueTypeKind::Tuple(types) => join_list(f, types, "(", ")", ", "),
            ValueTypeKind::Optional(inner) => write!(f, "{inner}?"),
        }
    }
}

impl Display for ValueType {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.kind)
    }
}

// ──────────────────────────────────────────────
//  Function references
// ──────────────────────────────────────────────

impl Display for FuncLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.name)?;
        let labels: Vec<String> = self
            .arg_names
            .iter()
            .map(|n| match n {
                None => "_:".to_string(),
                Some(v) => format!("{v}:"),
            })
            .collect();
        join_list(f, &labels, "(", ")", "")
    }
}

// ──────────────────────────────────────────────
//  Pacing / activation
// ──────────────────────────────────────────────

impl Display for PacingNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            PacingNode::NotAnnotated(_) => Ok(()),
            PacingNode::Global(expr) => write!(f, " @Global({expr})"),
            PacingNode::Local(expr) => write!(f, " @Local({expr})"),
            PacingNode::Unspecified(expr) => write!(f, " @{expr}"),
        }
    }
}

// ──────────────────────────────────────────────
//  Stream lifecycle declarations
// ──────────────────────────────────────────────

impl Display for StartDecl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if self.expression.is_some() || self.condition.is_some() {
            write!(f, "start")?;
        }
        write!(f, "{}", self.pacing)?;
        if let Some(c) = &self.condition {
            write!(f, " when {c}")?;
        }
        if let Some(e) = &self.expression {
            write!(f, " with {e}")?;
        }
        Ok(())
    }
}

impl Display for EvalDecl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let has_content = self.condition.is_some()
            || self.expression.is_some()
            || !matches!(self.pacing, PacingNode::NotAnnotated(_));
        if has_content {
            write!(f, "eval")?;
        }
        write!(f, "{}", self.pacing)?;
        if let Some(c) = &self.condition {
            write!(f, " when {c}")?;
        }
        if let Some(e) = &self.expression {
            write!(f, " with {e}")?;
        }
        Ok(())
    }
}

impl Display for EndDecl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "end{} when {}", self.pacing, self.condition)
    }
}

// ──────────────────────────────────────────────
//  Declarations
// ──────────────────────────────────────────────

impl Display for ParamDecl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match &self.annotation {
            None => write!(f, "{}", self.name),
            Some(ty) => write!(f, "{}: {ty}", self.name),
        }
    }
}

impl Display for ConstDecl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(
            f,
            "constant {}{} := {}",
            self.name,
            type_annotation(&self.annotation),
            self.value
        )
    }
}

// ──────────────────────────────────────────────
//  Expressions
// ──────────────────────────────────────────────

impl Display for ExprNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match &self.kind {
            ExprVariant::Literal(lit) => write!(f, "{lit}"),
            ExprVariant::Identifier(id) => write!(f, "{id}"),
            ExprVariant::SignalAccess(expr, mode) => match mode {
                AccessMode::Strict => write!(f, "{expr}"),
                AccessMode::Cached => write!(f, "{expr}.hold()"),
                AccessMode::Get => write!(f, "{expr}.get()"),
                AccessMode::Fresh => write!(f, "{expr}.is_fresh()"),
            },
            ExprVariant::Default(base, fallback) => write!(f, "{base}.defaults(to: {fallback})"),
            ExprVariant::Shift(base, offset) => write!(f, "{base}.offset(by: {offset})"),
            ExprVariant::Binary(op, lhs, rhs) => write!(f, "{lhs} {op} {rhs}"),
            ExprVariant::Unary(op, operand) => write!(f, "{op}{operand}"),
            ExprVariant::Ite(cond, then, els) => write!(f, "if {cond} then {then} else {els}"),
            ExprVariant::Bracket(inner) => write!(f, "({inner})"),
            ExprVariant::MissingExpr => Ok(()),
            ExprVariant::Tuple(elems) => join_list(f, elems, "(", ")", ", "),
            ExprVariant::Field(base, field) => write!(f, "{base}.{field}"),
            ExprVariant::Function(label, tys, args) => {
                write!(f, "{}", label.name)?;
                if !tys.is_empty() {
                    join_list(f, tys, "<", ">", ", ")?;
                }
                let formatted: Vec<String> = args
                    .iter()
                    .zip(&label.arg_names)
                    .map(|(arg, name)| match name {
                        None => format!("{arg}"),
                        Some(n) => format!("{n}: {arg}"),
                    })
                    .collect();
                join_list(f, &formatted, "(", ")", ", ")
            }
            ExprVariant::Method(base, label, tys, args) => {
                write!(f, "{}.{}", base, label.name)?;
                if !tys.is_empty() {
                    join_list(f, tys, "<", ">", ", ")?;
                }
                let formatted: Vec<String> = args
                    .iter()
                    .zip(&label.arg_names)
                    .map(|(arg, name)| match name {
                        None => format!("{arg}"),
                        Some(n) => format!("{n}: {arg}"),
                    })
                    .collect();
                join_list(f, &formatted, "(", ")", ", ")
            }
            ExprVariant::Quantified(q, ids1, ids2, body) => {
                let keyword = match q {
                    Quantifier::Forall => "forall",
                    Quantifier::Exists => "exists",
                };
                let part1: Vec<String> = ids1.iter().map(|i| i.to_string()).collect();
                let part2: Vec<String> = ids2.iter().map(|i| i.to_string()).collect();
                write!(
                    f,
                    "{keyword} [{}, {}]: {body}",
                    part1.join(","),
                    part2.join(",")
                )
            }
        }
    }
}
